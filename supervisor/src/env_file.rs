//! The `EnvironmentFile=` grammar, in one place.
//!
//! Deliberately `cfg`-free and shared rather than per-backend. The launchd
//! backend folds these pairs into a plist (launchd has no `EnvironmentFile=`
//! directive) and `kastellan-core`'s installer uses the same parser to diff the
//! env file it is about to overwrite. A second parser for one file format is
//! the drift shape #479 and #520 each cost a review round; and shared code is
//! compiled and tested on **both** hosts, while per-backend code is invisible
//! to CI (there is no macOS job at all) — the same reasoning that folded the
//! two backends' staging helpers into one `atomic_write` in #511.
//!
//! **Why this module owns file I/O too.** [`fold_env_files`] resolves an
//! ordered [`EnvFileRef`] list into a flat env map. It used to live inside the
//! macOS-only launchd backend, which meant the *ordering* and *optionality*
//! semantics of #458 — half of a cross-platform guarantee — compiled and ran
//! only when somebody happened to run `cargo test` on a Mac. Same argument as
//! the parser above; the I/O is incidental to it.

use std::path::Path;

use crate::{EnvFileRef, SupervisorError};

/// Parse an `EnvironmentFile`-style buffer into ordered `(KEY, value)` pairs.
///
/// Pure (no I/O). Implements the subset of systemd's `EnvironmentFile=` grammar
/// that operators actually write, **measured against a live systemd user manager
/// on 2026-08-09** rather than recalled:
///
/// | line | systemd | here |
/// | --- | --- | --- |
/// | `A="a b"` | `a b` | `a b` |
/// | `A='a b'` | `a b` | `a b` |
/// | `A=  c  ` | `c` | `c` |
/// | `A='  x  '` | `  x  ` | `  x  ` |
/// | `A=f"g` | `f"g` | `f"g` |
/// | `A="a` | `a` | `a` |
/// | `A="a'` | `a'` | `a'` |
/// | `A=""` | *empty* | *empty* |
/// | `export A=h` | dropped | dropped |
/// | `;A=i` | dropped | dropped |
/// | `#A=i` | dropped | dropped |
///
/// Quote-stripping and value-trimming are **not** cosmetic. Before #528 this
/// took values verbatim, justified by "the installer writes plain values" — a
/// premise #458 retired, because `kastellan.env.local` is the first env file a
/// *human* writes by hand, and humans quote. On Linux systemd did the stripping
/// and here nothing did, so one overlay file produced two different runtime
/// environments on the two first-class platforms.
///
/// Not implemented, because the installer never emits them and systemd's own
/// handling is more elaborate than an operator overlay warrants: backslash line
/// continuations, and C-style escapes inside double quotes. A value needing
/// either is out of contract on both platforms.
pub fn parse_env_file(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        // `;` is a comment introducer to systemd just as `#` is; treating it as
        // a key named `;FOO` invented a variable the Linux side never sees.
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        // systemd ignores an assignment whose name is not a bare identifier —
        // notably `export FOO=bar`, which shell users write by reflex. Silently
        // accepting it here created an env var literally named "export FOO".
        if k.is_empty() || k.contains(char::is_whitespace) {
            continue;
        }
        out.push((k.to_string(), unquote(v.trim())));
    }
    out
}

/// Strip systemd's quoting from a value.
///
/// A quote only matters as the value's **first** character: systemd enters a
/// quoted state there and leaves it at the matching close, or at end-of-line if
/// there is none. So the leading quote is always removed, a matching trailing
/// one is removed with it, and a quote anywhere else is literal.
///
/// Measured, not inferred — the "matched pair only" rule this replaced was the
/// obvious reading and was wrong for exactly the operator-typo cases: systemd
/// turns `A="a` into `a` and `A="a'` into `a'`, where a pair-only rule keeps
/// both quotes.
fn unquote(v: &str) -> String {
    let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return v.to_string();
    };
    let rest = &v[q.len_utf8()..];
    match rest.strip_suffix(q) {
        Some(inner) => inner.to_string(),
        None => rest.to_string(),
    }
}

/// Merge `from` into `into`, with `from` winning on key collision (matching
/// systemd's `EnvironmentFile=`-after-`Environment=` override order, and the
/// later-file-wins order between two `EnvironmentFile=` directives — both
/// measured on a live systemd user manager, not assumed). Existing keys keep
/// their position with the value replaced; new keys are appended.
///
/// A key repeated *within* one batch resolves to its **last** occurrence, for
/// the same reason: the first push creates the slot, each later one overwrites
/// it in place.
pub fn merge_env(into: &mut Vec<(String, String)>, from: Vec<(String, String)>) {
    for (k, v) in from {
        if let Some(slot) = into.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            into.push((k, v));
        }
    }
}

/// Resolve an ordered [`EnvFileRef`] list over `into`, later files winning.
///
/// The backend-neutral half of #458: read each file in declared order and merge
/// its pairs, so an operator's `kastellan.env.local` overrides the
/// `kastellan.env` the installer regenerates. An absent **optional** file is
/// skipped — that is the normal state of the overlay — while an absent
/// **required** one is an error, and a file that exists but cannot be read is
/// an error either way. `NotFound` is the only forgiven kind: silently treating
/// an unreadable overlay as an empty one would be #458 wearing a new hat, since
/// the operator wrote the file and believes it applies.
///
/// Used by the launchd backend, which has no `EnvironmentFile=` directive and
/// must bake the values into the plist at install time. systemd does this
/// resolution itself at service start, from the directives
/// `systemd_user::builder` renders — so this function is macOS's route to the
/// same guarantee, and lives here so its semantics are tested on both hosts.
pub fn fold_env_files(
    into: &mut Vec<(String, String)>,
    files: &[EnvFileRef],
) -> Result<(), SupervisorError> {
    for ef in files {
        let contents = match std::fs::read_to_string(&ef.path) {
            Ok(c) => c,
            Err(e) if ef.optional && e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(SupervisorError::Io(format!(
                    "read environment_file {}: {e}",
                    ef.path.display()
                )))
            }
        };
        merge_env(into, parse_env_file(&contents));
    }
    Ok(())
}

/// Reject an `EnvironmentFile=` path systemd would refuse or misread.
///
/// systemd requires an absolute path and drops the directive (with a journal
/// warning) otherwise, leaving the service running with **no** environment at
/// all — fail-open, and invisible unless someone reads the journal. The sibling
/// path fields already get this check at both backends' `install`; the env-file
/// list was added to the control-character guard only.
pub fn validate_env_file_path(path: &Path) -> Result<(), SupervisorError> {
    if !path.is_absolute() {
        return Err(SupervisorError::Io(format!(
            "environment_file must be absolute, got {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
