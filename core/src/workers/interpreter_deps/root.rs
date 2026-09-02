//! Where a venv's interpreter really lives.
//!
//! Split out of [`interpreter_deps`](super) as its own module: the dep-graph
//! walk next door answers *what does the interpreter link*, while this answers
//! the prior question — *which interpreter, and where*. Pure; the filesystem
//! probes are injected.

use std::path::{Path, PathBuf};

/// Resolve the real interpreter prefix a venv's `python3` symlinks to.
///
/// Locates `<venv>/bin/{python3,python}`, canonicalizes it to the real CPython,
/// and returns its **prefix** (`<bin>/..`) — the tree holding the interpreter
/// binary + `libpython` + the stdlib. Returns `None` when the interpreter can't
/// be found/canonicalized, or when it already lives **under** `venv_dir`
/// (self-contained — the venv `fs_read` already covers it, nothing extra to
/// bind). Pure: `exists` and `canonicalize` are injected.
///
/// Shared by every venv-backed worker (browser-driver, gliner-relex) so the
/// "where's the real interpreter" rule lives in exactly one place.
pub fn resolve_interpreter_root(
    venv_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let bin = venv_dir.join("bin");
    let candidate = ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| exists(p))?;
    let real = canonicalize(&candidate)?;
    let prefix = real.parent()?.parent()?; // <prefix>/bin/python → <prefix>
    // Self-contained: the real interpreter is already under venv_dir, so the
    // venv fs_read covers it — nothing extra to bind.
    if prefix.starts_with(venv_dir) {
        return None;
    }
    Some(prefix.to_path_buf())
}

#[cfg(test)]
mod tests;
