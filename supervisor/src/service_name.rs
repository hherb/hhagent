//! The one service-name gate, shared by both backends (issue [#642]).
//!
//! A service name reaches the filesystem as a **basename** — a
//! `<name>.service` file under `~/.config/systemd/user/` on Linux, a
//! `<name>.plist` under `~/Library/LaunchAgents/` on macOS. Validating
//! it is therefore a path-traversal gate, not a style check, and
//! returning `Ok` is what lets either backend write a file to disk.
//!
//! # Why this lives at the crate root rather than in each backend
//!
//! It used to live in both, character-identical, as a private
//! `MAX_NAME_LEN` plus a `pub fn validate_service_name` behind each
//! backend's `#[cfg(target_os = …)]`. Two costs followed from that:
//!
//! * **Neither host ever ran the other's copy.** The two were only
//!   *believed* identical; nothing compared them, and the rule sets
//!   could have drifted apart silently. The tree's own contract is that
//!   one user-facing service name is portable to either OS without a
//!   "rename for macOS" step, and a per-platform gate cannot state that
//!   contract — it can only be it, on one platform at a time.
//! * **A cross-platform caller could not reach either.** That is what
//!   made `tests-common` hand-copy the cap as a bare `200` — a third,
//!   unlinked copy that checked only the length half and would have
//!   stopped being a guard in silence had a backend lowered its own.
//!
//! Being un-`cfg`'d, this module compiles and its tests run on **both**
//! hosts, which is the property the split versions could not have.
//!
//! Both backends re-export these two items, so
//! `systemd_user::validate_service_name` and
//! `launchd_agents::validate_service_name` keep the public paths their
//! callers already use.
//!
//! [#642]: https://github.com/hherb/kastellan/issues/642

use crate::SupervisorError;

/// Maximum length of a service name, in bytes.
///
/// Generous compared to the 255-byte basename ceiling both platforms
/// impose — the headroom is for the `.service` / `.plist` suffix and
/// any future namespacing prefix.
///
/// Deliberately the **same** number on both platforms even though their
/// underlying limits differ slightly, so that a name accepted on one
/// host is accepted on the other. See the module docs.
pub const MAX_NAME_LEN: usize = 200;

/// Validate a service name against `[A-Za-z0-9._-]{1,200}` minus `.`,
/// `..`, and any name starting with `.` (hidden files) or `-` (would be
/// parsed as a flag by some tools).
///
/// Rejects path-traversal characters (`/`, `\`, `\0`) and any byte the
/// systemd unit-name grammar would refuse. Returning `Ok` is the gate
/// that lets a backend's `install` write a file to disk.
///
/// # Rule order
///
/// Empty → over-length → `.`/`..` → leading `.`/`-` → charset, most
/// specific message first. Order is observable only for a name that
/// breaks two rules at once, and the choice there is cosmetic: which of
/// two true complaints an operator is shown.
///
/// One thing that is **not** cosmetic: the cap counts **bytes**, not
/// `char`s, because that is what the filesystem's basename limit counts.
/// A multi-byte name would therefore consume more of the cap than its
/// visible length suggests — but it cannot get that far, since the
/// charset rule admits ASCII only.
///
/// # Errors
///
/// [`SupervisorError::InvalidName`] naming the rule that refused,
/// which is the message an operator sees when `install` fails.
pub fn validate_service_name(name: &str) -> Result<(), SupervisorError> {
    if name.is_empty() {
        return Err(SupervisorError::InvalidName(
            "service name must not be empty".into(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SupervisorError::InvalidName(format!(
            "service name longer than {MAX_NAME_LEN} chars"
        )));
    }
    if name == "." || name == ".." {
        return Err(SupervisorError::InvalidName(
            ". and .. are not valid service names".into(),
        ));
    }
    if name.starts_with('.') {
        return Err(SupervisorError::InvalidName(
            "service name must not start with '.'".into(),
        ));
    }
    if name.starts_with('-') {
        return Err(SupervisorError::InvalidName(
            "service name must not start with '-'".into(),
        ));
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-') {
            return Err(SupervisorError::InvalidName(format!(
                "service name contains illegal character: {ch:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
