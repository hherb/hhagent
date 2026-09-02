//! Interpreter binds for venv-backed worker fixtures (issue #284).
//!
//! An integration test that hand-builds a `GlinerRelexEnv` has to answer the
//! same question the production manifest answers: *where is the real CPython
//! this venv points at, and what does it link?* Three fixtures used to answer
//! it by hardcoding `interpreter_root: None` under the comment "self-contained
//! fixture" — an assumption that is false whenever `uv` provisions its own
//! interpreter, and one that fails as a contentless `Protocol(EarlyExit)`
//! rather than as anything nameable.
//!
//! [`venv_interpreter_binds`] is the single answer for all of them. It calls
//! the production resolver, and — this is the part the fixtures cannot get
//! from the resolver alone — it refuses to accept `None` on a venv whose
//! interpreter does not actually resolve.

use std::path::{Path, PathBuf};

use kastellan_core::workers::gliner_relex::resolve_host_interpreter_binds;
use kastellan_core::workers::interpreter_deps::resolve_deps_via_tool;

/// Resolve `(interpreter_root, interpreter_lib_dirs)` for a host-mode venv
/// worker, exactly as `GlinerRelexManifest::resolve` does.
///
/// # Why this is not just a call to the production resolver
///
/// `resolve_host_interpreter_binds` returns `(None, vec![])` for **three**
/// different states, and a fixture must not treat them alike:
///
/// 1. **Self-contained venv** — the interpreter already lives under
///    `venv_dir`, so the venv's own `fs_read` covers it. Nothing to bind;
///    `None` is the correct answer.
/// 2. **The interpreter does not resolve** — `<venv>/bin/python3` is absent,
///    or is a symlink into a prefix that no longer exists. `Path::exists`
///    follows symlinks, so a dangling one reports `false` and the resolver
///    fails safe to `None`.
/// 3. **The dep tool is unavailable** — `ldd`/`otool` missing or failing.
///    Production backstops that with the manual `*_EXTRA_FS_READ` hatch; a
///    fixture has no such hatch.
///
/// State 2 is not hypothetical: it is precisely the state the DGX checkout
/// was in for months (a `.venv` copied from the Mac, whose `bin/python`
/// named a macOS path), and it is why four tests read as passing while
/// containing nothing. So `None` here is *checked*: if the venv's
/// interpreter does not canonicalize to something under `venv_dir`, this
/// panics naming the path and the remedy instead of letting the jail come up
/// with no interpreter bound.
///
/// Panics only on a broken fixture — never on a legitimately self-contained
/// venv, and never on a venv whose interpreter is external (that path returns
/// `Some` and is not checked here).
pub fn venv_interpreter_binds(venv_dir: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
    let (interpreter_root, interpreter_lib_dirs) = resolve_host_interpreter_binds(
        venv_dir,
        |p| p.exists(),
        |p| std::fs::canonicalize(p).ok(),
        resolve_deps_via_tool,
    );
    if interpreter_root.is_none() {
        assert_venv_is_self_contained(venv_dir);
    }
    (interpreter_root, interpreter_lib_dirs)
}

/// Prove a `None` interpreter root means "self-contained", not "broken".
///
/// Split out so the message names the specific failure. Both arms describe
/// the remedy, because the operator hitting this is usually staging a host
/// for the first time or has moved a `.venv` between machines.
fn assert_venv_is_self_contained(venv_dir: &Path) {
    let bin = venv_dir.join("bin");
    let candidate = ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| p.exists());
    let Some(candidate) = candidate else {
        panic!(
            "no resolvable interpreter under {} — either it is missing, or it is a \
             DANGLING symlink (Path::exists follows symlinks, so a broken link reads \
             as absent). This .venv was most likely staged on another host; re-run \
             scripts/workers/gliner-relex/install.sh on this one.",
            bin.display()
        );
    };
    let real = std::fs::canonicalize(&candidate).unwrap_or_else(|e| {
        panic!(
            "{} does not canonicalize ({e}) — the interpreter it names is gone. \
             Re-run scripts/workers/gliner-relex/install.sh.",
            candidate.display()
        )
    });
    assert!(
        real.starts_with(venv_dir),
        "{} resolves to {}, which is OUTSIDE {} — so the interpreter root should have \
         been Some(..) and the jail is about to come up with no bind for it. This is \
         the #284 failure mode; the worker would die as a contentless \
         Protocol(EarlyExit).",
        candidate.display(),
        real.display(),
        venv_dir.display()
    );
}
