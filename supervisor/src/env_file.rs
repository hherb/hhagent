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
/// Two rules, both fail-open without the check:
///
///   - **Absolute.** systemd requires it and drops the directive (with a
///     journal warning) otherwise, leaving the service running with **no**
///     environment at all — invisible unless someone reads the journal.
///   - **No control characters.** `systemd_user::builder` emits this directive
///     bare, because a quoted path is one systemd *drops* (#530, measured), so
///     a newline in the path would terminate the directive and turn whatever
///     follows into another one: `/tmp/x\nExecStartPre=/evil` injects an
///     `ExecStartPre`. Quoting used to make that case fail safe as a side
///     effect of breaking the legitimate cases; with quoting gone this check
///     is what carries the guarantee, and it must run before any rendering.
///     `SystemdUser::install` also screens its other path fields for control
///     characters — this is the same rule, owned here so the env-file list
///     carries it on **both** backends rather than only on Linux.
///
/// A space is deliberately *not* rejected: `$HOME` may contain one, systemd
/// accepts it in a bare path, and launchd handles it natively. Refusing it
/// would fail an install that works.
pub fn validate_env_file_path(path: &Path) -> Result<(), SupervisorError> {
    if !path.is_absolute() {
        return Err(SupervisorError::Io(format!(
            "environment_file must be absolute, got {}",
            path.display()
        )));
    }
    if path.to_string_lossy().contains(|c: char| c.is_control()) {
        return Err(SupervisorError::Io(format!(
            "environment_file must not contain control characters, got {path:?}"
        )));
    }
    Ok(())
}

/// What an inspection found at the operator overlay's path.
///
/// `Absent` and `Unreadable` are deliberately separate variants
/// ([#531](https://github.com/hherb/kastellan/issues/531)). Absent is the
/// *normal* state — most hosts declare no overlay — while unreadable means the
/// operator wrote a file and believes it applies. Collapsing them is the same
/// mistake [`fold_env_files`] refuses to make.
#[derive(Debug, Eq, PartialEq)]
pub enum OverlayState {
    /// No file at the path. Normal, and not an error.
    Absent,
    /// Read and parsed; carries how many `KEY=value` pairs it declares.
    Present { keys: usize },
    /// Exists but could not be read or decoded. Carries the OS reason.
    Unreadable { reason: String },
}

/// Resolve parsed pairs to the keys the file actually **declares**, with a
/// repeated key collapsed onto its last value. Pure.
///
/// One definition, because every count and every check downstream has to agree
/// on what "5 keys" means. Counting raw pairs instead would let an overlay with
/// one appended correction report `1 of 6 keys did not reach this process`
/// while declaring five — the numerator deduped, the denominator not.
pub fn declared_keys(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut resolved: Vec<(String, String)> = Vec::new();
    merge_env(&mut resolved, pairs.to_vec());
    resolved
}

/// Read the operator overlay at `path` and classify it. Does I/O; the
/// rendering half is pure, so the wording is testable without a filesystem.
pub fn inspect_overlay(path: &Path) -> OverlayState {
    match std::fs::read_to_string(path) {
        Ok(c) => OverlayState::Present { keys: declared_keys(&parse_env_file(&c)).len() },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OverlayState::Absent,
        Err(e) => OverlayState::Unreadable { reason: e.to_string() },
    }
}

/// One install-transcript line saying what was found at the overlay path.
///
/// Pure. **Names keys and counts, never values** — the install transcript is
/// plaintext, and the overlay is where endpoints and token-file paths live.
///
/// Always names the path, including in the `Absent` case, because that is the
/// diagnostic: an operator whose heredoc landed in `~/.config/kastellan.env.local`
/// (missing directory component) can only see the mistake if the tool says
/// where it actually looked.
pub fn render_overlay_found(path: &Path, state: &OverlayState) -> String {
    let p = path.display();
    match state {
        OverlayState::Present { keys } => {
            format!("operator overlay: {p} ({keys} keys) — applied after kastellan.env, so these values win")
        }
        OverlayState::Absent => {
            format!("operator overlay: none at {p} — tuned settings put there survive a reinstall; see docs/deploy/operator-env.md")
        }
        OverlayState::Unreadable { reason } => {
            format!("operator overlay: {p} UNREADABLE ({reason}) — its values will NOT reach the daemon")
        }
    }
}

/// Overlay keys whose declared value is **not** what `live` reports.
///
/// Pure: `live` is the environment lookup, so the whole check is testable
/// without touching the process environment. The daemon passes
/// `|k| std::env::var(k).ok()`.
///
/// This is the end-to-end confirmation #531 asks for, and it is a stronger
/// claim than "the file exists": it compares what the operator wrote against
/// what the process actually inherited, which is the only thing that matters.
/// It catches a dropped `EnvironmentFile=` directive (#530's fail-open), a
/// mis-pathed overlay, and — because it compares *values*, not just presence —
/// an overlay listed BEFORE the generated file, where `kastellan.env` wins and
/// the operator's tuning is silently overridden, which is #458 itself.
///
/// A key repeated within the file is judged on its **last** value and named at
/// most once, matching [`merge_env`] and systemd: appending a correction rather
/// than editing in place is how an overlay naturally accumulates, and judging
/// the superseded value would report a key as unapplied exactly when the
/// correction did apply.
pub fn unapplied_keys(
    keys: &[(String, String)],
    live: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut resolved: Vec<(String, String)> = Vec::new();
    merge_env(&mut resolved, keys.to_vec());
    resolved
        .into_iter()
        .filter(|(k, v)| live(k).as_ref() != Some(v))
        .map(|(k, _)| k)
        .collect()
}

/// One daemon-startup line saying whether the overlay actually took effect.
///
/// Pure, and **names keys, never values** — same rule as
/// [`render_overlay_found`], and it bites harder here because the daemon log is
/// a plaintext file with none of `audit_log`'s role gating.
///
/// The two outcomes are worded differently rather than parameterised so an
/// operator skimming the log can tell them apart without reading the numbers.
pub fn render_overlay_applied(path: &Path, total: usize, unapplied: &[String]) -> String {
    let p = path.display();
    if unapplied.is_empty() {
        format!("operator overlay applied: {p} ({total} keys, all present in this process)")
    } else {
        format!(
            "operator overlay NOT fully applied: {p} — {} of {total} keys did not reach this process: {}",
            unapplied.len(),
            unapplied.join(", ")
        )
    }
}

#[cfg(test)]
mod tests;
