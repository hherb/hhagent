//! Path facts that only hold **before** canonicalization.
//!
//! Everything else in [`interpreter_deps`](super) works with canonical paths,
//! because a dependency graph is only comparable once every node is the one
//! real path. This module is the deliberate exception: a jail has to be able to
//! resolve the path a *shebang* names, and a shebang names whatever the venv
//! was built with — symlinks and all (issue #650).
//!
//! [`normalize_lexically`] and [`symlink_chain`] are pure; `read_link` is
//! injected so the walk is unit-testable with no filesystem. The only impurity
//! is [`read_link_via_fs`].

use std::path::{Component, Path, PathBuf};

/// Maximum symlink hops followed before giving up: the looser of the two host
/// kernels' own limits, so the walk never stops short of a path `execve` would
/// still resolve. Linux's is `MAXSYMLINKS` = 40 (`linux/namei.h`); macOS's is
/// 32 (`sys/param.h`). Note neither defines the POSIX `SYMLOOP_MAX` name —
/// `getconf SYMLOOP_MAX` reports `undefined` on the DGX and 32 on macOS — so
/// this is a termination bound, not a fidelity requirement.
///
/// A cycle is also detected directly (see [`symlink_chain`]); this bounds the
/// other shape — a pathological chain that never repeats a node — so the walk
/// always terminates.
pub(crate) const MAX_SYMLINK_HOPS: usize = 40;

// Pinned to the literal, outside `cfg(test)` so it holds in a release build.
// The cap test asserts `MAX_SYMLINK_HOPS + 1`, so it tracks whatever value sits
// here and cannot notice a silent shrink; this makes any change a deliberate
// two-line edit that shows up in review.
const _: () = assert!(MAX_SYMLINK_HOPS == 40);

/// Normalize `p` **lexically**: drop every *interior* `.` component and pop the
/// component before each `..`, touching no filesystem. (A *leading* `.` on a
/// relative path survives — see the note below.)
///
/// Used instead of `canonicalize` precisely because canonicalizing is what
/// loses the answer we are after — the path as it was *named*. The trade is
/// the usual one for lexical resolution: `a/link/..` normalizes to `a` even
/// when `link` points elsewhere. That is sound here because the only `..` we
/// ever see comes from a symlink target being resolved against its own
/// directory (a Homebrew `../Cellar/...`), which is exactly the relative-path
/// arithmetic `..` is written to express.
///
/// A `..` with nothing to pop is *kept* on a relative path (removing it would
/// silently change which directory the path names) and *dropped* at the root,
/// where POSIX defines `/..` as `/`.
///
/// Only `..` needs handling here, and the reason is narrower than it looks.
/// [`Path::components`] drops every **interior** `.` (`/a/./b` yields
/// `[RootDir, a, b]`), but it *keeps* a **leading** `.` on a relative path
/// (`./a/b` yields `[CurDir, a, b]`), which the `other` arm below then
/// preserves. So there is deliberately no `CurDir` arm — not because such a
/// component can never arrive, but because every production caller passes an
/// absolute path: [`symlink_chain`] starts at `<venv>/bin/python3` and resolves
/// each relative link target against its link's own absolute directory.
///
/// Do not read this as "no `.` survives": `normalize_lexically("./a")` is
/// `./a`. If a relative path ever reaches here, decide deliberately whether to
/// strip the leading `.` — the `normalize_*` tests pin the interior case only.
pub(crate) fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/` — never escape above the root.
                Some(Component::RootDir) => {}
                // Nothing to pop (empty, or a run of leading `..`): keep it.
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Every path the filesystem *names* on the way from `start` to the real file,
/// in order, with `start` first and no canonicalization.
///
/// A relative link target is resolved against the directory of the link that
/// held it, then normalized lexically — so the result is always the absolute
/// path a `#!` line, or a `execve`, would actually look up.
///
/// Terminates at a path that is not a symlink (the common case), at a repeated
/// path (a cycle), after [`MAX_SYMLINK_HOPS`], or on a relative target whose
/// path has no parent to resolve against. The chain never contains a duplicate
/// — the cycle check runs before the push — because callers derive bind paths
/// from it.
///
/// Note what this does *not* see: a symlinked **directory component** in the
/// middle of a path (`…/cpython-3.13-linux/bin/python3.13`, where only
/// `cpython-3.13-linux` is a link) is not a symlink at the final path, so the
/// chain stops there. That is the point — that last named path is the one the
/// jail must be able to resolve, and `canonicalize` is what would replace it
/// with the `.14` directory nobody named.
pub(crate) fn symlink_chain(
    start: &Path,
    read_link: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut current = normalize_lexically(start);
    let mut chain = vec![current.clone()];
    for _ in 0..MAX_SYMLINK_HOPS {
        let Some(target) = read_link(&current) else {
            break;
        };
        let resolved = if target.is_absolute() {
            normalize_lexically(&target)
        } else {
            match current.parent() {
                Some(dir) => normalize_lexically(&dir.join(&target)),
                // A relative target on a path with no parent has nothing to
                // resolve against; stop rather than guess.
                None => break,
            }
        };
        if chain.contains(&resolved) {
            break; // cycle
        }
        chain.push(resolved.clone());
        current = resolved;
    }
    chain
}

/// Read one symlink from the real filesystem. `None` when `p` is not a symlink
/// or cannot be read — the same fail-safe shape as
/// [`resolve_deps_via_tool`](super::resolve_deps_via_tool): the caller then
/// binds nothing extra, so the failure direction is always a *narrower* jail,
/// never a wider one.
///
/// Be clear about how that failure surfaces, because it is the whole of #650:
/// a worker that needed the bind dies at `execve` with ENOENT, which reaches
/// the core as a **contentless** `Protocol(EarlyExit)` (#613) — nothing drains
/// a one-shot worker's piped stderr above `debug!`. Quiet, but never wider.
/// Distinguishing the errno here (`InvalidInput` = "not a symlink", the normal
/// case, versus a real I/O error worth warning about) is deferred to #657.
///
/// Injected as a function rather than added to `ResolveCtx` — which already
/// carries the `exists`/`canonicalize` impurities, so "it is an impurity" is
/// not the distinction. The reason is cost: `ResolveCtx` is constructed at 29
/// sites for every manifest in the tree, and this probe serves two workers.
/// See #658 for the case that it should move there anyway — it is also half of
/// why `canonicalize` and `read_link`, being the same type and adjacent, can be
/// transposed silently.
pub fn read_link_via_fs(p: &Path) -> Option<PathBuf> {
    std::fs::read_link(p).ok()
}

#[cfg(test)]
mod tests;
