use super::*;


/// `exists` over an explicit path list — anything absent does not exist.
fn exists_of(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
    move |p: &Path| paths.iter().any(|q| Path::new(q) == p)
}

/// `canonicalize` over an explicit table. A path absent from the table does
/// **not** canonicalize (`None`), which is how a broken link reads in
/// production — so a test never gets an accidental identity mapping.
fn canon_of(
    pairs: &'static [(&'static str, &'static str)],
) -> impl Fn(&Path) -> Option<PathBuf> {
    move |p: &Path| {
        pairs
            .iter()
            .find(|(k, _)| Path::new(k) == p)
            .map(|(_, v)| PathBuf::from(v))
    }
}

/// `read_link` over an explicit table. Absent ⇒ not a symlink.
fn links_of(
    pairs: &'static [(&'static str, &'static str)],
) -> impl Fn(&Path) -> Option<PathBuf> {
    move |p: &Path| {
        pairs
            .iter()
            .find(|(k, _)| Path::new(k) == p)
            .map(|(_, v)| PathBuf::from(v))
    }
}

/// Never a symlink — the shape of a venv whose `bin/python3` is a real file.
fn no_links(_p: &Path) -> Option<PathBuf> {
    None
}

#[test]
fn interpreter_root_none_for_self_contained_venv() {
    // python3 canonicalizes to a path *under* venv_dir ⇒ nothing extra to bind.
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[("/v/bin/python3", "/v/bin/python3.12")]);
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon, &no_links),
        None
    );
}

#[test]
fn interpreter_root_resolved_for_external_venv() {
    // Pyenv-style: venv python3 symlinks to an interpreter outside the venv,
    // named by the same path it canonicalizes to — so there is no alias.
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        ("/v/bin/python3", "/home/u/.pyenv/versions/3.12.3/bin/python3.12"),
        (
            "/home/u/.pyenv/versions/3.12.3",
            "/home/u/.pyenv/versions/3.12.3",
        ),
    ]);
    let links = links_of(&[(
        "/v/bin/python3",
        "/home/u/.pyenv/versions/3.12.3/bin/python3.12",
    )]);
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links),
        Some(InterpreterRoot::canonical_only(
            "/home/u/.pyenv/versions/3.12.3"
        ))
    );
}

#[test]
fn interpreter_root_none_when_no_python_in_venv() {
    let exists = |_p: &Path| false;
    let canon = |_p: &Path| None;
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon, &no_links),
        None
    );
}

/// The uv layout from issue #650, verbatim in shape: a patch-version directory
/// with a minor-version symlink alias beside it, and the venv naming the alias.
/// Binding only the canonical `.14` prefix leaves `.venv/bin/python` dangling
/// inside the jail, and `execve` returns ENOENT for a file that is present and
/// readable.
#[test]
fn interpreter_root_binds_the_uv_minor_version_alias_the_shebang_names() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        // The venv's python3 really is the .14 interpreter …
        (
            "/v/bin/python3",
            "/u/py/cpython-3.13.14-linux-aarch64-gnu/bin/python3.13",
        ),
        // … and the minor-version directory is a symlink to the .14 tree, so
        // binding it grants exactly the same bytes under a second name.
        (
            "/u/py/cpython-3.13-linux-aarch64-gnu",
            "/u/py/cpython-3.13.14-linux-aarch64-gnu",
        ),
    ]);
    let links = links_of(&[
        ("/v/bin/python3", "python"),
        (
            "/v/bin/python",
            "/u/py/cpython-3.13-linux-aarch64-gnu/bin/python3.13",
        ),
    ]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert_eq!(
        root.bind_paths(),
        vec![
            PathBuf::from("/u/py/cpython-3.13.14-linux-aarch64-gnu"),
            PathBuf::from("/u/py/cpython-3.13-linux-aarch64-gnu"),
        ]
    );
}

/// The dep walk compares canonical `ldd`/`otool` output against this prefix, so
/// it must stay canonical however many aliases were bound. This is the second
/// job the one return value has always had, and the one #650 must not break.
#[test]
fn dep_walk_prefix_is_the_canonical_root_not_an_alias() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        ("/v/bin/python3", "/u/py/cpython-3.13.14-linux/bin/python3.13"),
        ("/u/py/cpython-3.13-linux", "/u/py/cpython-3.13.14-linux"),
    ]);
    let links = links_of(&[(
        "/v/bin/python3",
        "/u/py/cpython-3.13-linux/bin/python3.13",
    )]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert_eq!(
        root.dep_walk_prefix(),
        Path::new("/u/py/cpython-3.13.14-linux")
    );
}

