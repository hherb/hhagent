//! Driver tests for the Linux `systemd --user` backend.
//!
//! Lifted out of the inline `#[cfg(test)] mod tests` in `systemd_user.rs`
//! when that file outgrew the 500-LOC cap. `use super::*` resolves to the
//! parent `systemd_user` module, which gives these tests the
//! [`SystemdUser`] driver plus the builder functions it re-exports
//! (`build_unit_file`, `build_target_unit`, `validate_service_name`). The
//! pure-builder/validator tests live alongside their code in the sibling
//! `builder.rs`.
//!
//! These exercise the file-writing half of `install`/`uninstall`/
//! `install_target` against a custom units dir, without touching the live
//! `systemctl --user` manager. They run on any host with a writable /tmp.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Minimal spec used as a starting point in driver tests.
fn minimal_spec(name: &str) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        program: PathBuf::from("/usr/bin/true"),
        args: vec![],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_file: None,
    }
}

/// Tempdir helper mirroring `core::workspace::tests::TestRoot`:
/// unique per process+test+call, removed on drop.
struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kastellan-supervisor-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test root");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------- driver tests using a custom units dir ----------

#[test]
fn install_writes_unit_file_with_expected_content() {
    let dir = TestRoot::new("install-content");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let spec = minimal_spec("kastellan-test");
    sup.install(&spec).expect("install");

    let path = sup.unit_path("kastellan-test");
    assert!(path.exists(), "unit file not written: {}", path.display());
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("[Unit]"), "{body}");
    assert!(body.contains("ExecStart=/usr/bin/true"), "{body}");
}

#[test]
fn install_rejects_relative_program_path() {
    let dir = TestRoot::new("rel-program");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.program = PathBuf::from("relative/foo");
    let err = sup.install(&spec).expect_err("relative program");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}

#[test]
fn install_rejects_newline_in_path_field() {
    // Audit finding #10: a newline in a path field would inject a unit-file
    // directive (path fields are written verbatim via Display). Must fail
    // closed before any file is written.
    let dir = TestRoot::new("newline-path");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.working_dir = Some(PathBuf::from("/tmp\nExecStartPre=/evil"));
    let err = sup.install(&spec).expect_err("newline working_dir must be rejected");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        !sup.unit_path("svc").exists(),
        "no unit file may be written when a path field is rejected"
    );
}

#[test]
fn install_rejects_invalid_name() {
    let dir = TestRoot::new("bad-name");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.name = "../traversal".into();
    let err = sup.install(&spec).expect_err("traversal name");
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
}

#[test]
fn install_creates_units_dir_if_missing() {
    let dir = TestRoot::new("nested-dir");
    let nested = dir.path().join("a").join("b").join("c");
    let sup = SystemdUser::with_units_dir(nested.clone());
    sup.install(&minimal_spec("svc")).expect("install");
    assert!(nested.is_dir(), "nested units dir should be created");
    assert!(nested.join("svc.service").is_file());
}

#[test]
fn uninstall_removes_unit_file() {
    let dir = TestRoot::new("uninstall");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.unit_path("svc");
    assert!(path.exists());
    sup.uninstall("svc").expect("uninstall");
    assert!(!path.exists(), "unit file still present after uninstall");
}

#[test]
fn uninstall_is_idempotent_when_nothing_installed() {
    let dir = TestRoot::new("idempotent");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.uninstall("nonexistent")
        .expect("uninstall must be idempotent");
}

#[test]
fn status_returns_not_installed_when_unit_absent() {
    let dir = TestRoot::new("status-absent");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let s = sup.status("never-installed").expect("status");
    assert_eq!(s, ServiceStatus::NotInstalled);
}

// ---------- ordering-field injection rejection tests ----------

#[test]
fn install_rejects_after_entry_with_injection() {
    let dir = TestRoot::new("inject-after");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.after = vec!["pg\n[Service]\nExecStart=/bin/evil".into()];
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    // No unit file should have been written.
    assert!(!dir.path().join("kastellan-core.service").exists());
}

#[test]
fn install_rejects_part_of_with_injection() {
    let dir = TestRoot::new("inject-partof");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.part_of = Some("kastellan\nWantedBy=evil.target".into());
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("kastellan-core.service").exists());
}

#[test]
fn install_target_rejects_member_with_injection() {
    let dir = TestRoot::new("inject-member");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["pg\nExecStart=/bin/evil".into()],
    };
    // members slice can be empty here — the target-name/members validation
    // must fire before any member install.
    let err = sup.install_target(&target, &[]).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("kastellan.target").exists());
}

