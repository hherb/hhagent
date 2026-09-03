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

/// Refuse an interpreter root that would expose the daemon's own private
/// state to a worker (security audit 2026-09-02, sandbox S6).
///
/// [`resolve_interpreter_root`] derives `<prefix>` from `<prefix>/bin/python`;
/// every path in [`bind_paths`](InterpreterRoot::bind_paths) then rides
/// `fs_read` into the jail (bwrap `--ro-bind`, Seatbelt `file-read*`). A
/// uv-managed interpreter gives a leaf like
/// `~/.local/share/uv/python/cpython-3.12-…` — fine. But an interpreter
/// installed with `--prefix=$HOME` (`~/bin/python3`), or living in
/// `~/.local/bin`, derives `$HOME` or `~/.local` — which contains
/// `~/.config/kastellan` (env files, tokens), `~/.local/share/kastellan` (the
/// Matrix store), `~/.local/state/kastellan` (the audit mirror) and the vault's
/// keyring material. Returns `None` (the worker then fails to start with an
/// ENOENT it can explain) rather than binding any of that.
///
/// The check runs over **every** bind path, not the canonical prefix alone:
/// since #650 a root also carries the aliases the venv names it by, and each
/// one becomes a real directory inside the jail. An alias is a *lexical* name
/// the canonical comparison cannot see — a `$HOME` that is itself a symlink
/// into the prefix would pass `alias_prefixes` and the canonical check both,
/// and only the alias check catches it. One offending name refuses the whole
/// root: dropping just that alias would leave the shebang dangling with the
/// very ENOENT #650 fixed, only quieter.
///
/// A separate step rather than part of [`resolve_interpreter_root`] so the
/// resolver stays a pure function of its injected probes; `home` is the one
/// environmental input, and the callers pass `$HOME`.
pub fn guard_interpreter_root(
    root: Option<InterpreterRoot>,
    home: Option<&Path>,
) -> Option<InterpreterRoot> {
    let root = root?;
    let Some(home) = home else { return Some(root) };
    let sensitive: [PathBuf; 6] = [
        home.to_path_buf(),
        home.join(".config").join("kastellan"),
        home.join(".local").join("share").join("kastellan"),
        home.join(".local").join("state").join("kastellan"),
        home.join(".local").join("lib").join("kastellan"),
        home.join(".kastellan"),
    ];
    let offending = root
        .bind_paths()
        .into_iter()
        .find(|prefix| sensitive.iter().any(|s| s.starts_with(prefix)));
    if let Some(prefix) = offending {
        tracing::warn!(
            prefix = %prefix.display(),
            "refusing interpreter prefix: it contains the daemon's own config/state; \
             install the worker's interpreter under a leaf prefix (a uv-managed or \
             venv-local CPython)"
        );
        return None;
    }
    Some(root)
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
