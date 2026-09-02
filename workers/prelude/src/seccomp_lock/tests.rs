//! Unit tests for the parent [`super`] seccomp module.
//!
//! Lifted out of `seccomp_lock.rs` (sibling test-module split,
//! 2026-07-06) to keep the production file under the 500-LOC cap. The
//! `use super::*` below resolves to the parent `seccomp_lock` module,
//! reaching [`super::Profile`], the BPF builders, and every allow-list
//! table the parent re-exports from its `allow_lists` sibling. Test
//! bodies are verbatim moves.

use super::*;

#[test]
fn profile_parse_recognises_known_values() {
    assert_eq!(Profile::parse("strict").unwrap(), Some(Profile::Strict));
    assert_eq!(
        Profile::parse("net_client").unwrap(),
        Some(Profile::NetClient)
    );
    assert_eq!(
        Profile::parse("browser_client").unwrap(),
        Some(Profile::BrowserClient)
    );
    assert_eq!(Profile::parse("none").unwrap(), None);
    assert_eq!(Profile::parse("").unwrap(), None);
}

#[test]
fn profile_parse_rejects_unknown() {
    assert!(Profile::parse("garbage").is_err());
}

#[test]
fn build_bpf_strict_succeeds() {
    // Just verifies the rule construction + BPF compilation works on
    // the test host's arch. Doesn't actually load the filter (which
    // would poison subsequent tests).
    let bpf = build_bpf(Profile::Strict).expect("strict bpf must build");
    assert!(!bpf.is_empty(), "expected non-empty BPF program");
}

#[test]
fn build_bpf_net_client_succeeds() {
    let bpf = build_bpf(Profile::NetClient).expect("net_client bpf must build");
    assert!(!bpf.is_empty(), "expected non-empty BPF program");
}

#[test]
fn unshare_is_not_in_allow_list() {
    // The most important syscall in our threat model — escape into a
    // fresh user namespace — must NOT appear in any profile's
    // allow-list. If this regresses, the worker can re-enter
    // unshare(CLONE_NEWUSER) and bypass the namespace boundary.
    for profile in [Profile::Strict, Profile::NetClient] {
        let allow = allow_list_for(profile);
        assert!(
            !allow.contains(&libc::SYS_unshare),
            "unshare must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_mount),
            "mount must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_ptrace),
            "ptrace must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_bpf),
            "bpf must never be allow-listed (profile {profile:?})"
        );
    }
}

#[test]
fn socket_is_only_in_net_client_profile() {
    // The hard line between Strict and NetClient: socket() and the
    // BSD-socket family must be allowed under NetClient and killed
    // under Strict. This is the test that proves the two profiles
    // differ — if it ever regresses, NetClient and Strict have
    // collapsed back into the same set.
    let strict = allow_list_for(Profile::Strict);
    let net_client = allow_list_for(Profile::NetClient);

    assert!(
        !strict.contains(&libc::SYS_socket),
        "Strict must not allow socket()"
    );
    assert!(
        net_client.contains(&libc::SYS_socket),
        "NetClient must allow socket()"
    );

    // Sanity: the difference is exactly NET_CLIENT_ADDITIONS.
    for nr in NET_CLIENT_ADDITIONS {
        assert!(
            !strict.contains(nr),
            "syscall {nr} present in Strict but should be NetClient-only"
        );
        assert!(
            net_client.contains(nr),
            "syscall {nr} missing from NetClient"
        );
    }
}

#[test]
fn essentials_are_in_base_allow_list() {
    // Smoke test: a handful of syscalls that *every* worker hits
    // during normal operation must be in the base list. If one of
    // these regresses, the worker dies in a confusing way (SIGSYS at
    // startup with no obvious cause) — surface the failure here
    // instead.
    for nr in [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_openat,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_brk,
        libc::SYS_futex,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_wait4,
        libc::SYS_exit_group,
        libc::SYS_rt_sigreturn,
    ] {
        assert!(
            BASE_ALLOW.contains(&nr),
            "essential syscall {nr} missing from BASE_ALLOW"
        );
    }
}

#[test]
fn build_bpf_browser_client_succeeds() {
    let bpf = build_bpf(Profile::BrowserClient).expect("browser_client bpf must build");
    assert!(!bpf.is_empty(), "browser_client filter must emit instructions");
}

#[test]
fn io_uring_eperm_filter_builds() {
    let bpf = build_io_uring_eperm_bpf().expect("io_uring EPERM filter must build");
    assert!(!bpf.is_empty(), "io_uring EPERM filter must emit instructions");
}