/// The non-widening rule. A named prefix is bound only when it canonicalizes to
/// the interpreter prefix — i.e. it is the *same tree* under another name, so
/// the bind grants no byte the canonical bind did not already grant. Homebrew
/// is the counter-example: `/opt/hb/bin/python3.12` names a prefix of `/opt/hb`,
/// which is its own directory holding far more than an interpreter.
#[test]
fn a_named_prefix_that_is_not_the_interpreter_tree_is_never_bound() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        (
            "/v/bin/python3",
            "/opt/hb/Cellar/py/3.12.7/bin/python3.12",
        ),
        // /opt/hb is a real directory, not a link to the Cellar prefix.
        ("/opt/hb", "/opt/hb"),
    ]);
    let links = links_of(&[("/v/bin/python3", "/opt/hb/bin/python3.12")]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert_eq!(
        root.bind_paths(),
        vec![PathBuf::from("/opt/hb/Cellar/py/3.12.7")],
        "binding /opt/hb would widen the jail's read grant to the whole Homebrew tree"
    );
}

/// Fail closed: if the named prefix does not canonicalize at all we cannot show
/// it is the same tree, so we do not bind it.
#[test]
fn a_named_prefix_that_does_not_canonicalize_is_never_bound() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[("/v/bin/python3", "/u/py/real/bin/python3.13")]);
    let links = links_of(&[("/v/bin/python3", "/u/py/named/bin/python3.13")]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert_eq!(root.bind_paths(), vec![PathBuf::from("/u/py/real")]);
}

/// Chain nodes inside the venv (`bin/python3` → `bin/python`) name the venv as
/// their prefix, and the venv is already in `fs_read`. It stays out for free —
/// `/v` canonicalizes to `/v`, not to the interpreter prefix — which is why
/// there is no `starts_with(venv_dir)` guard to test directly.
#[test]
fn a_named_prefix_inside_the_venv_is_never_bound() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        ("/v/bin/python3", "/u/py/real/bin/python3.13"),
        ("/v", "/v"),
        ("/u/py/named", "/u/py/real"),
    ]);
    let links = links_of(&[
        ("/v/bin/python3", "python"),
        ("/v/bin/python", "/u/py/named/bin/python3.13"),
    ]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert!(
        !root.bind_paths().contains(&PathBuf::from("/v")),
        "the venv is already bound; got {:?}",
        root.bind_paths()
    );
}

/// Two chain nodes can name the same prefix (`bin/python` and `bin/python3.13`
/// both live in `<alias>/bin`). One bind, not two — bwrap would accept the
/// duplicate, but a repeated `fs_read` entry makes the policy unreadable.
#[test]
fn a_prefix_named_twice_in_the_chain_is_bound_once() {
    let exists = exists_of(&["/v/bin/python3"]);
    let canon = canon_of(&[
        ("/v/bin/python3", "/u/py/real/bin/python3.13"),
        ("/u/py/named", "/u/py/real"),
    ]);
    let links = links_of(&[
        ("/v/bin/python3", "/u/py/named/bin/python3"),
        ("/u/py/named/bin/python3", "/u/py/named/bin/python3.13"),
    ]);
    let root = resolve_interpreter_root(Path::new("/v"), &exists, &canon, &links)
        .expect("an external interpreter resolves");
    assert_eq!(
        root.bind_paths(),
        vec![
            PathBuf::from("/u/py/real"),
            PathBuf::from("/u/py/named"),
        ]
    );
}

/// The accepting arm for [`InterpreterRoot::bind_paths`]: with no alias it is
/// the canonical root alone. Without this, an implementation that returned an
/// empty vec — binding nothing at all — passes every alias test above.
#[test]
fn bind_paths_of_a_canonical_only_root_is_the_canonical_root() {
    let root = InterpreterRoot::canonical_only("/u/py/real");
    assert_eq!(root.bind_paths(), vec![PathBuf::from("/u/py/real")]);
    assert_eq!(root.dep_walk_prefix(), Path::new("/u/py/real"));
}
