//! One answer to "where are the gliner-relex weights on this host".
//!
//! Three integration tests need the `multi-v1.0` snapshot, and each carried
//! its own copy of the lookup. The copies had drifted: two honoured
//! `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` and one ignored it entirely, so a run
//! with that override set could exercise one snapshot in one suite and a
//! different one in another, silently, in the same `cargo test`.
//!
//! The rules are mirrored on the Python side by
//! `workers/gliner-relex/tests/live_support.py::weights_dir_candidate`, which
//! gates the same model behind the same three variables. Change one, change
//! both — a gate whose two halves disagree about where the weights live
//! produces a skip on one side and a run on the other, which is exactly the
//! false green this scaffolding exists to prevent.

use std::path::PathBuf;

/// Where `scripts/workers/gliner-relex/install.sh` puts the snapshot,
/// relative to the kastellan data dir. Kept as a literal (not assembled)
/// because both languages and the install script must agree on it.
pub const WEIGHTS_SUBPATH: &str = "workers/gliner-relex/weights/multi-v1.0";

/// Where the weights *should* be on this host, without looking on disk.
///
/// Three sources, most specific first:
///
/// 1. `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` — taken **verbatim**. This is the
///    daemon-style override, and it already names the snapshot itself, so
///    [`WEIGHTS_SUBPATH`] is *not* appended.
/// 2. `KASTELLAN_DATA_DIR` — the data root; the snapshot sits at
///    `<data dir>/WEIGHTS_SUBPATH`.
/// 3. `HOME` — the default data root is `$HOME/.local/share/kastellan`.
///
/// A variable that is **set but empty** counts as unset. `std::env::var`
/// returns `Ok("")` for those, and taking that branch would produce a
/// *relative* weights path resolved against whatever the current working
/// directory happens to be — a skip whose reason names a location nobody
/// ever installed to. Python's truthiness check treats empty as unset for
/// the same reason; this is the half of the mirror that used not to.
///
/// Returns `None` when none of the three is usable, rather than a relative
/// path. Pure: the environment is injected, and nothing here touches disk,
/// so the rules are unit-testable on a host with no weights at all.
pub fn weights_dir_candidate(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |k: &str| lookup(k).filter(|v| !v.is_empty());

    if let Some(explicit) = non_empty("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(data_dir) = non_empty("KASTELLAN_DATA_DIR") {
        return Some(PathBuf::from(data_dir).join(WEIGHTS_SUBPATH));
    }
    let home = non_empty("HOME")?;
    Some(PathBuf::from(home).join(".local/share/kastellan").join(WEIGHTS_SUBPATH))
}

/// Resolve the weights dir against the real environment and the real disk,
/// or print a `[SKIP]` line and return `None`.
///
/// The skip reason always names the path that was checked, so an operator can
/// tell "no weights staged" from "staged somewhere else" without re-deriving
/// the rules.
pub fn resolve_weights_dir_or_skip() -> Option<PathBuf> {
    let candidate = weights_dir_candidate(|k| std::env::var(k).ok());
    match candidate {
        Some(p) if p.is_dir() => Some(p),
        Some(p) => {
            eprintln!(
                "\n[SKIP] gliner-relex weights dir missing at {} — run scripts/workers/gliner-relex/install.sh\n",
                p.display()
            );
            None
        }
        None => {
            eprintln!(
                "\n[SKIP] gliner-relex weights dir unresolvable: none of \
                 KASTELLAN_GLINER_RELEX_WEIGHTS_DIR, KASTELLAN_DATA_DIR or HOME is set to a \
                 non-empty value — set one of them (install.sh cannot help until you do)\n"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a lookup over a fixed table, so no test touches the real env.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn explicit_override_is_used_verbatim() {
        // It IS the weights dir, not a base to append WEIGHTS_SUBPATH to.
        let env = env_of(&[("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/opt/weights/multi-v1.0")]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from("/opt/weights/multi-v1.0"))
        );
    }

    #[test]
    fn data_dir_gets_the_subpath_appended() {
        let env = env_of(&[("KASTELLAN_DATA_DIR", "/srv/kastellan")]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from("/srv/kastellan/workers/gliner-relex/weights/multi-v1.0"))
        );
    }

    #[test]
    fn home_is_the_last_resort() {
        let env = env_of(&[("HOME", "/home/agent")]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from(
                "/home/agent/.local/share/kastellan/workers/gliner-relex/weights/multi-v1.0"
            ))
        );
    }

    #[test]
    fn explicit_override_beats_data_dir() {
        let env = env_of(&[
            ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/opt/weights/multi-v1.0"),
            ("KASTELLAN_DATA_DIR", "/srv/kastellan"),
        ]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from("/opt/weights/multi-v1.0"))
        );
    }

    #[test]
    fn data_dir_beats_home() {
        let env = env_of(&[("KASTELLAN_DATA_DIR", "/srv/kastellan"), ("HOME", "/home/agent")]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from("/srv/kastellan/workers/gliner-relex/weights/multi-v1.0"))
        );
    }

    #[test]
    fn no_env_at_all_yields_none() {
        assert_eq!(weights_dir_candidate(env_of(&[])), None);
    }

    /// Set-but-empty is unset. `std::env::var` hands back `Ok("")`, and the
    /// old copies took that branch — producing a *relative* path resolved
    /// against the test binary's cwd.
    #[test]
    fn empty_explicit_override_falls_through_to_data_dir() {
        let env = env_of(&[
            ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", ""),
            ("KASTELLAN_DATA_DIR", "/srv/kastellan"),
        ]);
        assert_eq!(
            weights_dir_candidate(env),
            Some(PathBuf::from("/srv/kastellan/workers/gliner-relex/weights/multi-v1.0"))
        );
    }

    #[test]
    fn empty_data_dir_falls_through_to_home_not_a_relative_path() {
        let env = env_of(&[("KASTELLAN_DATA_DIR", ""), ("HOME", "/home/agent")]);
        let got = weights_dir_candidate(env).expect("HOME is set");
        assert!(got.is_absolute(), "resolved to a relative path: {}", got.display());
        assert_eq!(
            got,
            PathBuf::from(
                "/home/agent/.local/share/kastellan/workers/gliner-relex/weights/multi-v1.0"
            )
        );
    }

    #[test]
    fn all_empty_yields_none_rather_than_a_cwd_relative_path() {
        let env = env_of(&[
            ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", ""),
            ("KASTELLAN_DATA_DIR", ""),
            ("HOME", ""),
        ]);
        assert_eq!(weights_dir_candidate(env), None);
    }

    /// The subpath is a cross-language, cross-script constant: the Python
    /// `live_support.WEIGHTS_SUBPATH` and `install.sh` must agree with it.
    /// Asserting it against a literal is the only thing that catches a typo —
    /// every other test here builds its expectation from the constant.
    #[test]
    fn weights_subpath_is_the_literal_install_sh_writes() {
        assert_eq!(WEIGHTS_SUBPATH, "workers/gliner-relex/weights/multi-v1.0");
    }
}