#[test]
fn browser_client_is_a_superset_of_net_client() {
    // BrowserClient must allow everything NetClient does (it's net_client +
    // the browser additions), so a browser worker is never *more* restricted
    // on the socket family than the egress proxy.
    let net_client = allow_list_for(Profile::NetClient);
    let browser = allow_list_for(Profile::BrowserClient);
    for nr in net_client {
        assert!(
            browser.contains(&nr),
            "BrowserClient missing NetClient syscall {nr}"
        );
    }
    // socket() in particular (the net/strict dividing line).
    assert!(browser.contains(&libc::SYS_socket));
}

#[test]
fn browser_client_includes_the_spike_additions() {
    let browser = allow_list_for(Profile::BrowserClient);
    let strict = allow_list_for(Profile::Strict);
    for nr in BROWSER_CLIENT_ADDITIONS {
        assert!(
            browser.contains(nr),
            "BrowserClient missing spike syscall {nr}"
        );
        assert!(
            !strict.contains(nr),
            "browser syscall {nr} leaked into Strict"
        );
    }
}

#[test]
fn io_uring_is_allowed_in_the_main_filter_but_eperm_listed_separately() {
    // io_uring MUST be in the main allow-list (so the main filter returns
    // Allow, not Kill) — the second filter then downgrades it to EPERM.
    // Neither Strict nor NetClient list io_uring at all.
    let browser = allow_list_for(Profile::BrowserClient);
    for nr in BROWSER_IO_URING {
        assert!(
            browser.contains(nr),
            "io_uring {nr} must be in the BrowserClient main allow-list"
        );
        assert!(
            !allow_list_for(Profile::NetClient).contains(nr),
            "io_uring {nr} must NOT be in NetClient"
        );
        assert!(
            !allow_list_for(Profile::Strict).contains(nr),
            "io_uring {nr} must NOT be in Strict"
        );
    }
}

#[test]
fn profile_parse_recognises_ml_client() {
    assert_eq!(Profile::parse("ml_client").unwrap(), Some(Profile::MlClient));
}

#[test]
fn build_bpf_ml_client_succeeds() {
    let bpf = build_bpf(Profile::MlClient).expect("ml_client bpf must build");
    assert!(!bpf.is_empty(), "ml_client filter must emit instructions");
}

#[test]
fn ml_client_is_a_superset_of_net_client() {
    // ml_client = net_client + ML additions, so it must allow everything
    // net_client does (notably the socket family torch needs even offline).
    let net_client = allow_list_for(Profile::NetClient);
    let ml = allow_list_for(Profile::MlClient);
    for nr in net_client {
        assert!(ml.contains(&nr), "MlClient missing NetClient syscall {nr}");
    }
    assert!(ml.contains(&libc::SYS_socket), "MlClient must allow socket()");
}

#[test]
fn ml_client_excludes_escape_primitives() {
    // The threat-model invariant: even a torch-tier worker must never be able
    // to escape its namespace / inspect other processes / load BPF.
    let ml = allow_list_for(Profile::MlClient);
    for nr in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ] {
        assert!(!ml.contains(&nr), "MlClient must never allow {nr}");
    }
}

#[test]
fn ml_client_includes_enumerated_numa_additions() {
    // The DGX-enumerated torch additions (NUMA memory-policy syscalls) must
    // be present in MlClient and ML-specific — i.e. NOT already granted by
    // the Strict base (otherwise they'd belong in BASE_ALLOW, not here).
    let ml = allow_list_for(Profile::MlClient);
    let strict = allow_list_for(Profile::Strict);
    assert!(
        !ML_CLIENT_ADDITIONS.is_empty(),
        "ML_CLIENT_ADDITIONS was populated by the DGX enumeration"
    );
    for nr in ML_CLIENT_ADDITIONS {
        assert!(ml.contains(nr), "MlClient missing enumerated syscall {nr}");
        assert!(
            !strict.contains(nr),
            "enumerated syscall {nr} is already in Strict — move it to BASE_ALLOW"
        );
    }
}

#[test]
fn profile_parse_recognises_matrix_client() {
    assert_eq!(
        Profile::parse("matrix_client").unwrap(),
        Some(Profile::MatrixClient)
    );
}

#[test]
fn build_bpf_matrix_client_succeeds() {
    let bpf = build_bpf(Profile::MatrixClient).expect("matrix_client bpf must build");
    assert!(!bpf.is_empty(), "matrix_client filter must emit instructions");
}

