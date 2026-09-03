//! Owner-private directories and files for per-spawn state under a shared
//! temp root (security audit 2026-09-02, findings S1–S4 / F4).
//!
//! Every per-spawn directory the daemon mints — an egress sidecar's scratch
//! (`egress-<pid>-<seq>`), a broker's, the Matrix channel's, a Firecracker run
//! dir — used to be created with `create_dir_all` under `std::env::temp_dir()`
//! (`/tmp`) and a name computable from `/proc`. `create_dir_all` returns `Ok`
//! on a directory that already exists **whoever owns it**, so on a multi-user
//! host another local uid could pre-create the next name, own it, and then
//! substitute the proxy UDS + MITM CA the worker is about to be bound to, read
//! the `secret_hashes.json` written into it, or plant symlinks for the
//! daemon's `fs::write` calls to follow. This module is the one place that
//! shape is allowed to be spelled:
//!
//! * [`create_private_dir`] — **exclusive** creation (`mkdir`, never
//!   `mkdir -p`): anything already at the path — a directory, a symlink, a
//!   file — is an error, so a pre-planted name fails the spawn closed instead
//!   of being adopted. Mode `0700`, then verified.
//! * [`ensure_private_dir`] — for a path that legitimately persists across
//!   spawns (the Matrix VM password dir keyed by daemon pid): create it if
//!   absent, otherwise **verify** it is a real directory (not a symlink) owned
//!   by this uid with no group/other bits. Another uid cannot produce a
//!   directory that passes the uid check, so adoption is sound.
//! * [`create_private_file`] — `O_CREAT|O_EXCL` + mode `0600` for the secret-
//!   bearing files written into those directories (`secret_hashes.json`, the
//!   VM config, image files before `mkfs` fills them).
//! * [`harden_owned_file_mode`] — `chmod 0600` for a pre-existing file the
//!   daemon owns (a persistent VM image created before this module existed).
//!
//! The name predictability itself is unchanged and deliberately so: the
//! orphan sweeps parse `<prefix><pid>-<seq>`, and a pre-planted name now costs
//! the attacker only one failed spawn of ours (a local-DoS, outside the
//! invariant), never adoption.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

fn other(msg: String) -> io::Error {
    io::Error::other(msg)
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Verify `dir` is a real directory (not a symlink) owned by this uid with no
/// group/other permission bits. Used after creation and by
/// [`ensure_private_dir`] on adoption.
fn verify_private_dir(dir: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(dir)?;
    if md.file_type().is_symlink() {
        return Err(other(format!("{}: is a symlink, refusing to use it", dir.display())));
    }
    if !md.is_dir() {
        return Err(other(format!("{}: exists but is not a directory", dir.display())));
    }
    let uid = current_uid();
    if md.uid() != uid {
        return Err(other(format!(
            "{}: owned by uid {}, not this daemon's uid {uid} — refusing a directory another \
             principal controls",
            dir.display(),
            md.uid()
        )));
    }
    if md.mode() & 0o077 != 0 {
        return Err(other(format!(
            "{}: mode {:o} grants group/other access; expected 0700",
            dir.display(),
            md.mode() & 0o777
        )));
    }
    Ok(())
}

/// Create `dir` **exclusively** with mode `0700` and verify it. Fails if
/// anything already exists at `dir`, whatever it is and whoever owns it.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(dir).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            other(format!(
                "{}: already exists — refusing to adopt a pre-existing per-spawn directory \
                 (another principal may have planted it)",
                dir.display()
            ))
        } else {
            e
        }
    })?;
    // `mode(0o700)` is subject to umask, which can only remove bits; make the
    // intended mode explicit regardless of the daemon's umask.
    std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700))?;
    verify_private_dir(dir)
}

/// Create `dir` (mode `0700`) if absent; if present, adopt it **only** after
/// [`verify_private_dir`] passes. The recursive form is deliberately not
/// offered: every parent must already exist and is not verified here.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {
            std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700))?;
            verify_private_dir(dir)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => verify_private_dir(dir),
        Err(e) => Err(e),
    }
}

/// Create `path` with `O_CREAT | O_EXCL` (fails if it exists, symlinks
/// included) and mode `0600`, returning the open handle for writing.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// `chmod 0600` a pre-existing regular file this uid owns (refuses symlinks
/// and foreign owners). For files created before this module existed, e.g. a
/// persistent VM image under a group-traversable directory.
pub fn harden_owned_file_mode(path: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() || !md.is_file() {
        return Err(other(format!("{}: not a regular file", path.display())));
    }
    if md.uid() != current_uid() {
        return Err(other(format!(
            "{}: owned by uid {}, not this daemon's uid — refusing to adopt it",
            path.display(),
            md.uid()
        )));
    }
    if md.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, PermissionsExt::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn create_is_exclusive_and_0700() {
        let root = tmp();
        let d = root.path().join("egress-1-0");
        create_private_dir(&d).unwrap();
        let md = std::fs::metadata(&d).unwrap();
        assert!(md.is_dir());
        assert_eq!(md.mode() & 0o777, 0o700);
        // A second creation of the same name is refused — the pre-planted case.
        let err = create_private_dir(&d).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[test]
    fn create_refuses_a_planted_symlink_and_a_planted_file() {
        let root = tmp();
        let target = root.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let link = root.path().join("egress-1-1");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(create_private_dir(&link).is_err(), "symlink must not be adopted");
        let file = root.path().join("egress-1-2");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_private_dir(&file).is_err(), "a file must not be adopted");
    }

    #[test]
    fn ensure_adopts_only_our_own_0700_dir() {
        let root = tmp();
        let d = root.path().join("kastellan-matrix-42");
        ensure_private_dir(&d).unwrap();
        ensure_private_dir(&d).unwrap(); // idempotent on our own dir
        // Widened mode is refused on adoption (would be a foreign or tampered dir).
        std::fs::set_permissions(&d, PermissionsExt::from_mode(0o755)).unwrap();
        let err = ensure_private_dir(&d).unwrap_err();
        assert!(err.to_string().contains("group/other"), "{err}");
        // A symlink at the path is refused.
        let link = root.path().join("kastellan-matrix-43");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }

    #[test]
    fn private_file_is_exclusive_and_0600() {
        let root = tmp();
        let p = root.path().join("secret_hashes.json.tmp");
        let f = create_private_file(&p).unwrap();
        drop(f);
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        assert!(create_private_file(&p).is_err(), "O_EXCL must refuse an existing file");
        // A dangling symlink at the path must not be followed into creation.
        let link = root.path().join("fc.json");
        std::os::unix::fs::symlink(root.path().join("victim"), &link).unwrap();
        assert!(create_private_file(&link).is_err());
        assert!(!root.path().join("victim").exists());
    }

    #[test]
    fn harden_owned_file_mode_chmods_our_file_and_refuses_symlinks() {
        let root = tmp();
        let p = root.path().join("state.ext4");
        std::fs::write(&p, b"img").unwrap();
        std::fs::set_permissions(&p, PermissionsExt::from_mode(0o644)).unwrap();
        harden_owned_file_mode(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        let link = root.path().join("link.ext4");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(harden_owned_file_mode(&link).is_err());
    }
}
