#![cfg(target_os = "linux")]
//! Slice 4a e2e: proves the guest-initiated vsock egress reverse-channel on real
//! KVM. A force-routed VM (Net::Allowlist + proxy_uds) boots with the self-test
//! knob; the guest init dials the in-guest egress UDS, which relays over a second
//! vsock port to the launcher's reverse-relay and on to a host echo UnixListener
//! standing in for the egress proxy. We assert the host echo RECEIVES the guest's
//! PING — the novel guest→host direction, observed entirely host-side.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + a built rootfs
//! (REBUILD via build-rootfs.sh so it carries the /run mountpoint) + the
//! kastellan-microvm-run RELEASE launcher (rebuild it; target/release is
//! preferred and a stale one silently shadows source changes). Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH   # firecracker is off the ssh PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test firecracker_egress_channel_e2e -- --ignored --nocapture

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use kastellan_core::tool_host::{spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::Net;
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "python-exec.ext4";

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with /run mountpoint"]
async fn egress_reverse_channel_delivers_guest_ping_to_host_proxy_uds() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }

    // Host echo "proxy": the proxy_uds target. On accept, read PING and reply PONG,
    // signalling receipt back to the test thread.
    let dir = std::env::temp_dir().join(format!("kastellan-s4a-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo_path = dir.join("egress.sock");
    let _ = std::fs::remove_file(&echo_path);
    let listener = UnixListener::bind(&echo_path).unwrap();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        if let Ok((mut c, _)) = listener.accept() {
            let mut buf = [0u8; 5];
            if c.read_exact(&mut buf).is_ok() {
                let _ = tx.send(buf.to_vec());
                let _ = c.write_all(b"PONG\n");
            }
        }
    });

    // Force-routed entry: python-exec rootfs, but Net::Allowlist + proxy_uds +
    // the self-test knob. The worker process is irrelevant here — the init's
    // self-test originates the PING during boot.
    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.net = Net::Allowlist(vec!["example.com:443".into()]);
    entry.policy.proxy_uds = Some(echo_path.clone());
    entry.policy.env.push(("KASTELLAN_MICROVM_EGRESS_SELFTEST".into(), "1".into()));

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let worker = spawn_worker(&*backend, &spec).expect("spawn force-routed worker in micro-VM");

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("host proxy UDS never received the guest PING (reverse channel broken)");
    assert_eq!(&got, b"PING\n", "guest-initiated egress reached the host proxy UDS");

    let _ = worker.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The guest side of the relay: `/run`'s mode (#672) and whether the worker can
/// actually reach the relay socket it is configured to dial (#669, #670).
///
/// **Why this is worth its own boot.** Every other test on this path observes
/// the relay from the HOST — the reverse-channel test above proves a PING
/// arrives, but the PING is originated by the guest *init*, which is still
/// root. The property that matters after W-2's privilege drop is whether the
/// unprivileged **worker** can connect, and that had never been read from
/// inside the guest: it was inferred from the networked suites passing, which
/// is exactly the kind of indirect evidence that let the socket stay
/// root-owned from the 2026-09-02 audit until the gate that found it.
///
/// Three facts, each pinning a different fix:
///
/// - **`/run` is 0755, not 1777.** The kernel's tmpfs default is
///   world-writable and sticky, which nobody chose and which silently granted
///   the worker everything the (removed) `/run` chown was justified by. At 0755
///   the per-socket chown is the only grant, so deleting it must break the
///   worker rather than being masked (#672).
/// - **The socket is owned by the worker's own uid** — the #669 fix, read from
///   inside the guest for the first time rather than inferred.
/// - **The worker can write to it**, which is the property `connect(2)` on an
///   `AF_UNIX` socket actually requires and the one whose absence produced
///   `connect proxy uds: Permission denied` naming the proxy rather than the
///   cause.
///
/// The socket path is taken from the worker's own environment rather than
/// re-spelled here: a fourth hand-copied literal of `/run/kastellan-egress.sock`
/// is how this family of constants drifts, and reading the env additionally
/// asserts the worker was *configured* to dial the socket the init bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with /run mountpoint"]
async fn guest_run_dir_and_relay_socket_are_reachable_by_the_unprivileged_worker() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }

    // A host listener at the proxy UDS: the plan only wires the relay when the
    // policy carries a proxy_uds, and the launcher binds its reverse-relay
    // before boot. Nothing needs to be accepted — the worker never dials here;
    // we only inspect the in-guest socket the init bound.
    let dir = std::env::temp_dir().join(format!("kastellan-672-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo_path = dir.join("egress.sock");
    let _ = std::fs::remove_file(&echo_path);
    let _listener = UnixListener::bind(&echo_path).unwrap();

    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.net = Net::Allowlist(vec!["example.com:443".into()]);
    entry.policy.proxy_uds = Some(echo_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker =
        spawn_worker(&*backend, &spec).expect("spawn force-routed worker in micro-VM");

    let code = "import os\n\
        uds = os.environ.get('KASTELLAN_EGRESS_PROXY_UDS', '')\n\
        print('kastellan_uds=%s' % (uds or 'unset'))\n\
        print('kastellan_run_mode=%o' % (os.stat('/run').st_mode & 0o7777))\n\
        print('kastellan_my_uid=%d' % os.getuid())\n\
        if uds:\n\
        \x20   st = os.stat(uds)\n\
        \x20   print('kastellan_sock_uid=%d' % st.st_uid)\n\
        \x20   print('kastellan_sock_writable=%d' % (1 if os.access(uds, os.W_OK) else 0))\n";
    let out = kastellan_core::tool_host::dispatch_with_sink(
        &kastellan_tests_common::NoopAuditSink,
        &kastellan_core::secrets::Vault::new(),
        None,
        &mut worker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await
    .expect("dispatch python.exec in the force-routed micro-VM");
    let _ = worker.close();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(out["exit_code"], 0, "clean exit expected: {out}");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    let fact = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("guest printed no {key} line: {out}"))
            .to_string()
    };

    assert_ne!(
        fact("kastellan_uds"),
        "unset",
        "a force-routed worker must be told which UDS to dial; without it the rest of \
         this test would assert nothing: {out}"
    );
    assert_eq!(
        fact("kastellan_run_mode"),
        "755",
        "the guest /run tmpfs must be mounted with an explicit mode=0755 (#672). 1777 is \
         the kernel default nobody chose: world-writable and sticky, and it masks whether \
         the per-socket chown is doing anything: {out}"
    );
    assert_eq!(
        fact("kastellan_sock_uid"),
        fact("kastellan_my_uid"),
        "the relay socket must be owned by the uid the worker runs as (#669). Root-owned \
         is the defect that killed every networked VM worker between the 2026-09-02 audit \
         and the gate that found it: {out}"
    );
    assert_eq!(
        fact("kastellan_sock_writable"),
        "1",
        "the worker must have WRITE permission on the socket file — that, not ownership \
         as such, is what connect(2) on an AF_UNIX socket requires: {out}"
    );
}
