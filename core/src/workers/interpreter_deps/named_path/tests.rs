use super::*;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// normalize_lexically — `.` and `..` removed without touching the filesystem
// ---------------------------------------------------------------------------

/// Assert on the **literal** path text, not on `PathBuf` equality.
///
/// `Path`'s `PartialEq` compares `components()`, and `Components` normalizes
/// away **interior** `.` — so `PathBuf::from("/a/./b") == PathBuf::from("/a/b")`
/// is *true* and a component-wise assertion cannot see that stray `.` at all.
/// A **leading** `.` on a relative path is a different story: it survives
/// `components()`, so `PathBuf::from("./a") == PathBuf::from("a")` is *false*
/// and equality does see it. Asserting on the literal makes both cases visible.
/// The literal is what matters anyway: these paths are handed to bwrap as
/// `--ro-bind` arguments and printed in policies an operator reads.
fn assert_normalizes_to(input: &str, expected: &str) {
    let got = normalize_lexically(Path::new(input));
    assert_eq!(
        got.to_str().expect("test paths are valid UTF-8"),
        expected,
        "normalizing {input}"
    );
}

/// The output contract for the paths that actually reach here: no **interior**
/// `.` survives. Today that is `Path::components()` upstream of the loop rather
/// than a branch inside it — which is why there is no `CurDir` arm to mutate.
/// Kept so a future change of iteration source cannot start emitting `.` into a
/// bind path unnoticed.
#[test]
fn normalize_drops_interior_current_dir_components() {
    assert_normalizes_to("/a/./b/./c", "/a/b/c");
    // Also stripped straight after the root, where a reader might expect the
    // `RootDir` arm to have pushed something first.
    assert_normalizes_to("/./a", "/a");
}

/// The carve-out, pinned so nobody re-derives "no `.` ever survives" from the
/// test above. `Path::components()` KEEPS a leading `.` on a relative path, and
/// the `other` arm preserves it. Unreachable in production — every caller
/// passes an absolute path — but if that ever changes, this test is what makes
/// the decision to strip or keep it a deliberate one.
#[test]
fn normalize_keeps_a_leading_current_dir_on_a_relative_path() {
    assert_normalizes_to("./a/b", "./a/b");
}

#[test]
fn normalize_pops_the_component_before_a_parent_dir() {
    // The Homebrew shape: `/opt/homebrew/bin/../Cellar/x` → `/opt/homebrew/Cellar/x`.
    assert_normalizes_to(
        "/opt/homebrew/bin/../Cellar/python@3.12/bin/python3.12",
        "/opt/homebrew/Cellar/python@3.12/bin/python3.12",
    );
}

#[test]
fn normalize_swallows_a_parent_dir_at_the_root() {
    // POSIX: `/..` is `/`. Never escape above the root.
    assert_normalizes_to("/../../a", "/a");
}

#[test]
fn normalize_keeps_leading_parent_dirs_of_a_relative_path() {
    // Nothing to pop, and inventing one would silently change the path's
    // meaning. Callers join relative link targets onto an absolute dir before
    // normalizing, so this arm is defensive rather than load-bearing.
    assert_normalizes_to("../a/b", "../a/b");
}

#[test]
fn normalize_leaves_an_already_clean_path_alone() {
    // The accepting arm: a normalizer that mangled ordinary paths would break
    // every caller, and no `.`/`..` test would notice.
    assert_normalizes_to("/usr/local/bin/python3.13", "/usr/local/bin/python3.13");
}

// ---------------------------------------------------------------------------
// symlink_chain — every path the venv NAMES, in order, uncanonicalized
// ---------------------------------------------------------------------------

/// Build a fake `read_link` from an explicit link table. Anything absent is a
/// non-symlink, exactly as `std::fs::read_link` reports it.
fn links(entries: &[(&str, &str)]) -> impl Fn(&Path) -> Option<PathBuf> {
    let map: HashMap<PathBuf, PathBuf> = entries
        .iter()
        .map(|(k, v)| (PathBuf::from(k), PathBuf::from(v)))
        .collect();
    move |p: &Path| map.get(p).cloned()
}

