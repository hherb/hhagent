use super::*;

#[test]
fn interpreter_root_none_for_self_contained_venv() {
    // python3 canonicalizes to a path *under* venv_dir ⇒ nothing extra to bind.
    let exists = |p: &Path| p == Path::new("/v/bin/python3");
    let canon = |p: &Path| {
        (p == Path::new("/v/bin/python3")).then(|| PathBuf::from("/v/bin/python3.12"))
    };
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        None
    );
}

#[test]
fn interpreter_root_resolved_for_external_venv() {
    // Pyenv-style: venv python3 symlinks to an interpreter outside the venv.
    let exists = |p: &Path| p == Path::new("/v/bin/python3");
    let canon = |p: &Path| {
        (p == Path::new("/v/bin/python3"))
            .then(|| PathBuf::from("/home/u/.pyenv/versions/3.12.3/bin/python3.12"))
    };
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        Some(PathBuf::from("/home/u/.pyenv/versions/3.12.3"))
    );
}

#[test]
fn interpreter_root_none_when_no_python_in_venv() {
    let exists = |_p: &Path| false;
    let canon = |_p: &Path| None;
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        None
    );
}
