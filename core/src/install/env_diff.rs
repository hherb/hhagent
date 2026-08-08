//! What an install is about to destroy, named before it destroys it.
//!
//! `kastellan-cli install` regenerates `kastellan.env` from CLI flags, so any
//! hand-added key is dropped and any hand-tuned value reverts to the flag
//! default. On 2026-08-08 that silently removed the deployed agent's mail
//! capability for two days: with `KASTELLAN_MAIL_ENDPOINT` gone the `mail.*`
//! tools never registered, the planner fell back to filesystem probing, and the
//! only symptom was a wrong answer. See [#458].
//!
//! This module is the pure half of the fix: compare the file about to be
//! overwritten against the freshly rendered content and report the difference by
//! **key name only**. Values stay out of the install transcript — the operator
//! reads them from the `.bak` copy the caller writes — because an env file may
//! one day hold something that should not be echoed to a terminal.
//!
//! [#458]: https://github.com/hherb/kastellan/issues/458

use kastellan_supervisor::env_file::parse_env_file;

/// Keys an install would drop or change, in the old file's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvDiff {
    /// Present in the old file, absent from the new one.
    pub lost: Vec<String>,
    /// Present in both with a different value.
    pub changed: Vec<String>,
}

impl EnvDiff {
    /// True when the install destroys nothing — the common case, and the
    /// condition under which the caller writes no backup and prints nothing.
    pub fn is_empty(&self) -> bool {
        self.lost.is_empty() && self.changed.is_empty()
    }
}

/// Diff two `EnvironmentFile` buffers by key.
///
/// Only uncommented `KEY=value` lines count, via the shared
/// [`kastellan_supervisor::env_file::parse_env_file`] grammar — so the commented
/// defaults `render_env_file` emits are not mistaken for keys, and a key the
/// operator *uncommented* is correctly reported as lost.
///
/// Keys present only in `new` are not reported: that is the installer adding
/// something, not destroying it. Output follows `old`'s line order so the
/// operator-facing message is deterministic, and each key is reported at most
/// once even if the source file repeats it.
///
/// A key's operative value is its **last** occurrence in the file, matching
/// systemd's precedence and the behaviour of
/// [`kastellan_supervisor::env_file::merge_env`].
///
/// Values are compared *after* that grammar has normalised them, so an operator
/// who quoted a value the installer renders bare is not reported as a spurious
/// `changed:`.
pub fn diff_env_files(old: &str, new: &str) -> EnvDiff {
    use std::collections::{HashMap, HashSet};

    // `collect` keeps the LAST insert for a repeated key, on both sides —
    // systemd's precedence. Comparing the first value instead would report
    // `old = "A=1\nA=2"` against `new = "A=1"` as no change at all, which is a
    // genuine revert going unreported: the exact silence this module exists to
    // break.
    let new_values: HashMap<String, String> = parse_env_file(new).into_iter().collect();
    // `old` stays a Vec as well: the report follows its line order.
    let old_pairs = parse_env_file(old);
    let old_values: HashMap<&str, &str> =
        old_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let mut diff = EnvDiff::default();
    let mut seen: HashSet<&str> = HashSet::new();
    for (key, _) in &old_pairs {
        if !seen.insert(key.as_str()) {
            continue; // already reported at its first appearance
        }
        match new_values.get(key.as_str()) {
            None => diff.lost.push(key.clone()),
            Some(new_value) if new_value.as_str() != old_values[key.as_str()] => {
                diff.changed.push(key.clone())
            }
            Some(_) => {}
        }
    }
    diff
}

#[cfg(test)]
mod tests;
