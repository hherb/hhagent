//! Tests for the shared staging/rename helper.
//!
//! Both hazards these pin were found in a *backend-specific* copy of this
//! code — the `.target`-collapses-onto-`.service` one on Linux, the
//! is-the-staging-file-loadable one on macOS. Now that there is one
//! implementation, both run on both hosts, which is the point: the DGX is
//! blind to `cfg(target_os = "macos")` items and the Mac to
//! `cfg(target_os = "linux")` ones.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Tempdir helper mirroring `systemd_user::tests::TestRoot`:
/// unique per process+test+call, removed on drop.
struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kastellan-atomic-write-test-{}-{}-{}",
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

/// Names in `dir` that mark an in-flight atomic write.
fn staging_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect()
}

#[test]
fn tmp_path_keeps_the_whole_file_name_including_a_target_suffix() {
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
fn tmp_path_keeps_the_plist_suffix_first_so_staging_is_not_loadable() {
    // launchd loads every `*.plist` in `~/Library/LaunchAgents/`, so the
    // `.tmp.<pid>.<n>` part must stay AFTER `.plist` — a staging file that
    // still ends in `.plist` is itself a loadable agent.
    let p = tmp_path_for(Path::new("/agents/com.example.svc.plist")).expect("tmp for plist");
    let name = p.file_name().unwrap().to_string_lossy().into_owned();

    assert!(name.starts_with("com.example.svc.plist.tmp."), "{name}");
    assert!(!name.ends_with(".plist"), "staging file must not look loadable: {name}");
}

#[test]
fn tmp_path_is_unique_per_call_for_one_destination() {
    // The staging path must be a function of the WRITER, not of the
    // destination — otherwise two concurrent writers of one file race on
    // a single tmp path and the loser's rename fails ENOENT.
    let p = Path::new("/units/kastellan.service");
    let a = tmp_path_for(p).expect("first");
    let b = tmp_path_for(p).expect("second");
    assert_ne!(a, b, "two writers of one destination must not share a staging path");
}

#[test]
fn successful_write_publishes_the_bytes_and_leaves_no_staging_file() {
    let dir = TestRoot::new("clean");
    let dest = dir.path().join("kastellan.service");

    write_atomic(&dest, b"[Unit]\n").expect("write");

    assert_eq!(fs::read_to_string(&dest).expect("read back"), "[Unit]\n");
    assert!(
        staging_files(dir.path()).is_empty(),
        "staging files left after a successful write: {:?}",
        staging_files(dir.path())
    );
}

#[test]
fn a_second_write_replaces_the_previous_contents() {
    let dir = TestRoot::new("replace");
    let dest = dir.path().join("kastellan.service");

    write_atomic(&dest, b"old\n").expect("first write");
    write_atomic(&dest, b"new\n").expect("second write");

    assert_eq!(fs::read_to_string(&dest).expect("read back"), "new\n");
    assert!(staging_files(dir.path()).is_empty(), "{:?}", staging_files(dir.path()));
}

#[test]
fn failed_write_removes_its_staging_file() {
    // Cleanup on the error path is not optional now that staging names
    // are unique: a deterministic name meant a retry overwrote the
    // previous attempt's leftover, whereas a unique one would accumulate
    // a file per failed write. Force the failure at the rename by parking
    // a *directory* where the file belongs — rename(2) refuses to replace
    // a directory with a file.
    //
    // This exercises the rename seam. The other cleanup seam (a failed
    // `write`/`fsync`) is left untested on purpose: it is reachable only
    // from ENOSPC/EIO, which no hermetic test can force portably.
    let dir = TestRoot::new("failed");
    let dest = dir.path().join("kastellan.service");
    fs::create_dir_all(&dest).expect("blocking dir");

    let err = write_atomic(&dest, b"[Unit]\n").expect_err("rename over a directory must fail");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        staging_files(dir.path()).is_empty(),
        "failed write left its staging file behind: {:?}",
        staging_files(dir.path())
    );
}

/// Permission bits of `path`, as the low 12 bits of `st_mode`.
#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

#[cfg(unix)]
#[test]
fn published_files_are_private_to_the_owner() {
    // #529: the staging file used to be opened with no mode, so every unit
    // file and every LaunchAgent plist landed at the umask default (0644).
    //
    // That matters most on macOS, where launchd has no `EnvironmentFile=`
    // directive and the backend therefore folds the env files' KEY=value
    // pairs into the plist's `EnvironmentVariables` at install time — so a
    // world-readable plist is a world-readable copy of `kastellan.env`,
    // which `write_private` deliberately writes 0600.
    //
    // Asserting on the DESTINATION rather than the staging file is
    // deliberate and sufficient: `rename` keeps the inode, so the mode the
    // staging file was created with is the mode the published file has.
    let dir = TestRoot::new("mode");
    let dest = dir.path().join("kastellan.service");

    write_atomic(&dest, b"[Unit]\n").expect("write");

    // The claim is "no group or other bits", not the literal 0600: `open`
    // masks the requested mode by the process umask, so a hardened
    // `umask 0200` legitimately publishes 0400. Asserting the exact value
    // would fail there for a reason unrelated to the property.
    assert_eq!(mode_of(&dest) & 0o077, 0, "published unit file must be owner-only");
    assert_ne!(mode_of(&dest) & 0o400, 0, "and still readable by its owner");
}

#[cfg(unix)]
#[test]
fn republishing_tightens_a_previously_world_readable_file() {
    // The upgrade path, and the reason the assertion above is not enough on
    // its own: hosts installed before #529 already have 0644 units on disk.
    // Because `write_atomic` publishes by renaming a fresh inode over the
    // destination rather than truncating it in place, the new mode wins —
    // one reinstall repairs the mode with no operator step. A truncating
    // writer would have kept 0644 here and passed the previous test.
    let dir = TestRoot::new("tighten");
    let dest = dir.path().join("kastellan.service");
    fs::write(&dest, b"stale\n").expect("pre-existing file");
    fs::set_permissions(&dest, <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644))
        .expect("chmod 644");

    write_atomic(&dest, b"[Unit]\n").expect("write");

    assert_eq!(mode_of(&dest) & 0o077, 0, "reinstall must tighten a legacy 0644 unit file");
}

#[test]
fn write_into_a_missing_directory_errors_without_leaving_anything() {
    // The create seam: nothing was created, so nothing is removed — and
    // in particular the helper must not delete a path it does not own.
    let dir = TestRoot::new("nodir");
    let dest = dir.path().join("no-such-dir").join("kastellan.service");

    let err = write_atomic(&dest, b"[Unit]\n").expect_err("missing parent dir must fail");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(staging_files(dir.path()).is_empty(), "{:?}", staging_files(dir.path()));
}