#[test]
fn install_target_writes_target_unit_and_members_into_units_dir() {
    let dir = std::env::temp_dir().join(format!(
        "kastellan-target-unit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sup = SystemdUser::with_units_dir(dir.clone());

    let mut pg = minimal_spec("kastellan-postgres");
    pg.program = std::path::PathBuf::from("/usr/lib/postgresql/18/bin/postgres");
    pg.part_of = Some("kastellan".into());
    let mut core = minimal_spec("kastellan-core");
    core.program = std::path::PathBuf::from("/opt/kastellan/kastellan");
    core.after = vec!["kastellan-postgres".into()];
    core.part_of = Some("kastellan".into());

    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    sup.install_target(&target, &[pg, core]).expect("install_target");

    // Target unit written with Wants= of both members.
    let target_body =
        std::fs::read_to_string(dir.join("kastellan.target")).expect("target file");
    assert!(
        target_body.contains("Wants=kastellan-postgres.service kastellan-core.service\n"),
        "{target_body}"
    );
    // Member units written, core ordered After= postgres.
    assert!(dir.join("kastellan-postgres.service").exists());
    let core_body =
        std::fs::read_to_string(dir.join("kastellan-core.service")).expect("core file");
    assert!(core_body.contains("After=kastellan-postgres.service\n"), "{core_body}");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------- atomic-write staging (#509 review) ----------

/// Count files in `dir` whose name marks them as an in-flight atomic write.
fn staging_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read units dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect()
}

#[test]
fn tmp_path_keeps_the_whole_unit_name_including_a_target_suffix() {
    // The former `with_extension("service.tmp")` REPLACED the final
    // `.`-component, so `kastellan.target` staged through
    // `kastellan.service.tmp` — the very path a like-named `.service`
    // would have used. The staging name must preserve the destination's
    // own suffix, or two different units share one staging path.
    let svc = tmp_path_for(Path::new("/units/kastellan.service")).expect("tmp for service");
    let tgt = tmp_path_for(Path::new("/units/kastellan.target")).expect("tmp for target");
    let svc_name = svc.file_name().unwrap().to_string_lossy().into_owned();
    let tgt_name = tgt.file_name().unwrap().to_string_lossy().into_owned();

    assert!(svc_name.starts_with("kastellan.service.tmp."), "{svc_name}");
    assert!(tgt_name.starts_with("kastellan.target.tmp."), "{tgt_name}");
    assert_ne!(svc, tgt);
    // Both stay in the destination's directory: the rename must not cross
    // a filesystem boundary, or it stops being atomic.
    assert_eq!(svc.parent(), Some(Path::new("/units")));
    assert_eq!(tgt.parent(), Some(Path::new("/units")));
}

#[test]
fn tmp_path_is_unique_per_call_for_one_destination() {
    // The staging path must be a function of the WRITER, not of the
    // destination — otherwise two concurrent writers of one unit race on
    // a single tmp file and the loser's rename fails ENOENT.
    let p = Path::new("/units/kastellan.service");
    let a = tmp_path_for(p).expect("first");
    let b = tmp_path_for(p).expect("second");
    assert_ne!(a, b, "two writers of one unit must not share a staging path");
}

#[test]
fn successful_install_leaves_no_staging_file_behind() {
    let dir = TestRoot::new("staging-clean");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("kastellan-test")).expect("install");

    assert!(dir.path().join("kastellan-test.service").exists());
    assert!(
        staging_files(dir.path()).is_empty(),
        "staging files left after a successful write: {:?}",
        staging_files(dir.path())
    );
}

#[test]
fn failed_write_removes_its_staging_file() {
    // Cleanup on the error path is not optional now that staging names are
    // unique: a deterministic name meant a retry overwrote the previous
    // attempt's leftover, whereas a unique one would accumulate a file per
    // failed write. Force the failure at the rename by parking a
    // *directory* where the unit file belongs — rename(2) refuses to
    // replace a directory with a file.
    let dir = TestRoot::new("staging-failed");
    fs::create_dir_all(dir.path().join("kastellan-test.service")).expect("blocking dir");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());

    let err = sup
        .install(&minimal_spec("kastellan-test"))
        .expect_err("rename over a directory must fail");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        staging_files(dir.path()).is_empty(),
        "failed write left its staging file behind: {:?}",
        staging_files(dir.path())
    );
}