#[test]
fn matrix_client_is_a_superset_of_net_client() {
    // matrix_client = net_client + MATRIX additions, so it must allow
    // everything net_client does (the socket family matrix-sdk needs for
    // homeserver I/O + reconnects).
    let net_client = allow_list_for(Profile::NetClient);
    let mx = allow_list_for(Profile::MatrixClient);
    for nr in net_client {
        assert!(mx.contains(&nr), "MatrixClient missing NetClient syscall {nr}");
    }
    assert!(mx.contains(&libc::SYS_socket), "MatrixClient must allow socket()");
}

#[test]
fn matrix_client_excludes_escape_primitives() {
    // Threat-model invariant: the worker with the largest external attack
    // surface must never be able to escape its namespace / inspect other
    // processes / load BPF.
    let mx = allow_list_for(Profile::MatrixClient);
    for nr in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ] {
        assert!(!mx.contains(&nr), "MatrixClient must never allow {nr}");
    }
}

#[test]
fn matrix_client_includes_enumerated_additions() {
    // The DGX-enumerated matrix-sdk additions (SQLite ftruncate today) must
    // be present in MatrixClient and matrix-specific — i.e. NOT already
    // granted by the Strict/net_client base (else they'd belong there).
    let mx = allow_list_for(Profile::MatrixClient);
    let net_client = allow_list_for(Profile::NetClient);
    assert!(
        !MATRIX_CLIENT_ADDITIONS.is_empty(),
        "MATRIX_CLIENT_ADDITIONS was populated by the DGX enumeration"
    );
    for nr in MATRIX_CLIENT_ADDITIONS {
        assert!(mx.contains(nr), "MatrixClient missing enumerated syscall {nr}");
        assert!(
            !net_client.contains(nr),
            "enumerated syscall {nr} is already in net_client — drop it from MATRIX_CLIENT_ADDITIONS"
        );
    }
}

// ── Namespace-flag `clone` guard + `clone3` ENOSYS overlay (security audit
// 2026-09-02). `unshare(CLONE_NEWUSER)` was killed all along, but
// `clone(CLONE_NEWUSER)` mints the same fresh user namespace and used to be an
// unconditional allow; `clone3` carries its flags in a struct seccomp cannot
// read, so it must answer ENOSYS for glibc to fall back to `clone`. ──

#[test]
fn namespace_clone_flags_cover_every_namespace_kind_and_nothing_else() {
    for bit in [
        libc::CLONE_NEWNS,
        libc::CLONE_NEWCGROUP,
        libc::CLONE_NEWUTS,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWNET,
    ] {
        assert_ne!(NAMESPACE_CLONE_FLAGS & bit as u64, 0, "mask must include {bit:#x}");
    }
    // The bits an ordinary fork / pthread_create carries must NOT be in the
    // mask, or every worker would die on its first thread. The low byte is the
    // exit signal (SIGCHLD = 17 for fork).
    for bit in [
        libc::CLONE_VM,
        libc::CLONE_FS,
        libc::CLONE_FILES,
        libc::CLONE_SIGHAND,
        libc::CLONE_THREAD,
        libc::CLONE_SYSVSEM,
        libc::CLONE_SETTLS,
        libc::CLONE_PARENT_SETTID,
        libc::CLONE_CHILD_CLEARTID,
        libc::CLONE_CHILD_SETTID,
        libc::CLONE_VFORK,
        libc::SIGCHLD,
    ] {
        assert_eq!(NAMESPACE_CLONE_FLAGS & bit as u64, 0, "mask must not include {bit:#x}");
    }
    // CLONE_NEWTIME (0x80) overlaps the exit-signal byte on clone(2) and is
    // deliberately excluded (see the const's doc).
    assert_eq!(NAMESPACE_CLONE_FLAGS & 0x80, 0);
}

#[test]
fn clone3_enosys_overlay_builds_for_this_arch() {
    let bpf = build_clone3_enosys_bpf().expect("clone3 overlay must build");
    assert!(!bpf.is_empty(), "clone3 ENOSYS filter must emit instructions");
}

#[test]
fn clone_and_clone3_stay_in_every_allow_list() {
    // Both must remain LISTED: the main filter has to answer Allow for
    // clone3 so the ENOSYS overlay (not KillProcess) wins, and clone's
    // conditional rule is attached to its listed entry.
    for profile in [
        Profile::Strict,
        Profile::NetClient,
        Profile::BrowserClient,
        Profile::MlClient,
        Profile::MatrixClient,
    ] {
        let allow = allow_list_for(profile);
        assert!(allow.contains(&libc::SYS_clone), "{profile:?} must list clone");
        assert!(allow.contains(&libc::SYS_clone3), "{profile:?} must list clone3");
        assert!(!allow.contains(&libc::SYS_unshare), "{profile:?} must never list unshare");
        assert!(!allow.contains(&libc::SYS_setns), "{profile:?} must never list setns");
        build_bpf(profile).expect("main filter with the clone condition must build");
    }
}

