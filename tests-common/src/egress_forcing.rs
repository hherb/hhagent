//! Shared scaffolding for the egress force-routing e2e tests. Lifted out of
//! `core/tests/egress_force_routing_e2e.rs` so a second force-routing e2e
//! (`mail_e2e.rs`) can reuse it without a verbatim copy — the #475
//! duplicated-test-helper lesson. Unix-only (`UnixStream`), so scoped to the
//! platforms the egress coupling runs on.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// The sidecar binds its UDS at `<scratch>/egress.sock` — one copy of the
/// crate-private `egress::spawn::UDS_FILE_NAME` (not reachable from an
/// integration test), so the two e2e files agree on the name from one place.
pub const UDS_FILE_NAME: &str = "egress.sock";

/// `spawn_forced_net_worker` mints a unique `egress-<pid>-<seq>/` subdir under
/// the scratch root and the sidecar binds `<that>/egress.sock` in it. Exactly
/// one such subdir exists per spawn, so we resolve the UDS by finding it.
pub fn minted_uds(scratch_root: &Path) -> PathBuf {
    let sub = std::fs::read_dir(scratch_root)
        .expect("read scratch root")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("egress-"))
        .expect("force-routed spawn must mint an egress-* scratch subdir");
    sub.path().join(UDS_FILE_NAME)
}

/// Create a short `/tmp`-based scratch root and return it. Short on purpose:
/// `spawn_forced_net_worker` nests `<root>/egress-<pid>-<seq>/egress.sock`, and
/// that projected UDS path must fit the 104-byte macOS `sockaddr_un.sun_path`
/// (the default `$TMPDIR` on macOS is ~50 chars deep and overflows once nested).
/// `/tmp` exists on both Linux and macOS.
pub fn short_scratch_root(tag: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("kfr-{tag}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Read the proxy's full CONNECT 200 response head (39 bytes — same length the
/// sibling `egress_proxy_e2e` pins) so subsequent reads see only tunnelled bytes.
pub fn assert_connect_established(client: &mut UnixStream) {
    let mut head = [0u8; 39];
    client.read_exact(&mut head).expect("read CONNECT 200 head");
    assert!(
        std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
        "expected a 200 tunnel head, got {:?}",
        std::str::from_utf8(&head)
    );
}
