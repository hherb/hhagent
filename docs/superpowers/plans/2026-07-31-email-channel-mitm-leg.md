# Email channel MITM leg + `Mitm` posture type — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the email fallback channel's force-routed sidecar terminate and re-originate TLS — so a self-signed localmail becomes reachable and the leg becomes visible to the egress boundary — while collapsing `disable_mitm` + `upstream_extra_ca` into one `Mitm` type that makes the "anchor on a transparent tunnel" contradiction unrepresentable.

**Architecture:** Two behaviour-free layers then two behaviour changes. First a pure refactor (`Mitm` enum + `SidecarSpawn` params struct) across every sidecar call site, closing [#494](https://github.com/hherb/kastellan/issues/494). Then `spawn_net_transport` — the long-lived channel transport bring-up — gains a posture parameter and an `Intercept` arm that reuses the exact CA derivation `spawn_net_worker` already performs. Then the email channel opts into `Intercept` unconditionally, selecting its upstream anchor once at boot via #492's existing `upstream_ca_for`. Finally a hermetic MITM round-trip e2e proves the leg end to end.

**Tech Stack:** Rust 1.96, `kastellan-core` (`egress/`, `channel/email/`, `worker_lifecycle/force_route.rs`), `kastellan-tests-common` (`mock_localmail`, `tls_origin`), `kastellan-worker-egress-proxy`. No new dependencies.

**Spec:** [`docs/superpowers/specs/2026-07-31-email-channel-mitm-leg-design.md`](../specs/2026-07-31-email-channel-mitm-leg-design.md)

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** This plan adds none.
- **Cross-platform (Linux + macOS first-class).** Nothing here is `cfg`-gated; if you find yourself writing `#[cfg(target_os = ...)]`, stop and re-read the spec.
- **TDD.** Every step pair is "write the failing test" → "run it, watch it fail" → "implement" → "run it, watch it pass". Security-relevant assertions must be proved to fail against the un-hardened code (the #479 house rule).
- **Run cargo in the FOREGROUND.** Never background a `cargo test`/`clippy` and poll it; never pipe it through `tail` (masks the exit code).
- **Mac cargo needs a scratch target dir.** The IDE's rust-analyzer holds `target/debug/.cargo-lock`, so CLI cargo blocks. Export once per shell:
  ```sh
  source "$HOME/.cargo/env"
  export CARGO_TARGET_DIR=/private/tmp/claude-501/-Users-hherb-src-kastellan/f6ee3413-e0a8-4696-85f8-26041a82fedd/scratchpad/target
  ```
- **Commit with explicit paths.** `git add <files>` — never `git add -A` (untracked files in this repo must not be swept in).
- **Files under 500 lines where feasible.** `core/src/egress/spawn.rs` is 546 today; this change is roughly net-neutral there. If it passes ~600, lift its `mod tests` into `core/src/egress/spawn/tests.rs` (the pattern `net_worker.rs` / `force_route.rs` already use) as part of the same commit.
- **The DGX gate is OWED.** That host is offline. Do not claim it; Task 5 states it as outstanding in the PR body.

---

## File Structure

| File | Responsibility after this change |
| --- | --- |
| `core/src/egress/spawn.rs` (modify) | Owns `Mitm`, `SidecarSpawn`, `proxy_policy`, `spawn_sidecar`, `check_upstream_extra_ca`. The single definition of "what TLS posture is a sidecar in". |
| `core/src/egress/net_worker.rs` (modify) | `NetWorkerSpawn.mitm` replaces two fields; `spawn_net_worker` derives the worker's CA from the posture. |
| `core/src/egress/persistent_net.rs` (modify) | `NetTransportSpawn.mitm` + `worker_extra_ca`; pure `forced_transparent_policy` / `forced_intercept_policy`; `check_worker_extra_ca`. |
| `core/src/worker_lifecycle/force_route.rs` (modify) | New pure `mitm_for(worker_name, anchor)` — preserves the operator-facing refusal that today lives inline in `spawn_worker_maybe_forced`. |
| `core/src/channel/email/mod.rs` (modify) | Selects the upstream anchor once at boot; passes `Mitm::Intercept`. |
| `core/src/install/plan.rs` (modify) | `render_email_help` TRAP 3 rewritten — it currently documents the opposite of what will be true. |
| `core/tests/email_mitm_e2e.rs` (create) | Hermetic MITM round-trip for the channel + negative control. |

---

### Task 1: The `Mitm` posture type and `SidecarSpawn` params struct (#494)

Pure refactor, **atomic** — a signature change cannot be split into compiling halves. No behaviour changes; the existing suite is the check. Two assertions are *added* (Step 7) because extracting `mitm_for` gives an untested operator refusal its first test.

**Files:**
- Modify: `core/src/egress/spawn.rs` (`proxy_policy` 101-169, `check_upstream_extra_ca` 190-210, `spawn_sidecar` 245-338, `mod tests` 340-546)
- Modify: `core/src/egress/net_worker.rs:31-65` (struct), `:217-257` (spawn)
- Modify: `core/src/egress/net_worker/tests.rs` (5 `NetWorkerSpawn` literals)
- Modify: `core/src/egress/persistent_net.rs:85-96` (mechanical: `Mitm::Transparent`)
- Modify: `core/src/worker_lifecycle/force_route.rs:382-420`, `core/src/worker_lifecycle/force_route/tests.rs`
- Modify: `core/tests/egress_proxy_e2e.rs` (3 `spawn_sidecar` calls), `core/tests/egress_force_routing_e2e.rs` (3), `core/tests/browser_driver_e2e.rs` (1), `core/tests/mail_e2e.rs` (3), `core/tests/web_research_firecracker_broker_e2e.rs` (1)

**Interfaces:**
- Produces: `pub enum Mitm<'a> { Intercept { upstream_extra_ca: Option<&'a Path> }, Transparent }` with `pub(crate) fn is_transparent(&self) -> bool` and `pub(crate) fn upstream_extra_ca(&self) -> Option<&Path>`; `pub struct SidecarSpawn<'a> { binary, allowlist, scratch, worker, cert_pins_json, mitm, long_lived }`; `pub fn proxy_policy(spec: &SidecarSpawn<'_>) -> SandboxPolicy`; `pub fn spawn_sidecar(backend: &dyn SandboxBackend, spec: &SidecarSpawn<'_>) -> anyhow::Result<SidecarHandle>`; `pub(crate) fn mitm_for<'a>(worker_name: &str, upstream_extra_ca: Option<&'a Path>) -> Result<Mitm<'a>, String>`.
- Consumes: nothing new.

- [ ] **Step 1: Write the failing tests for the new API**

Replace the four `check_upstream_extra_ca_*` tests and add posture tests, in `core/src/egress/spawn.rs`'s `mod tests`:

```rust
/// A `SidecarSpawn` with everything defaulted except the posture, so each test
/// states only what it is about.
fn spec_with(mitm: Mitm<'_>) -> SidecarSpawn<'_> {
    SidecarSpawn {
        binary: Path::new("/opt/proxy"),
        allowlist: &[],
        scratch: Path::new("/scratch"),
        worker: "email",
        cert_pins_json: None,
        mitm,
        long_lived: false,
    }
}

#[test]
fn transparent_posture_sets_the_disable_mitm_env_and_no_anchor() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Transparent);
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    assert!(p.env.iter().any(|(k, v)| k == ENV_DISABLE_MITM && v == "1"));
    assert!(!p.env.iter().any(|(k, _)| k == ENV_UPSTREAM_EXTRA_CA));
}

#[test]
fn intercept_without_anchor_is_byte_identical_to_the_default_path() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    assert!(!p.env.iter().any(|(k, _)| k == ENV_DISABLE_MITM));
    assert!(!p.env.iter().any(|(k, _)| k == ENV_UPSTREAM_EXTRA_CA));
    assert!(!p.fs_read.iter().any(|f| f.ends_with("ca.pem")));
}

#[test]
fn intercept_with_anchor_sets_both_the_env_and_the_fs_read_bind() {
    let allow = vec!["10.0.0.3:8443".to_string()];
    let ca = PathBuf::from("/etc/kastellan/localmail.pem");
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: Some(&ca) });
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    // Both halves are load-bearing: the proxy READS the PEM before lock_down,
    // so the env key alone would name a path it cannot open.
    assert!(p.env.iter().any(|(k, v)| k == ENV_UPSTREAM_EXTRA_CA && v == ca.to_string_lossy()));
    assert!(p.fs_read.contains(&ca));
}

#[test]
fn check_upstream_extra_ca_rejects_a_relative_anchor() {
    let ca = PathBuf::from("relative/ca.pem");
    let err = check_upstream_extra_ca(Mitm::Intercept { upstream_extra_ca: Some(&ca) })
        .expect_err("relative path must fail");
    assert!(err.to_string().contains("absolute"), "unhelpful error: {err}");
}

#[test]
fn check_upstream_extra_ca_accepts_both_postures_without_an_anchor() {
    assert!(check_upstream_extra_ca(Mitm::Transparent).is_ok());
    assert!(check_upstream_extra_ca(Mitm::Intercept { upstream_extra_ca: None }).is_ok());
}
```

**Delete** `check_upstream_extra_ca_rejects_pairing_with_disable_mitm`. This is deliberate, not an oversight: the pair it asserted is no longer expressible — `Mitm::Transparent` has no field to hold an anchor. Record that in the deletion's commit message and in the `Mitm` doc comment (Step 3).

- [ ] **Step 2: Run the tests to verify they fail**

```sh
cargo test -p kastellan-core --lib egress::spawn
```
Expected: FAIL — `cannot find type Mitm in this scope`, `cannot find struct SidecarSpawn`.

- [ ] **Step 3: Add the types**

In `core/src/egress/spawn.rs`, above `proxy_policy`:

```rust
/// The TLS posture of a worker's egress sidecar.
///
/// One value rather than two fields, because the two are not independent: an
/// upstream trust anchor is meaningful ONLY on the re-origination leg, and that
/// leg exists only when the proxy terminates the worker's TLS. Before #494 this
/// was a `disable_mitm: bool` beside an `upstream_extra_ca: Option<&Path>`, and
/// the nonsensical pair (a tunnel handed an anchor it can never consult) had to
/// be rejected at runtime by [`check_upstream_extra_ca`]. That rule is not gone
/// — it moved into this type, where the pair cannot be written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mitm<'a> {
    /// The proxy terminates the worker's TLS and re-originates upstream. The
    /// worker trusts the sidecar's per-instance CA (exported beside the UDS);
    /// the sidecar validates the real origin with webpki plus
    /// `upstream_extra_ca` when the operator configured one (#491/#492).
    Intercept { upstream_extra_ca: Option<&'a Path> },
    /// The proxy relays ciphertext untouched; the worker validates the origin
    /// itself and never receives our CA. For workers that cannot be made to
    /// trust a per-instance CA — the browser (Chromium's NSS store) and
    /// matrix-sdk.
    Transparent,
}

impl<'a> Mitm<'a> {
    /// Whether the sidecar must be told to skip interception.
    pub(crate) fn is_transparent(&self) -> bool {
        matches!(self, Mitm::Transparent)
    }

    /// The upstream trust anchor, if this posture can use one. `Transparent`
    /// always yields `None` — structurally, not by convention.
    pub(crate) fn upstream_extra_ca(&self) -> Option<&'a Path> {
        match self {
            Mitm::Intercept { upstream_extra_ca } => *upstream_extra_ca,
            Mitm::Transparent => None,
        }
    }
}

/// Everything [`proxy_policy`] and [`spawn_sidecar`] need to describe one
/// sidecar. A struct rather than 8-9 positional arguments (#494): the old
/// signature had `disable_mitm` and `long_lived` adjacent as bare bools, and
/// transposing them compiled silently into both the wrong TLS posture and the
/// wrong CPU governance (the #395 SIGKILL shape).
pub struct SidecarSpawn<'a> {
    /// The egress-proxy binary.
    pub binary: &'a Path,
    /// `host:port` entries this sidecar may dial.
    pub allowlist: &'a [String],
    /// Per-worker scratch dir; the UDS and the exported CA live here.
    pub scratch: &'a Path,
    /// Worker name, for the proxy's decision rows.
    pub worker: &'a str,
    /// SPKI pin JSON (slice #4). Passed opaque; the proxy parses + enforces.
    pub cert_pins_json: Option<&'a str>,
    /// TLS posture — see [`Mitm`].
    pub mitm: Mitm<'a>,
    /// Lifetime-scoped CPU governance (issue #395). `true` for a channel
    /// sidecar that outlives many dispatches (no cumulative `RLIMIT_CPU`, which
    /// would eventually SIGKILL it mid-flight); `false` for a per-tool-call
    /// sidecar, which keeps the bounded cap as defense-in-depth.
    pub long_lived: bool,
}
```

- [ ] **Step 4: Rewrite the three functions**

`proxy_policy` becomes `pub fn proxy_policy(spec: &SidecarSpawn<'_>) -> SandboxPolicy`. Body changes only in these four places — everything else is a field access (`spec.allowlist`, `spec.scratch`, …):

```rust
    // Omit the disable-MITM key entirely when intercepting so the MITM path is
    // byte-identical to the pre-#494 default (mirrors the pins pattern).
    if spec.mitm.is_transparent() {
        env.push((ENV_DISABLE_MITM.to_string(), "1".to_string()));
    }
    if let Some(ca) = spec.mitm.upstream_extra_ca() {
        env.push((ENV_UPSTREAM_EXTRA_CA.to_string(), ca.to_string_lossy().into_owned()));
    }
    // …
    if let Some(ca) = spec.mitm.upstream_extra_ca() {
        fs_read.push(ca.to_path_buf());
    }
    // …
    cpu_ms: if spec.long_lived { 0 } else { SHORT_LIVED_SIDECAR_CPU_MS },
```

Delete `#[allow(clippy::too_many_arguments)]` from both functions.

`check_upstream_extra_ca` loses its second rule and its first parameter:

```rust
/// Reject an anchor that cannot do what the caller intends, before anything is
/// spawned: a **relative** path. The CA is bound into the proxy jail via
/// `SandboxPolicy.fs_read`, and both backends reject relative `fs_read` entries
/// — so the failure would name the sandbox rather than the misconfigured field.
/// (A *nonexistent* absolute path is deliberately NOT rejected: `canonicalize_one`
/// tolerates `NotFound` and the Linux bind is `--ro-bind-try`, leaving the proxy
/// — the authority on the PEM's content — to fail closed on it at startup.)
///
/// The old second rule ("never paired with a transparent tunnel") is gone
/// because [`Mitm`] makes that pair unrepresentable.
fn check_upstream_extra_ca(mitm: Mitm<'_>) -> anyhow::Result<()> {
    let Some(ca) = mitm.upstream_extra_ca() else {
        return Ok(());
    };
    if !ca.is_absolute() {
        anyhow::bail!(
            "upstream extra CA path must be absolute (it is bound into the proxy jail via \
             fs_read, which rejects relative paths): {ca:?}"
        );
    }
    Ok(())
}
```

`spawn_sidecar` becomes `pub fn spawn_sidecar(backend: &dyn SandboxBackend, spec: &SidecarSpawn<'_>) -> anyhow::Result<SidecarHandle>`; its first lines become `check_upstream_extra_ca(spec.mitm)?;`, the WARN reads `spec.mitm.upstream_extra_ca()` and logs `worker = spec.worker`, `let policy = proxy_policy(spec);`, and `let uds_path = spec.scratch.join(UDS_FILE_NAME);`. Drop its `#[allow(clippy::too_many_arguments)]`.

- [ ] **Step 5: Migrate `NetWorkerSpawn` and `spawn_net_worker`**

In `core/src/egress/net_worker.rs`, replace the `disable_mitm` and `upstream_extra_ca` fields with one, keeping the trust-scope prose from the old `upstream_extra_ca` doc:

```rust
    /// This worker's sidecar TLS posture. `Mitm::Transparent` for a worker that
    /// does its own end-to-end TLS and cannot trust our CA (the browser);
    /// `Mitm::Intercept { upstream_extra_ca }` otherwise, where the optional
    /// anchor is trusted for every host THIS sidecar may reach — so it suits a
    /// single-origin worker. See `egress-proxy::pins::build_upstream_client_config`
    /// for the trust-scope and `CA:FALSE` constraints.
    pub mitm: Mitm<'a>,
```

In `spawn_net_worker`, the sidecar call and the CA derivation become:

```rust
    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        &SidecarSpawn {
            binary: params.proxy_bin,
            allowlist: params.allowlist,
            scratch,
            worker: params.worker_name,
            cert_pins_json: params.cert_pins_json,
            mitm: params.mitm,
            long_lived: false, // 1:1 with a single tool-call dispatch (issue #395)
        },
    )
    .map_err(|e| ToolHostError::Io(std::io::Error::other(format!("egress sidecar: {e}"))))?;
```

```rust
    // The sidecar exports its CA next to the UDS (same scratch dir). An
    // intercepting worker trusts it; a transparent-tunnel worker gets `None`.
    let ca = uds
        .parent()
        .map(|d| d.join(super::spawn::CA_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(super::spawn::CA_FILE_NAME));
    let ca = (!params.mitm.is_transparent()).then_some(ca.as_path());
```

- [ ] **Step 6: Migrate `persistent_net.rs` mechanically**

Only the sidecar call changes in this task — the posture stays hardcoded transparent, because giving it a caller-chosen posture is Task 2:

```rust
    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        &SidecarSpawn {
            binary: params.proxy_bin,
            allowlist: params.allowlist,
            scratch,
            worker: params.worker_name,
            cert_pins_json: None,
            mitm: Mitm::Transparent,
            long_lived: true, // a channel sidecar outlives many dispatches (#395)
        },
    )?;
```

- [ ] **Step 7: Extract `mitm_for` in `force_route.rs`, with its first tests**

`spawn_worker_maybe_forced` currently decides the posture inline and refuses an anchor handed to a transparent worker (lines 389-403) — an operator-facing rule with **no unit test**. Extract it so the refactor cannot quietly drop it. Add to `core/src/worker_lifecycle/force_route.rs`:

```rust
/// Pure: this worker's sidecar posture, given the anchor #492's selector picked
/// for its allowlist.
///
/// Refuses — rather than silently dropping the anchor — when the operator
/// configured one for a worker whose sidecar transparently tunnels. `Mitm`
/// makes that pair unrepresentable *downstream*, but the operator still needs
/// to be told their config line does nothing, and told it in terms of the env
/// var and the worker name rather than an internal field.
pub(crate) fn mitm_for<'a>(
    worker_name: &str,
    upstream_extra_ca: Option<&'a Path>,
) -> Result<Mitm<'a>, String> {
    if disable_mitm_for(worker_name) {
        if upstream_extra_ca.is_some() {
            return Err(format!(
                "worker {worker_name:?}: refusing to spawn — {ENV_UPSTREAM_EXTRA_CA} names an \
                 extra trust anchor for this worker's origin, but this worker's sidecar runs in \
                 transparent-tunnel (no-MITM) mode and never re-originates TLS, so the anchor \
                 would be silently inert. Remove that entry."
            ));
        }
        return Ok(Mitm::Transparent);
    }
    Ok(Mitm::Intercept { upstream_extra_ca })
}
```

Call site (replacing lines 387-403 and the two struct fields):

```rust
            let mitm = mitm_for(worker_name, upstream_extra_ca)
                .map_err(|msg| ToolHostError::Io(std::io::Error::other(msg)))?;
            let params = crate::egress::net_worker::NetWorkerSpawn {
                // …unchanged fields…
                cert_pins_json: pins_json.as_deref(),
                mitm,
            };
```

Add to `core/src/worker_lifecycle/force_route/tests.rs`:

```rust
#[test]
fn mitm_for_gives_transparent_workers_no_anchor_and_refuses_a_configured_one() {
    let ca = std::path::PathBuf::from("/etc/kastellan/localmail.pem");
    assert_eq!(mitm_for(MATRIX_TOOL, None).unwrap(), Mitm::Transparent);
    assert_eq!(mitm_for(BROWSER_DRIVER_TOOL, None).unwrap(), Mitm::Transparent);
    let err = mitm_for(MATRIX_TOOL, Some(&ca)).expect_err("anchor on a tunnel must refuse");
    assert!(err.contains("silently inert"), "unhelpful error: {err}");
    assert!(err.contains(MATRIX_TOOL), "error must name the worker: {err}");
}

#[test]
fn mitm_for_gives_intercepting_workers_the_selected_anchor() {
    let ca = std::path::PathBuf::from("/etc/kastellan/localmail.pem");
    assert_eq!(
        mitm_for("mail", Some(&ca)).unwrap(),
        Mitm::Intercept { upstream_extra_ca: Some(ca.as_path()) }
    );
    assert_eq!(
        mitm_for("web-fetch", None).unwrap(),
        Mitm::Intercept { upstream_extra_ca: None }
    );
}
```

- [ ] **Step 8: Migrate the remaining call sites**

Mechanical. In each, replace `disable_mitm: <b>, upstream_extra_ca: <a>` with `mitm: Mitm::Transparent` (when the old bool was `true`) or `mitm: Mitm::Intercept { upstream_extra_ca: <a> }` (when it was `false`), and each positional `spawn_sidecar(...)` with the struct form:

- `core/src/egress/net_worker/tests.rs` — 5 literals
- `core/tests/egress_proxy_e2e.rs` — 3 `spawn_sidecar` calls
- `core/tests/egress_force_routing_e2e.rs` — 3 literals
- `core/tests/browser_driver_e2e.rs` — 1 literal (browser ⇒ `Mitm::Transparent`)
- `core/tests/mail_e2e.rs` — 3 literals (mail ⇒ `Intercept`; the round-trip test's anchor moves into the variant)
- `core/tests/web_research_firecracker_broker_e2e.rs` — 1 literal

Export the new names from `core/src/egress/mod.rs` if the tests import them via `kastellan_core::egress::{...}` — check the existing re-exports rather than assuming.

- [ ] **Step 9: Run the full workspace + clippy**

```sh
cargo test --workspace -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: PASS, with the same counts as `main` **minus one** (the deleted pairing test) **plus four** (Step 1's two new posture tests beyond the two it replaced, and Step 7's two). Record the exact numbers — Task 5 needs the delta.

- [ ] **Step 10: Commit**

```bash
git add core/src/egress/spawn.rs core/src/egress/net_worker.rs core/src/egress/net_worker/tests.rs \
        core/src/egress/persistent_net.rs core/src/worker_lifecycle/force_route.rs \
        core/src/worker_lifecycle/force_route/tests.rs core/src/egress/mod.rs \
        core/tests/egress_proxy_e2e.rs core/tests/egress_force_routing_e2e.rs \
        core/tests/browser_driver_e2e.rs core/tests/mail_e2e.rs \
        core/tests/web_research_firecracker_broker_e2e.rs
git commit -m "refactor(egress): one Mitm posture type + SidecarSpawn params struct (closes #494)"
```

---

### Task 2: `spawn_net_transport` gains a posture

Still no behaviour change: every caller passes `Mitm::Transparent`, so the bytes are identical to Task 1. This task only makes the posture *expressible* on the channel-transport path, and adds the pure `Intercept` arm plus its guard.

**Files:**
- Modify: `core/src/egress/persistent_net.rs` (struct 29-44, `forced_transparent_policy` 20-22, `spawn_net_transport` 79-125, `mod tests` 127-148)
- Modify: `core/src/channel/matrix.rs:296`, `core/src/channel/email/mod.rs:223`, `core/tests/net_demo_egress_e2e.rs:149`, `core/tests/net_demo_firecracker_egress_e2e.rs:170`, `core/tests/matrix_firecracker_live_e2e.rs:233`

**Interfaces:**
- Consumes: `Mitm`, `SidecarSpawn` from Task 1.
- Produces: `NetTransportSpawn { …, pub mitm: Mitm<'a>, pub worker_extra_ca: Option<&'a Path> }` (the field formerly named `extra_ca`); `pub(crate) fn forced_intercept_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy`; `fn check_worker_extra_ca(mitm: Mitm<'_>, worker_extra_ca: Option<&Path>) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing tests**

In `core/src/egress/persistent_net.rs`'s `mod tests` (keep the existing transparent test as-is):

```rust
#[test]
fn forced_intercept_policy_gives_the_worker_the_sidecar_ca() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["10.0.0.3:8443".into()]),
        fs_read: vec!["/etc/resolv.conf".into(), "/bin/email-in".into()],
        ..SandboxPolicy::default()
    };
    let uds = std::path::PathBuf::from("/scratch/egress-1/egress.sock");
    let out = forced_intercept_policy(base, &uds);
    let ca = std::path::PathBuf::from("/scratch/egress-1/ca.pem");
    // Announced AND readable in-jail — the worker's transport opens the path it
    // is handed, so either half alone is useless.
    assert!(out.env.iter().any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_CA"
        && v == "/scratch/egress-1/ca.pem"));
    assert!(out.fs_read.contains(&ca));
    assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
    // The proxy resolves DNS now, in either posture.
    assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
    assert!(out.fs_read.contains(&"/bin/email-in".into()));
}

#[test]
fn a_worker_side_origin_ca_is_refused_under_interception() {
    let ca = std::path::PathBuf::from("/tmp/origin-ca.pem");
    let err = check_worker_extra_ca(
        Mitm::Intercept { upstream_extra_ca: None },
        Some(&ca),
    )
    .expect_err("worker-side origin CA under MITM must be refused");
    assert!(err.to_string().contains("inert"), "unhelpful error: {err}");
}

#[test]
fn a_worker_side_origin_ca_is_accepted_under_a_transparent_tunnel() {
    let ca = std::path::PathBuf::from("/tmp/origin-ca.pem");
    assert!(check_worker_extra_ca(Mitm::Transparent, Some(&ca)).is_ok());
    assert!(check_worker_extra_ca(Mitm::Transparent, None).is_ok());
    assert!(check_worker_extra_ca(Mitm::Intercept { upstream_extra_ca: None }, None).is_ok());
}
```

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test -p kastellan-core --lib egress::persistent_net
```
Expected: FAIL — `cannot find function forced_intercept_policy`, `cannot find function check_worker_extra_ca`.

- [ ] **Step 3: Implement the two pure functions**

In `core/src/egress/persistent_net.rs`:

```rust
/// Rewrite `base` for MITM force-routing onto `uds`: proxy_uds set, resolv.conf
/// dropped, UDS env injected, AND the sidecar's per-instance CA made readable +
/// announced, so the worker's transport trusts the proxy instead of the origin.
/// The CA lives beside the UDS — the sidecar writes it there before it reports
/// ready, so the path is valid by the time any worker is spawned.
pub(crate) fn forced_intercept_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    let ca = uds
        .parent()
        .map(|d| d.join(super::spawn::CA_FILE_NAME))
        .unwrap_or_else(|| std::path::PathBuf::from(super::spawn::CA_FILE_NAME));
    rewrite_worker_policy(base, uds, Some(ca.as_path()))
}

/// Reject a worker-side origin CA handed to an intercepting transport.
///
/// Under interception the worker's transport (`web-common::http::make_get`)
/// trusts `KASTELLAN_EGRESS_PROXY_CA` and nothing else, so an origin anchor
/// given to the worker would be **silently inert** — the same false-belief
/// failure mode #491 exists to correct, one layer over. `worker_extra_ca` is
/// meaningful only for a transparent tunnel, where the worker does validate the
/// origin itself.
fn check_worker_extra_ca(mitm: Mitm<'_>, worker_extra_ca: Option<&Path>) -> anyhow::Result<()> {
    if worker_extra_ca.is_some() && !mitm.is_transparent() {
        anyhow::bail!(
            "worker_extra_ca was given to an intercepting transport, whose worker trusts only \
             the sidecar's per-instance CA — the origin anchor would be inert. Use \
             Mitm::Intercept {{ upstream_extra_ca }} to widen the SIDECAR's upstream trust instead"
        );
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify they pass**

```sh
cargo test -p kastellan-core --lib egress::persistent_net
```
Expected: PASS (4 tests: the pre-existing transparent one plus the three new).

- [ ] **Step 5: Wire the posture through `NetTransportSpawn`**

Rename the field and add the posture:

```rust
    /// This transport's sidecar TLS posture — see [`super::spawn::Mitm`].
    /// Channel callers choose: the email channel intercepts (so the operator's
    /// upstream anchor reaches a self-signed private origin, and the leg is
    /// visible to the leak scanner); Matrix tunnels transparently, because
    /// matrix-sdk terminates its own TLS through `ProxyBridge` and cannot be
    /// made to trust a per-instance CA.
    pub mitm: Mitm<'a>,
    /// A **worker-side** origin cert, appended to `fs_read` so a VM RO-share
    /// carries it in-guest. Test-only today; `None` in production. Meaningful
    /// only under [`Mitm::Transparent`] — pairing it with `Intercept` is
    /// refused by `check_worker_extra_ca`. Note this is the opposite side of
    /// the connection from `Mitm::Intercept`'s `upstream_extra_ca`, which
    /// widens the SIDECAR's trust; the old name `extra_ca` invited exactly that
    /// confusion.
    pub worker_extra_ca: Option<&'a Path>,
```

In `spawn_net_transport`, guard first, then thread the posture through both halves:

```rust
    // Fail before anything is spawned.
    check_worker_extra_ca(params.mitm, params.worker_extra_ca)?;

    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        &SidecarSpawn {
            binary: params.proxy_bin,
            allowlist: params.allowlist,
            scratch,
            worker: params.worker_name,
            cert_pins_json: None,
            mitm: params.mitm,
            long_lived: true, // a channel sidecar outlives many dispatches (#395)
        },
    )?;
```

```rust
    let mut base = params.base_policy.clone();
    if let Some(ca) = params.worker_extra_ca {
        if !base.fs_read.iter().any(|p| p == ca) {
            base.fs_read.push(ca.to_path_buf());
        }
    }
    let forced = match params.mitm {
        Mitm::Transparent => forced_transparent_policy(base, &uds),
        Mitm::Intercept { .. } => forced_intercept_policy(base, &uds),
    };
```

Update the module doc: it currently asserts "The sidecar runs in `disable_mitm` mode; the worker does its own end-to-end TLS and receives no CA" — that becomes the *caller's* choice. Update `spawn_net_transport`'s own doc comment the same way.

- [ ] **Step 6: Update the five call sites to `Mitm::Transparent`**

`core/src/channel/matrix.rs:296` — add, with the reason, so nobody "fixes" the asymmetry with email later:

```rust
                    // matrix-sdk terminates its own TLS through `ProxyBridge`
                    // and cannot be made to trust a per-instance CA, so this
                    // sidecar tunnels rather than intercepts. Unlike the email
                    // channel, which intercepts (see channel::email).
                    mitm: Mitm::Transparent,
                    worker_extra_ca: None,
```

`core/src/channel/email/mod.rs:223` — **temporarily** `mitm: Mitm::Transparent, worker_extra_ca: None`. Task 3 flips it; keeping this task behaviour-free is the point.

The three e2e files (`net_demo_egress_e2e.rs:149`, `net_demo_firecracker_egress_e2e.rs:170`, `matrix_firecracker_live_e2e.rs:233`) — `mitm: Mitm::Transparent` and rename their existing `extra_ca:` field to `worker_extra_ca:` (value unchanged).

- [ ] **Step 7: Run the full workspace + clippy**

```sh
cargo test --workspace -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: PASS, +3 tests over Task 1.

- [ ] **Step 8: Commit**

```bash
git add core/src/egress/persistent_net.rs core/src/channel/matrix.rs core/src/channel/email/mod.rs \
        core/tests/net_demo_egress_e2e.rs core/tests/net_demo_firecracker_egress_e2e.rs \
        core/tests/matrix_firecracker_live_e2e.rs
git commit -m "refactor(egress): give spawn_net_transport a caller-chosen TLS posture"
```

---

### Task 3: The email channel intercepts

The behaviour change. After this the channel can reach a self-signed localmail, and its leg is MITM-visible.

**Files:**
- Modify: `core/src/channel/email/mod.rs` (`spawn_email_worker` doc 142-172, anchor selection before the factory ~204, the `NetTransportSpawn` literal ~223)
- Modify: `core/src/install/plan.rs` (`render_email_help` TRAP 3, doc lines 191-203, and the env-block body if it repeats the claim)
- Test: `core/src/channel/email/mod.rs`'s test module (or `core/tests/email_channel_e2e.rs` if the boot path is exercised there — check which before writing)

**Interfaces:**
- Consumes: `NetTransportSpawn.mitm` (Task 2), `ForceRoutingConfig::upstream_ca_for` (existing, `pub(crate)`).
- Produces: no new public API.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn email_transport_intercepts_and_carries_the_selected_anchor() {
    // A config naming this channel's own origin yields an intercepting posture
    // holding that anchor — the whole point of the slice.
    let map = crate::egress::upstream_ca::parse_upstream_cas(
        r#"{"10.0.0.3":"/etc/kastellan/localmail.pem"}"#,
    )
    .expect("valid config");
    let allowlist = vec!["10.0.0.3:8443".to_string()];
    let selected = crate::egress::upstream_ca::select_ca_for_allowlist(&map, &allowlist)
        .expect("single private origin selects cleanly");
    assert_eq!(selected, Some(std::path::Path::new("/etc/kastellan/localmail.pem")));
}

#[test]
fn email_anchor_selection_refuses_a_mixed_allowlist() {
    // The channel must fail its own bring-up rather than widen trust to a second
    // host, and it must do so at boot, not inside the respawn loop.
    let map = crate::egress::upstream_ca::parse_upstream_cas(
        r#"{"10.0.0.3":"/etc/kastellan/localmail.pem"}"#,
    )
    .expect("valid config");
    let allowlist = vec!["10.0.0.3:8443".to_string(), "smtp.example.com:587".to_string()];
    assert!(crate::egress::upstream_ca::select_ca_for_allowlist(&map, &allowlist).is_err());
}
```

If `upstream_ca`'s items are `pub(crate)` only within `egress`, add these to `core/src/channel/email/mod.rs`'s own `mod tests`; if visibility blocks it, widen to `pub(crate)` rather than duplicating the logic.

- [ ] **Step 2: Run to verify they fail or reveal the visibility gap**

```sh
cargo test -p kastellan-core --lib channel::email
```
Expected: FAIL — either an unresolved path (visibility) or a missing import.

- [ ] **Step 3: Select the anchor once, at boot**

In `spawn_email_worker`, immediately after `let allowlist = vec![format!("{host}:{port}")];` and **before** the factory closure:

```rust
    // #492's selector, reused verbatim so the channel inherits the
    // single-private-origin rule: exactly one configured origin, and it must be
    // the only host this worker can dial. Selected HERE rather than inside the
    // factory so a configuration disagreement disables the email channel once,
    // loudly, at startup — instead of failing forever inside the supervisor's
    // respawn backoff. Owned so the closure holds no borrow into the Arc.
    let upstream_extra_ca: Option<PathBuf> = match &egress {
        Some(eg) => eg
            .routing
            .upstream_ca_for(&allowlist)
            .map_err(|e| {
                anyhow::anyhow!(
                    "email channel: refusing to start — KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA: {e}"
                )
            })?
            .map(|p| p.to_path_buf()),
        None => None,
    };
```

- [ ] **Step 4: Flip the posture**

In the factory's `NetTransportSpawn` literal:

```rust
                // The email channel ALWAYS intercepts: the sidecar terminates
                // the worker's TLS and re-originates upstream, so an operator
                // anchor (when configured) reaches a self-signed localmail and
                // the leg is visible to the #3b leak scanner. With no anchor the
                // upstream leg is plain webpki — the same posture every
                // force-routed tool worker has.
                mitm: Mitm::Intercept {
                    upstream_extra_ca: upstream_extra_ca.as_deref(),
                },
                worker_extra_ca: None,
```

Update `spawn_email_worker`'s doc comment: the paragraph claiming the sidecar "is a TRANSPARENT tunnel … it does not by itself solve TLS to a self-signed localmail origin (that needs the MITM + upstream-extra-CA seam, #492, which is not wired into this path)" is now false and must be replaced, not softened.

- [ ] **Step 5: Rewrite `render_email_help` TRAP 3**

Both the doc comment (lines 191-203) and any matching text inside the returned env block. Replacement:

```
/// 3. **A self-signed localmail needs `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`.**
///    This channel's force-routed sidecar terminates the worker's TLS and
///    re-originates upstream, so the operator anchor named for this origin is
///    what lets it validate a self-signed cert. Two constraints come with it,
///    both inherited from #492: the cert must be a real CA that signed the
///    origin leaf **or** a self-signed leaf with `basicConstraints CA:FALSE`
///    (a `CA:TRUE` self-signed leaf is rejected at handshake time by rustls as
///    `CaUsedAsEndEntity`, even though `openssl verify` accepts it — and
///    `openssl req -x509` commonly produces exactly that shape); and the
///    anchor is trusted for every host that sidecar can reach, so this
///    worker's allowlist must resolve to that single private origin. Verify a
///    cert's shape with:
///      openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'
```

- [ ] **Step 6: Run the email suites, then the workspace**

```sh
cargo test -p kastellan-core --lib channel::email
cargo test -p kastellan-core --test email_channel_e2e
cargo test --workspace -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: PASS throughout. The hermetic `email_channel_e2e` uses a fixture worker with no sandbox, so the posture flip must not perturb it — if it does, the flip has leaked into the non-force-routed path and that is a bug in Step 4, not a test to update.

- [ ] **Step 7: Commit**

```bash
git add core/src/channel/email/mod.rs core/src/install/plan.rs
git commit -m "feat(channel): the email channel's sidecar intercepts TLS"
```

---

### Task 4: Hermetic MITM round-trip e2e for the channel

The load-bearing proof. Mirrors `core/tests/mail_e2e.rs::force_routed_search_round_trips_through_mitm_sidecar`, which does exactly this for the mail tool.

**Files:**
- Create: `core/tests/email_mitm_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_tests_common::mock_localmail::spawn_mock_localmail_tls()` → `(MockLocalmail, String)` where the `String` is the origin's cert PEM and the mock already serves `GET /v1/changes` + `POST /v1/changes/ack`; `kastellan_tests_common::{egress_proxy_bin_or_skip, short_scratch_root, workspace_target_binary}`; `spawn_email_worker` with a `Some(EmailEgress)`.

- [ ] **Step 1: Write the failing positive test**

Read `core/tests/mail_e2e.rs:400-500` first and follow its structure — it already solves the skip-gating, the scratch-root shortening, and the decision-quiescence polling. The new test:

```rust
/// Hermetic full round-trip: the REAL email-in worker, force-routed in MITM
/// mode, polls a self-signed HTTPS localmail mock; the proxy MITM-terminates
/// and re-originates TLS validated against the operator-provided upstream extra
/// CA. This is the leg that did not exist before — slice 1's sidecar was a
/// transparent tunnel, so a self-signed origin was unreachable by this channel.
///
/// The load-bearing assertion is the polled event round-tripping: bytes only
/// reach the worker if the proxy's upstream handshake against the self-signed
/// origin validated. `tls_intercepted: true` is asserted too, but it is weaker
/// than it reads — the proxy emits that decision when it takes the MITM branch,
/// BEFORE the upstream handshake — so on its own it proves only "not
/// transparently tunnelled".
#[test]
fn force_routed_email_poll_round_trips_through_mitm_sidecar() { /* … */ }
```

Structure: write `cert_pem` to `<scratch>/localmail-ca.pem`; build an `EmailEgress` whose `ForceRoutingConfig` carries `with_upstream_cas(Some(parse_upstream_cas(&format!(r#"{{"127.0.0.1":"{}"}}"#, ca_path.display()))?))`; point `KASTELLAN_EMAIL_ENDPOINT` at `https://127.0.0.1:<mock port>`; assert an inbound event arrives **and** a captured `egress.allowed` row carries `tls_intercepted == true`.

- [ ] **Step 2: Run to verify it fails**

```sh
cargo test -p kastellan-core --test email_mitm_e2e -- --nocapture
```
Expected: FAIL (compile error first, then a real assertion failure). If it *skips*, the skip gate is wrong — check `egress_proxy_bin_or_skip` and that `cargo build --workspace` produced `kastellan-worker-email-in`.

- [ ] **Step 3: Make it pass**

No production code should be needed — Tasks 2 and 3 supply the behaviour. If the test cannot pass without a production change, that change belongs in Task 2 or 3 and this is telling you those tasks are incomplete; go back rather than patching here.

- [ ] **Step 4: Add the negative control**

```rust
/// Without the operator anchor the same round-trip must FAIL, and fail on the
/// upstream handshake — proving the anchor is load-bearing rather than
/// incidental. Asserts both the error and a `mitm_failed: …` egress decision.
#[test]
fn without_the_operator_anchor_the_mitm_leg_fails_closed() { /* … */ }
```

Same harness with `upstream_cas: None`. Poll decisions to quiescence before asserting — the ingest thread is detached and races teardown, worst for a connection's *last* decision (#491 trap 4).

- [ ] **Step 5: Run both**

```sh
cargo test -p kastellan-core --test email_mitm_e2e -- --nocapture
```
Expected: PASS, 2 tests, no `[SKIP]` on a Mac with the proxy binary built.

- [ ] **Step 6: Commit**

```bash
git add core/tests/email_mitm_e2e.rs
git commit -m "test(channel): hermetic MITM round-trip for the email channel + negative control"
```

---

### Task 5: Docs, gate, PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Retire the KNOWN GAP in both documents**

HANDOVER: the `KNOWN GAP — the email channel has no MITM leg` paragraph and the `Current state` sentence that references it. ROADMAP: the email entry's `KNOWN GAP (open — slice 3 …)` block, and move the MITM half out of "Slice 3" (leaving the DGX deploy + live tier). Add the merge bullet with the commit hashes and the measured counts.

- [ ] **Step 2: Run the full Mac gate**

```sh
cargo test --workspace -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Record: passed / failed / ignored, and the `[SKIP]` count.

- [ ] **Step 3: Run the PG-gated suites separately**

A full-workspace run under the PG override flakes `embedding_recall_e2e` at bring-up (the standing macOS gotcha), so these run on their own:

```sh
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test channel_bus_pg_e2e -- --nocapture
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-db --test pairings_e2e -- --nocapture
```

- [ ] **Step 4: Commit the docs and open the PR**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: retire the email-channel MITM gap; record the new baseline"
git push -u origin <branch>
```

PR body must state: closes #494; closes the email-channel MITM gap; the Mac counts from Steps 2-3; and — explicitly — that **the DGX gate is owed and this must not merge until it is green**, because the sandbox tiers skip-as-pass on the Mac and macOS clippy compiles `cfg(target_os = "linux")` items out.

---

## Self-Review

**Spec coverage.** §3 `Mitm` type → Task 1 Steps 1-4. §4 `SidecarSpawn` → Task 1 Steps 3-5, 8. §5 `spawn_net_transport` → Task 2. §6 email wiring + boot-time selection → Task 3 Steps 3-4. §7 the `worker_extra_ca` pairing check → Task 2 Steps 1-3. §8 tests: unit 1-4 → Task 1 Step 1; unit 5 → Task 2 Step 1; unit 6 → Task 2 Step 1; unit 7 → Task 3 Step 1 plus the existing `email_channel_e2e`; hermetic e2e → Task 4. §9 docs → Task 3 Step 5 (TRAP 3), Task 2 Step 5 (module docs), Task 5 Step 1 (HANDOVER/ROADMAP). §10 gate → Task 5 Steps 2-4. **One item the spec did not name and this plan adds: `force_route.rs`'s operator-facing "anchor on a transparent worker" refusal**, which exists in code today with no test and would have been easy to drop in the refactor — Task 1 Step 7 extracts it as `mitm_for` and tests it. That is a strict addition, not a deviation.

**Placeholders.** Task 4 Steps 1 and 4 give the test docs, names, harness, and assertions but not a full literal body, because it must follow `mail_e2e.rs`'s existing structure (skip gates, `short_scratch_root`, quiescence polling) rather than a body invented here — the step says which function to read first and names every helper by exact signature. Everything else contains the literal code.

**Type consistency.** `Mitm` / `SidecarSpawn` / `mitm_for` / `forced_intercept_policy` / `check_worker_extra_ca` / `worker_extra_ca` are spelled identically in every task. `check_upstream_extra_ca` keeps its name but changes arity (Task 1 Step 4), and no later task calls the old form. `NetWorkerSpawn.mitm` (Task 1) and `NetTransportSpawn.mitm` (Task 2) are distinct structs sharing one field name deliberately.
