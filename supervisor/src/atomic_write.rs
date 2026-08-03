//! Crash-safe file publication shared by both supervisor backends.
//!
//! Each driver publishes a generated file — a systemd unit, a launchd
//! plist — by staging it beside the destination and renaming over it.
//! The reader on the other side is a service manager reading the file in
//! one shot (`systemctl --user daemon-reload`, `launchctl bootstrap`), so
//! the observable state must stay binary: either the old contents are
//! visible or the new ones, never a torn read.
//!
//! **One implementation, deliberately not one per backend.**
//! [#508](https://github.com/hherb/kastellan/issues/508) was a fix
//! applied to one backend and not the other, and two `cfg`-exclusive
//! copies of this helper would be the same hazard in miniature — worse,
//! since there is no macOS CI runner, a launchd-side copy would not even
//! be *compiled* on a pull request (see HANDOVER's "What CI does not
//! cover"). This module is `cfg`-free, so it compiles and its tests run
//! on Linux **and** macOS.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::SupervisorError;

/// Unique staging path for an atomic write of `path`.
///
/// Appends `.tmp.<pid>.<n>` to the **whole** file name. The per-backend
/// predecessors were `path.with_extension("service.tmp")` and
/// `path.with_extension("plist.tmp")`, which got two things wrong:
///
///   - `with_extension` *replaces* the final `.`-component, so a
///     `.target` unit was staged through `<name>.service.tmp` — the same
///     path a like-named `.service` would use.
///   - the path was a pure function of the destination, so two concurrent
///     writers of one file raced on a single tmp path and the loser's
///     `rename` failed `ENOENT`. That is the production-side twin of the
///     smoke-test name collision fixed in
///     [#509](https://github.com/hherb/kastellan/pull/509); deriving the
///     staging name from the *writer* fixes it for every caller instead
///     of only for callers that manage to pick unique destination names.
///
/// The suffix goes last on purpose, so neither manager mistakes the
/// staging file for something to load: it ends in neither a systemd unit
/// type nor `.plist`.
///
/// (`kastellan-tests-common`, both supervisor smoke binaries and
/// `kastellan-core`'s installer carry the same `pid`-suffix pattern;
/// [#104] tracks de-duplicating it across the workspace.)
///
/// [#104]: https://github.com/hherb/kastellan/issues/104
pub(crate) fn tmp_path_for(path: &Path) -> Result<PathBuf, SupervisorError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| SupervisorError::Io(format!("{} has no file name", path.display())))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(tmp_name))
}

/// Atomically write `bytes` to `path` via write-to-tmp + fsync + rename.
///
/// Both error paths *after* the staging file exists remove it. That
/// matters more than it did with a deterministic tmp name: a retry used
/// to overwrite the previous attempt's leftover, whereas a unique name
/// would otherwise leave one more file per failed write. Nothing is
/// removed when the create itself fails — at that point the path is not
/// ours to delete.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SupervisorError> {
    let tmp = tmp_path_for(path)?;
    let f = create_staging(&tmp)?;
    if let Err(e) = write_and_sync(f, &tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SupervisorError::Io(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Create the staging file, refusing to reuse an existing path.
///
/// `create_new` turns [`tmp_path_for`]'s uniqueness from an assumption
/// into an enforced invariant: a collision becomes a loud error instead
/// of a silent clobber, and a symlink pre-planted at the staging path is
/// refused rather than followed (`O_EXCL`).
fn create_staging(tmp: &Path) -> Result<fs::File, SupervisorError> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| SupervisorError::Io(format!("create {}: {e}", tmp.display())))
}

/// Write `bytes` into the already-created staging file and fsync it.
/// Split out of [`write_atomic`] so the caller has a single error seam to
/// clean up behind.
fn write_and_sync(mut f: fs::File, tmp: &Path, bytes: &[u8]) -> Result<(), SupervisorError> {
    f.write_all(bytes)
        .map_err(|e| SupervisorError::Io(format!("write {}: {e}", tmp.display())))?;
    f.sync_all()
        .map_err(|e| SupervisorError::Io(format!("fsync {}: {e}", tmp.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests;