/// Real-kernel behavioural pins. Each case forks, installs the production
/// filter stack in the CHILD via [`apply`] (this needs no privilege:
/// `PR_SET_NO_NEW_PRIVS` + unprivileged seccomp), performs one action, and
/// the parent reads how the child ended. Installation is one-way per process,
/// which is why it happens in a forked child and never in the test process.
#[cfg(target_os = "linux")]
mod behavioural {
    use super::*;

    /// Fork; run `body` in the child under `Profile::Strict`; return the raw
    /// wait status. `body` must end the child itself (or die trying).
    fn run_locked_child(body: fn() -> !) -> libc::c_int {
        // SAFETY: fork() in a test process is sound as long as the child
        // only performs async-signal-safe work before `_exit`/death, which
        // every body below respects (raw syscalls, `_exit`).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            if apply(Profile::Strict).is_err() {
                // Distinguishable from every other outcome the bodies produce.
                unsafe { libc::_exit(99) };
            }
            body();
        }
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid on our own child with a valid out-pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(rc, pid, "waitpid failed: {}", std::io::Error::last_os_error());
        status
    }

    fn seccomp_unavailable(status: libc::c_int) -> bool {
        // apply() failed inside the child — e.g. a kernel without seccomp
        // filter support. Skip-as-pass would be a false green; report loudly.
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 99
    }

    #[test]
    fn clone_with_clone_newuser_is_killed_by_sigsys() {
        fn body() -> ! {
            // SAFETY: a raw clone(2) with a NULL child stack is the fork
            // idiom; the flag set asks for a fresh user namespace. Under the
            // filter this never returns.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                )
            };
            // Reached only if the filter did not fire: exit with a code the
            // parent will reject. (rc == 0 would be the namespace child;
            // exit either way.)
            let _ = rc;
            unsafe { libc::_exit(1) }
        }
        let status = run_locked_child(body);
        assert!(!seccomp_unavailable(status), "seccomp filter could not be installed");
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
            "clone(CLONE_NEWUSER) must die with SIGSYS, got status {status:#x}"
        );
    }

    #[test]
    fn clone_with_clone_newnet_is_killed_by_sigsys() {
        fn body() -> ! {
            let _ = unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_NEWNET | libc::SIGCHLD) as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                )
            };
            unsafe { libc::_exit(1) }
        }
        let status = run_locked_child(body);
        assert!(!seccomp_unavailable(status), "seccomp filter could not be installed");
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
            "clone(CLONE_NEWNET) must die with SIGSYS, got status {status:#x}"
        );
    }

    #[test]
    fn clone3_answers_enosys_instead_of_killing() {
        fn body() -> ! {
            // SAFETY: a NULL args pointer with size 0 would earn EINVAL from a
            // kernel that actually ran the syscall; the overlay must answer
            // ENOSYS before the kernel ever looks at the arguments.
            let rc = unsafe { libc::syscall(libc::SYS_clone3, 0 as libc::c_long, 0 as libc::c_long) };
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            let code = if rc == -1 && errno == libc::ENOSYS { 0 } else { 2 };
            unsafe { libc::_exit(code) }
        }
        let status = run_locked_child(body);
        assert!(!seccomp_unavailable(status), "seccomp filter could not be installed");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "clone3 must return ENOSYS (glibc then falls back to clone), got status {status:#x}"
        );
    }

    #[test]
    fn plain_fork_and_threads_still_work_under_the_filter() {
        fn body() -> ! {
            // glibc's fork() tries clone3 first, gets ENOSYS, falls back to
            // clone(SIGCHLD|…) — which must pass the flags condition.
            let grandchild = unsafe { libc::fork() };
            if grandchild < 0 {
                unsafe { libc::_exit(3) }
            }
            if grandchild == 0 {
                unsafe { libc::_exit(0) }
            }
            let mut st: libc::c_int = 0;
            let rc = unsafe { libc::waitpid(grandchild, &mut st, 0) };
            if rc != grandchild || !libc::WIFEXITED(st) || libc::WEXITSTATUS(st) != 0 {
                unsafe { libc::_exit(4) }
            }
            // pthread_create takes the same clone3 → clone fallback path.
            let joined = std::thread::spawn(|| 42u8).join().unwrap_or(0);
            unsafe { libc::_exit(if joined == 42 { 0 } else { 5 }) }
        }
        let status = run_locked_child(body);
        assert!(!seccomp_unavailable(status), "seccomp filter could not be installed");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "fork()/thread spawn must keep working under the filter, got status {status:#x}"
        );
    }
}