#[test]
fn chain_of_a_plain_file_is_just_that_file() {
    let rl = links(&[]);
    assert_eq!(
        symlink_chain(Path::new("/v/bin/python3"), &rl),
        vec![PathBuf::from("/v/bin/python3")]
    );
}

#[test]
fn chain_resolves_a_relative_target_against_the_links_own_directory() {
    // `uv`'s venv shape: python3 → python (a bare name, same dir).
    let rl = links(&[("/v/bin/python3", "python")]);
    assert_eq!(
        symlink_chain(Path::new("/v/bin/python3"), &rl),
        vec![PathBuf::from("/v/bin/python3"), PathBuf::from("/v/bin/python")]
    );
}

#[test]
fn chain_follows_a_relative_target_through_a_parent_dir() {
    let rl = links(&[("/opt/hb/bin/python3.12", "../Cellar/py/3.12.7/bin/python3.12")]);
    assert_eq!(
        symlink_chain(Path::new("/opt/hb/bin/python3.12"), &rl),
        vec![
            PathBuf::from("/opt/hb/bin/python3.12"),
            PathBuf::from("/opt/hb/Cellar/py/3.12.7/bin/python3.12"),
        ]
    );
}

#[test]
fn chain_follows_the_full_uv_venv_shape_to_the_external_interpreter() {
    // The #650 chain verbatim, as `readlink` reports it on the DGX.
    let rl = links(&[
        ("/v/bin/python3", "python"),
        (
            "/v/bin/python",
            "/u/.local/share/uv/python/cpython-3.13-linux-aarch64-gnu/bin/python3.13",
        ),
    ]);
    assert_eq!(
        symlink_chain(Path::new("/v/bin/python3"), &rl),
        vec![
            PathBuf::from("/v/bin/python3"),
            PathBuf::from("/v/bin/python"),
            PathBuf::from("/u/.local/share/uv/python/cpython-3.13-linux-aarch64-gnu/bin/python3.13"),
        ]
    );
}

/// The fourth termination arm: a relative target on a path with no parent to
/// resolve it against. Only `/` and `""` reach it (`Path::new("x").parent()` is
/// `Some("")`, not `None`), so production never does — but the arm is reachable
/// through the injected `read_link`, so it gets a test rather than a claim that
/// no test could reach it.
#[test]
fn chain_stops_at_a_relative_target_with_no_parent_to_resolve_against() {
    let rl = links(&[("/", "x")]);
    assert_eq!(symlink_chain(Path::new("/"), &rl), vec![PathBuf::from("/")]);
}

#[test]
fn chain_stops_at_a_cycle_without_repeating_a_node() {
    // A self-referential pair must terminate, and must not list either node
    // twice — a caller derives bind paths from this and would emit duplicates.
    let rl = links(&[("/a", "/b"), ("/b", "/a")]);
    assert_eq!(
        symlink_chain(Path::new("/a"), &rl),
        vec![PathBuf::from("/a"), PathBuf::from("/b")]
    );
}

#[test]
fn chain_is_capped_at_the_symlink_hop_limit() {
    // A long non-repeating chain (so the cycle guard cannot be what stops it).
    let entries: Vec<(String, String)> = (0..200)
        .map(|i| (format!("/l/{i}"), format!("/l/{}", i + 1)))
        .collect();
    let map: HashMap<PathBuf, PathBuf> = entries
        .iter()
        .map(|(k, v)| (PathBuf::from(k), PathBuf::from(v)))
        .collect();
    let rl = move |p: &Path| map.get(p).cloned();
    let chain = symlink_chain(Path::new("/l/0"), &rl);
    // Start node + at most MAX_SYMLINK_HOPS hops.
    assert_eq!(chain.len(), MAX_SYMLINK_HOPS + 1);
    assert_eq!(chain[0], PathBuf::from("/l/0"));
}
