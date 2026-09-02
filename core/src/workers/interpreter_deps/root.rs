//! Where a venv's interpreter really lives.
//!
//! Split out of [`interpreter_deps`](super) as its own module: the dep-graph
//! walk next door answers *what does the interpreter link*, while this answers
//! the prior question — *which interpreter, and where*. Pure; the filesystem
//! probes are injected.

use std::path::{Path, PathBuf};

use super::named_path::symlink_chain;

/// The interpreter prefix a venv points at, in both of the forms a sandbox
/// needs — and it needs both, which is what issue #650 was.
///
/// One tree, two names:
///
/// * [`dep_walk_prefix`](Self::dep_walk_prefix) is the **canonical** path, and
///   is the right answer for comparing against `ldd`/`otool` output, which is
///   itself canonical. Get this wrong and the dep walk binds the interpreter's
///   own libraries as if they were out-of-prefix.
/// * [`bind_paths`](Self::bind_paths) is what goes into `fs_read`, and must
///   also carry every **alias** the venv names, because `execve` resolves a
///   shebang through the uncanonicalized path. Get this wrong and
///   `.venv/bin/python` dangles inside the jail and `execve` returns ENOENT for
///   a file that is present and readable.
///
/// Holding both in one value is deliberate: the two used to be a single
/// `Option<PathBuf>` serving both jobs, and the caller that needed the second
/// silently got the first.
///
/// **Backend asymmetry.** The aliases are load-bearing only under bwrap, where
/// a bind mount is what makes a path resolvable at all: `build_argv` emits
/// `--ro-bind-try <canonical-src> <alias-dest>`, so the alias becomes a real
/// directory inside the jail. macOS Seatbelt filters access instead of
/// remapping a namespace, and `canonicalize_policy_paths` canonicalizes every
/// `fs_read` entry before emitting rules — so on macOS an alias collapses back
/// into a duplicate of `canonical` and is inert. #650 was a Linux-only bug;
/// this is a Linux-only fix that both platforms compile and run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterRoot {
    /// The real prefix, symlinks resolved.
    canonical: PathBuf,
    /// Prefixes that name the very same tree, as the venv spells them. Each is
    /// proven to canonicalize to `canonical`, so binding one grants no byte the
    /// canonical bind did not already grant. Empty for the common case where
    /// the venv names the interpreter by its real path.
    ///
    /// That proof is taken **at resolve time** (daemon startup, once) while the
    /// backend re-resolves each `fs_read` entry **at every spawn**. An alias is
    /// by definition a symlink, so unlike the canonical prefix it is mutable
    /// state on the bind path: repointing it after startup would bind whatever
    /// it names then. #387's "TOCTOU-safe" note covers the check→bind window
    /// *inside* `spawn_under_policy`, not this resolve→spawn one. Writing to
    /// the interpreter directory already requires the agent's own OS user,
    /// which is the threat model's worst case, so this is a residual rather
    /// than a break — tracked in #659.
    aliases: Vec<PathBuf>,
}

impl InterpreterRoot {
    /// A root the venv names by its real path — no alias to bind.
    pub fn canonical_only(canonical: impl Into<PathBuf>) -> Self {
        Self {
            canonical: canonical.into(),
            aliases: Vec::new(),
        }
    }

    /// The prefix to treat as "already inside the jail" when walking the
    /// interpreter's dynamic dependencies. Canonical, because `ldd`/`otool`
    /// report canonical paths and the walk compares with `starts_with`.
    pub fn dep_walk_prefix(&self) -> &Path {
        &self.canonical
    }

    /// Every directory to bind read-only: the canonical prefix first, then each
    /// alias. All of them name the same bytes; the aliases exist so a shebang
    /// written against one still resolves.
    pub fn bind_paths(&self) -> Vec<PathBuf> {
        std::iter::once(self.canonical.clone())
            .chain(self.aliases.iter().cloned())
            .collect()
    }
}

/// Resolve the real interpreter prefix a venv's `python3` symlinks to, together
/// with the aliases the venv names it by.
///
/// Locates `<venv>/bin/{python3,python}`, canonicalizes it to the real CPython,
/// and returns its **prefix** (`<bin>/..`) — the tree holding the interpreter
/// binary + `libpython` + the stdlib. Returns `None` when the interpreter can't
/// be found/canonicalized, or when it already lives **under** `venv_dir`
/// (self-contained — the venv `fs_read` already covers it, nothing extra to
/// bind). Pure: `exists`, `canonicalize` and `read_link` are injected.
///
/// Shared by every venv-backed worker (browser-driver, gliner-relex) so the
/// "where's the real interpreter" rule lives in exactly one place.
pub fn resolve_interpreter_root(
    venv_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    read_link: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Option<InterpreterRoot> {
    let bin = venv_dir.join("bin");
    let candidate = ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| exists(p))?;
    let real = canonicalize(&candidate)?;
    let canonical = real.parent()?.parent()?; // <prefix>/bin/python → <prefix>
    // Self-contained: the real interpreter is already under venv_dir, so the
    // venv fs_read covers it — nothing extra to bind.
    if canonical.starts_with(venv_dir) {
        return None;
    }
    Some(InterpreterRoot {
        canonical: canonical.to_path_buf(),
        aliases: alias_prefixes(&candidate, canonical, canonicalize, read_link),
    })
}

/// Every prefix the venv *names* that is the canonical prefix under a different
/// name, in the order the symlink chain names them.
///
/// The admission rule is deliberately narrow: a named prefix is kept only when
/// it **canonicalizes to `canonical`** — i.e. it is the same tree spelled
/// differently, so binding it grants no byte the canonical bind did not already
/// grant. That is the whole of #650's uv case, where
/// `cpython-3.13-linux-aarch64-gnu` is a symlink to
/// `cpython-3.13.14-linux-aarch64-gnu`.
///
/// Two kinds of named prefix are therefore rejected:
///
/// * **A different tree** — Homebrew's `/opt/hb/bin/python3.12` names a prefix
///   of `/opt/hb`, which holds far more than an interpreter. Binding it would
///   *widen* the jail, and a containment fix must not do that. Such a venv is
///   no worse off than before this function learned about aliases.
/// * **One that does not canonicalize at all** — we cannot show it is the same
///   tree, so we fail closed and leave it out.
///
/// The venv's own prefix (`<venv>/bin/python3` → `<venv>`) needs no separate
/// guard and deliberately does not have one: it can only pass the rule above by
/// canonicalizing to the *external* interpreter prefix, which would mean the
/// venv and that prefix are the same tree — in which case binding it grants
/// nothing new, and `venv_dir` is already in `fs_read` regardless. An explicit
/// `starts_with(venv_dir)` check here would be redundant, not load-bearing.
/// (It would not be *untestable*: `canonicalize` is injected, so a fixture
/// mapping `/v` to the interpreter prefix does distinguish the two. Redundant
/// is the honest reason to leave it out.)
fn alias_prefixes(
    candidate: &Path,
    canonical: &Path,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    read_link: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for node in symlink_chain(candidate, read_link) {
        // <prefix>/bin/python → <prefix>, the same shape as the canonical root.
        let Some(prefix) = node.parent().and_then(|b| b.parent()) else {
            continue;
        };
        if prefix == canonical {
            continue;
        }
        if canonicalize(prefix).as_deref() != Some(canonical) {
            continue;
        }
        if !out.iter().any(|seen| seen == prefix) {
            out.push(prefix.to_path_buf());
        }
    }
    out
}

#[cfg(test)]
mod tests;
