# Full force-routed mail round-trip e2e (#491) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove a real `mail.search` round-trips through the force-routed MITM egress
sidecar to a self-signed localmail, by adding an operator-provided upstream extra-CA seam
to the egress proxy and exercising it with a hermetic e2e (+ negative control) and a live
DGX tier.

**Architecture:** The egress-proxy's re-origination (upstream) leg trusts webpki roots
only. We add an optional operator-provided extra trust anchor for that leg, wired through
the same rails as `cert_pins_json`/`ENV_PINS` (`build_upstream_client_config` →
`KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA` → `proxy_policy`/`spawn_sidecar` →
`NetWorkerSpawn.upstream_extra_ca`). Off by default ⇒ byte-identical to today;
fail-closed. Tests pass the CA directly via `NetWorkerSpawn`; production `force_route.rs`
stays `None` (prod wiring deferred).

**Tech Stack:** Rust (rustls 0.23, rcgen 0.13, tokio, hyper), the workspace's egress-proxy
+ sandbox + tests-common crates. Dev host macOS (Seatbelt); DGX Spark (aarch64) over
`ssh dgx '<cmd>'` for real bwrap/KVM/PG Linux acceptance.

## Global Constraints

- **AGPL-compatible deps only** (Apache/MIT/BSD/MPL/LGPL/(A)GPL). No new non-compatible dep.
- **Cross-platform Linux + macOS first-class.** No host-only code without an equivalent counterpart.
- **Rust core; Python only inside sandboxed workers.** No in-process untrusted code.
- **Every worker is sandboxed before it runs.** No "spawn unsandboxed" escape hatch.
- **Unset env ⇒ byte-identical behaviour.** The extra-CA seam adds nothing to the wire/policy when `None`.
- **Fail-closed.** A set-but-unreadable/invalid/zero-cert extra CA aborts proxy startup — never silently degrade to webpki-only.
- **TDD.** Failing test first, minimal impl, green, commit. **All tests pass before commit** (unless the operator grants an exception).
- **Inline docs understandable to a junior contributor** on every new fn.
- **Files ≤ ~500 LOC where feasible** (none here should approach it).
- **Cargo not on the non-interactive PATH:** every shell task starts with `source "$HOME/.cargo/env"`.
- **DGX is authoritative** for `cfg(target_os="linux")` code, the e2e tiers, and the full `cargo test --workspace` + `clippy --workspace --all-targets -D warnings`, **0 `[SKIP]`**. Drive it as exactly `ssh dgx '<cmd>'` (flags before the hostname are denied). Never write DGX run logs to `/tmp` (scrubbed mid-run) — use `~`.
- **Prove the negative case:** each security assertion must be shown to fail against un-hardened/un-seamed code before it is trusted.

---

## File Structure

| File | Responsibility |
|---|---|
| `workers/egress-proxy/src/pins.rs` | pure `add_extra_ca_pem` helper + `build_upstream_client_config` gains `extra_ca_path` |
| `workers/egress-proxy/src/pins/tests.rs` | extra-CA unit tests |
| `workers/egress-proxy/src/main.rs` | read `KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA`, startup WARN |
| `core/src/egress/spawn.rs` | `ENV_UPSTREAM_EXTRA_CA`, `proxy_policy`/`spawn_sidecar` param, fs_read + env, unit tests |
| `core/src/egress/net_worker.rs` | `NetWorkerSpawn.upstream_extra_ca`, thread into `spawn_sidecar` |
| `core/src/egress/net_worker/tests.rs`, `core/src/worker_lifecycle/force_route.rs`, `core/src/egress/persistent_net.rs`, and 4 `core/tests/*_e2e.rs` | mechanical `upstream_extra_ca: None` / append `None` arg |
| `tests-common/src/tls_origin.rs` | factor shared `generate_loopback_cert()` |
| `tests-common/src/mock_localmail.rs` | extract `serve_localmail_conn` + `spawn_mock_localmail_tls` |
| `core/tests/mail_e2e.rs` | hermetic round-trip + negative control + live `#[ignore]` DGX tier |

---

## Task 1: Proxy upstream extra-CA capability (pins.rs + main.rs)

**Files:**
- Modify: `workers/egress-proxy/src/pins.rs`
- Modify: `workers/egress-proxy/src/pins/tests.rs`
- Modify: `workers/egress-proxy/src/main.rs:90-98`

**Interfaces:**
- Produces: `pub(crate) fn add_extra_ca_pem(roots: &mut rustls::RootCertStore, pem: &[u8]) -> Result<(), PinError>`
- Produces: `pub fn build_upstream_client_config(pins_env: Option<&str>, extra_ca_path: Option<&std::path::Path>) -> Result<Arc<rustls::ClientConfig>, PinError>` (the second param is new)
- Produces: env `KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA` (a path) read by the proxy binary.

- [ ] **Step 1: Write the failing unit tests** in `workers/egress-proxy/src/pins/tests.rs` (append):

```rust
#[test]
fn add_extra_ca_pem_adds_a_valid_cert() {
    // A fresh CA PEM (reusing the proxy's own CA generator) is a valid certificate
    // and must become a trust anchor.
    let ca = crate::ca::generate_ca().expect("generate ca");
    let mut roots = rustls::RootCertStore::empty();
    assert!(roots.is_empty());
    super::add_extra_ca_pem(&mut roots, ca.cert_pem().as_bytes()).expect("add valid ca");
    assert!(!roots.is_empty(), "a valid extra CA becomes a trust anchor");
}

#[test]
fn add_extra_ca_pem_rejects_pem_with_no_certificate() {
    // Garbage and non-certificate PEM both yield zero certs → fail closed.
    let mut roots = rustls::RootCertStore::empty();
    assert!(super::add_extra_ca_pem(&mut roots, b"not a pem at all").is_err());
    let key_only = "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n";
    assert!(super::add_extra_ca_pem(&mut roots, key_only.as_bytes()).is_err());
}

#[test]
fn build_upstream_config_missing_extra_ca_file_fails_closed() {
    let r = build_upstream_client_config(None, Some(std::path::Path::new("/nonexistent/ca.pem")));
    assert!(r.is_err(), "a set-but-unreadable extra CA must fail closed");
}

#[test]
fn build_upstream_config_none_extra_ca_is_ok() {
    assert!(build_upstream_client_config(None, None).is_ok());
}
```

Also update the **existing** calls in `pins/tests.rs` that pass one arg
(`build_upstream_client_config(None)`, `Some("   ")`, `Some("{}")`, `Some(&json)`,
`Some("{ this is not json")`) to add the new `, None` second argument.

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-egress-proxy pins:: 2>&1 | tail -30
```
Expected: compile error (`build_upstream_client_config` takes 1 arg; `add_extra_ca_pem` not found).

- [ ] **Step 3: Implement in `pins.rs`.** Add `use std::path::Path;` near the top imports and `use rustls::pki_types::pem::PemObject;` (needed for `pem_slice_iter`). Add a `PinError` variant + its `Display` arm:

```rust
    /// The operator-provided upstream extra CA could not be read or parsed.
    ExtraCa(String),
```
```rust
            PinError::ExtraCa(s) => write!(f, "upstream extra CA: {s}"),
```

Add the pure helper (place it just above `build_upstream_client_config`):

```rust
/// Add every certificate in `pem` to `roots` as a trust anchor for the upstream
/// re-origination leg. Fail-closed: an unparseable cert, or a PEM containing
/// **zero** certificates, is an error — we never proceed with an extra CA the
/// operator asked for but we could not load. Pure over its inputs (no
/// filesystem), so the trust-widening logic is unit-testable directly.
pub(crate) fn add_extra_ca_pem(roots: &mut RootCertStore, pem: &[u8]) -> Result<(), PinError> {
    let mut added = 0usize;
    for der in CertificateDer::pem_slice_iter(pem) {
        let der = der.map_err(|e| PinError::ExtraCa(format!("parse: {e}")))?;
        roots.add(der).map_err(|e| PinError::ExtraCa(format!("add: {e}")))?;
        added += 1;
    }
    if added == 0 {
        return Err(PinError::ExtraCa("PEM contained no certificates".to_string()));
    }
    Ok(())
}
```

Change `build_upstream_client_config` to accept and apply the extra CA:

```rust
pub fn build_upstream_client_config(
    pins_env: Option<&str>,
    extra_ca_path: Option<&Path>,
) -> Result<Arc<rustls::ClientConfig>, PinError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Operator-provided extra trust anchor for the re-origination leg (e.g. a
    // self-signed personal localmail). Off by default ⇒ webpki-only, unchanged.
    // Fail-closed: a set-but-unreadable/invalid/zero-cert PEM aborts startup.
    if let Some(path) = extra_ca_path {
        let pem = std::fs::read(path)
            .map_err(|e| PinError::ExtraCa(format!("read {path:?}: {e}")))?;
        add_extra_ca_pem(&mut roots, &pem)?;
    }

    let pins = match pins_env.map(str::trim) {
        None | Some("") => PinSet::default(),
        Some(json) => PinSet::parse(json)?,
    };

    if pins.is_empty() {
        return Ok(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
    }

    let verifier = Arc::new(PinningVerifier::new(Arc::new(roots), pins)?);
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous() // custom verifier — STRENGTHENS validation (webpki + pin overlay)
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}
```

> If `CertificateDer::pem_slice_iter` does not resolve, `workers/egress-proxy/Cargo.toml`
> needs the pki-types `pem` feature (web-common already enables it): add
> `rustls-pki-types = { version = "1", features = ["std"] }` or enable the `pem` feature on
> the existing pki-types/rustls dep. Verify with `cargo build -p kastellan-worker-egress-proxy`.

- [ ] **Step 4: Wire `main.rs`** — replace the `upstream_tls` block (lines ~90-98):

```rust
    // Upstream trust for the re-origination leg: the REAL public roots, plus an
    // optional operator-provided SPKI pin overlay (slice #4) AND an optional
    // operator-provided extra CA (#491, for a self-signed private origin like a
    // personal localmail). Both are off by default; a malformed value aborts
    // startup (fail-closed) rather than silently disabling protection.
    let upstream_extra_ca = std::env::var("KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA").ok();
    if let Some(ref p) = upstream_extra_ca {
        eprintln!(
            "[egress-proxy] WARN: trusting operator-provided upstream extra CA {p:?} on the \
             re-origination leg (widens upstream trust beyond webpki roots)"
        );
    }
    let upstream_tls = pins::build_upstream_client_config(
        std::env::var("KASTELLAN_EGRESS_PROXY_PINS").ok().as_deref(),
        upstream_extra_ca.as_deref().map(std::path::Path::new),
    )
    .map_err(|e| anyhow::anyhow!("build upstream TLS config: {e}"))?;
```

- [ ] **Step 5: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-egress-proxy 2>&1 | tail -20
cargo build -p kastellan-worker-egress-proxy 2>&1 | tail -5
```
Expected: all pins tests PASS; crate builds.

- [ ] **Step 6: Commit**

```sh
git add workers/egress-proxy/src/pins.rs workers/egress-proxy/src/pins/tests.rs workers/egress-proxy/src/main.rs
# include workers/egress-proxy/Cargo.toml only if you had to touch it
git commit -m "egress-proxy: operator-provided upstream extra-CA seam for re-origination (#491)"
```

---

## Task 2: Core-side plumbing (spawn.rs + net_worker.rs + all constructors/callers)

**Files:**
- Modify: `core/src/egress/spawn.rs` (const, `proxy_policy`, `spawn_sidecar`, unit tests)
- Modify: `core/src/egress/net_worker.rs` (`NetWorkerSpawn` field + the `spawn_sidecar` call)
- Modify: `core/src/egress/net_worker/tests.rs:118,152,194,225,302` (add field `None`)
- Modify: `core/src/worker_lifecycle/force_route.rs:257` (add field `None`)
- Modify: `core/src/egress/persistent_net.rs:85` (append `None` arg)
- Modify: `core/tests/egress_proxy_e2e.rs:60,119` (append `None` arg)
- Modify: `core/tests/browser_driver_e2e.rs:288`, `core/tests/egress_force_routing_e2e.rs:117,228,353`, `core/tests/mail_e2e.rs:272`, `core/tests/web_research_firecracker_broker_e2e.rs:375` (add field `None`)

**Interfaces:**
- Consumes: `build_upstream_client_config(_, extra_ca_path)` + env `KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA` from Task 1.
- Produces: `proxy_policy(binary, allowlist, scratch, worker, cert_pins_json, disable_mitm, long_lived, upstream_extra_ca: Option<&Path>)` (new **trailing** param).
- Produces: `spawn_sidecar(backend, binary, allowlist, scratch, worker, cert_pins_json, disable_mitm, long_lived, upstream_extra_ca: Option<&Path>)` (new **trailing** param).
- Produces: `NetWorkerSpawn.upstream_extra_ca: Option<&'a std::path::Path>`.

> **Why trailing, not grouped with `cert_pins_json`:** appending minimizes positional-arg
> churn across the ~14 existing call sites (each just gains `, None`), which is safer for a
> mechanical edit. The fns already carry `#[allow(clippy::too_many_arguments)]`.

- [ ] **Step 1: Write the failing unit tests** in `core/src/egress/spawn.rs` (in the `mod tests`):

```rust
#[test]
fn proxy_policy_includes_upstream_extra_ca_env_and_fs_read_when_set() {
    let ca = PathBuf::from("/etc/localmail/ca.pem");
    let p = proxy_policy(
        Path::new("/bin/proxy"), &["127.0.0.1:8443".into()],
        Path::new("/scratch"), "mail", None, false, false, Some(&ca),
    );
    let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
    assert_eq!(env[ENV_UPSTREAM_EXTRA_CA], "/etc/localmail/ca.pem");
    assert!(p.fs_read.contains(&ca), "the extra CA must be bound into the proxy jail");
}

#[test]
fn proxy_policy_omits_upstream_extra_ca_when_none() {
    let p = proxy_policy(
        Path::new("/bin/proxy"), &["example.com".into()],
        Path::new("/scratch"), "web-fetch", None, false, false, None,
    );
    let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
    assert!(!env.contains_key(ENV_UPSTREAM_EXTRA_CA));
    assert!(!p.fs_read.contains(&PathBuf::from("/etc/localmail/ca.pem")));
}
```

- [ ] **Step 2: Run to verify failure**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib egress::spawn 2>&1 | tail -20
```
Expected: compile error (`proxy_policy` takes 7 args; `ENV_UPSTREAM_EXTRA_CA` undefined).

- [ ] **Step 3: Implement in `spawn.rs`.** Add the const near the other `ENV_*`:

```rust
/// Env key pointing the sidecar at an operator-provided extra CA to trust on the
/// re-origination (upstream) leg — for a self-signed private origin (localmail,
/// #491). Must match the read in `egress-proxy::main`.
const ENV_UPSTREAM_EXTRA_CA: &str = "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA";
```

In `proxy_policy`, add the trailing param `upstream_extra_ca: Option<&Path>`. After the
`disable_mitm` env push, add:

```rust
    // Operator-provided extra CA for the re-origination leg (#491). Omit the key
    // entirely when absent so the no-extra-CA path is byte-identical.
    if let Some(ca) = upstream_extra_ca {
        env.push((ENV_UPSTREAM_EXTRA_CA.to_string(), ca.to_string_lossy().into_owned()));
    }
```

Replace the `fs_read: vec![ ... ]` literal in the returned `SandboxPolicy` with a
pre-built vec so the CA can be appended:

```rust
    let mut fs_read = vec![
        binary.to_path_buf(),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    // The proxy reads the extra CA at startup (before lock_down); it must be
    // bound into the jail's fs_read to be openable.
    if let Some(ca) = upstream_extra_ca {
        fs_read.push(ca.to_path_buf());
    }
```
and use `fs_read,` in the struct literal.

In `spawn_sidecar`, add the trailing param `upstream_extra_ca: Option<&Path>` and pass it
to `proxy_policy(...)` as the new final argument.

- [ ] **Step 4: Update every `proxy_policy(...)` call in `spawn.rs`'s own `mod tests`** — append `, None` to each (there are ~8: `policy_uses_proxy_egress_and_net_client`, `derived_proxy_policy_carries_lockdown_env_for_dns`, `proxy_policy_long_lived_has_no_cpu_cap`, `proxy_policy_short_lived_keeps_bounded_cpu_cap`, `derived_short_lived_policy_carries_cpu_ms_env`, `proxy_policy_omits_pins_env_when_none`, `proxy_policy_includes_pins_env_when_set`, `proxy_policy_sets_disable_mitm_env_when_requested`, `proxy_policy_omits_disable_mitm_env_when_false`).

- [ ] **Step 5: Thread it through `net_worker.rs`.** Add `use std::path::Path;` if not present. Add the field to `NetWorkerSpawn`:

```rust
    /// Operator-provided extra CA to trust on the sidecar's re-origination
    /// (upstream) leg, for a self-signed private origin (localmail, #491).
    /// `None` ⇒ webpki-only (the production default — no prod wiring yet).
    pub upstream_extra_ca: Option<&'a Path>,
```
Change the `spawn_sidecar(...)` call inside `spawn_net_worker` to pass
`params.upstream_extra_ca` as the new final argument.

- [ ] **Step 6: Update the remaining callers/constructors (mechanical).**
  - `core/src/egress/persistent_net.rs:85` — append `None` to the `spawn_sidecar(...)` call.
  - `core/tests/egress_proxy_e2e.rs:60,119` — append `None` to the `spawn_sidecar(...)` calls.
  - Add `upstream_extra_ca: None,` to each `NetWorkerSpawn { ... }`:
    `core/src/egress/net_worker/tests.rs` (5), `core/src/worker_lifecycle/force_route.rs:257`,
    `core/tests/browser_driver_e2e.rs:288`, `core/tests/egress_force_routing_e2e.rs` (3),
    `core/tests/mail_e2e.rs:272`, `core/tests/web_research_firecracker_broker_e2e.rs:375`.
  - Verify none were missed: `git grep -n "NetWorkerSpawn {" -- '*.rs'` and confirm each now has the field.

- [ ] **Step 7: Run tests + clippy to verify green**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib egress::spawn 2>&1 | tail -20
cargo build -p kastellan-core --all-targets 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets 2>&1 | tail -10
```
Expected: the two new `proxy_policy` tests PASS; `--all-targets` builds (all constructors updated); no clippy warnings on the changed code.

> **Mac note:** `core` integration tests won't fully run on the Mac (ring C-dep / PG), and
> a full-workspace run may flake PG bring-up — use skip-as-pass locally; the DGX gate in
> Task 8 is authoritative. `cargo build -p kastellan-core --all-targets` is the compile
> check that all `core/tests/*` constructors were updated.

- [ ] **Step 8: Commit**

```sh
git add core/src/egress/spawn.rs core/src/egress/net_worker.rs core/src/egress/net_worker/tests.rs \
        core/src/egress/persistent_net.rs core/src/worker_lifecycle/force_route.rs \
        core/tests/egress_proxy_e2e.rs core/tests/browser_driver_e2e.rs \
        core/tests/egress_force_routing_e2e.rs core/tests/mail_e2e.rs \
        core/tests/web_research_firecracker_broker_e2e.rs
git commit -m "egress: thread upstream_extra_ca through spawn_sidecar + NetWorkerSpawn (#491)"
```

---

## Task 3: Shared loopback cert-gen helper (tls_origin.rs)

**Files:**
- Modify: `tests-common/src/tls_origin.rs`

**Interfaces:**
- Produces: `pub fn generate_loopback_cert() -> (rustls_pki_types::CertificateDer<'static>, rustls_pki_types::PrivateKeyDer<'static>, String)` — (cert DER, PKCS#8 key DER, cert PEM). The self-signed leaf carries a `127.0.0.1` IP-SAN and is its own trust anchor.

- [ ] **Step 1: Add the helper** above `spawn_loopback_tls_origin`:

```rust
/// Generate a self-signed loopback cert with a `127.0.0.1` IP SAN. Returns
/// `(cert_der, key_der, cert_pem)`. The leaf is its own trust anchor — a client
/// trusting `cert_pem` validates a TLS session to `127.0.0.1:<port>`. Shared by
/// the 204 origin here and the TLS localmail mock (#491).
pub fn generate_loopback_cert() -> (
    rustls_pki_types::CertificateDer<'static>,
    rustls_pki_types::PrivateKeyDer<'static>,
    String,
) {
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("generate self-signed cert");
    let cert_pem = ck.cert.pem();
    let cert_der = ck.cert.der().clone();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
    );
    (cert_der, key_der, cert_pem)
}
```

- [ ] **Step 2: Use it in `spawn_loopback_tls_origin`** — replace the inline `rcgen::…`/`cert_pem`/`cert_der`/`key_der` block (lines ~42-48) with:

```rust
    let (cert_der, key_der, cert_pem) = generate_loopback_cert();
```
(The rest — `ServerConfig::builder()…with_single_cert(vec![cert_der], key_der)` — is unchanged.)

- [ ] **Step 3: Verify the existing harness test still passes**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common tls_origin 2>&1 | tail -15
```
Expected: `origin_serves_204_over_tls_trusting_returned_cert` PASS (behaviour preserved).

- [ ] **Step 4: Commit**

```sh
git add tests-common/src/tls_origin.rs
git commit -m "tests-common: factor generate_loopback_cert shared by tls_origin + localmail mock (#491)"
```

---

## Task 4: HTTPS `mock_localmail` (tests-common)

**Files:**
- Modify: `tests-common/src/mock_localmail.rs`

**Interfaces:**
- Consumes: `tls_origin::generate_loopback_cert()` (Task 3).
- Produces: `pub async fn spawn_mock_localmail_tls() -> (MockLocalmail, String)` — the mock (aborts on drop) + the cert PEM. `base_url` is `https://127.0.0.1:<port>`. Serves the identical `/v1` shapes as `spawn_mock_localmail` (reuses `route`).

- [ ] **Step 1: Write the failing unit test** in `mock_localmail.rs`'s `mod tests`:

```rust
/// The TLS mock serves the same `/v1/search` `results` shape as the plain mock,
/// over TLS, to a client trusting only the returned cert — the exact trust path
/// the force-routed MITM e2e relies on (proxy upstream extra CA), without a sandbox.
#[test]
fn tls_mock_serves_search_results_over_tls() {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mock, cert_pem) = spawn_mock_localmail_tls().await;
        let port: u16 = mock.base_url.rsplit(':').next().unwrap().parse().unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap()).unwrap();
        let cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(std::sync::Arc::new(cfg));

        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let sni = ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
        let mut tls = connector.connect(sni, tcp).await.expect("tls handshake");
        tls.write_all(
            b"POST /v1/search HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer t\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        ).await.unwrap();
        let mut resp = Vec::new();
        tls.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(v["results"].is_array(), "expected results array, got {v}");
    });
}
```

- [ ] **Step 2: Run to verify failure**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common mock_localmail 2>&1 | tail -20
```
Expected: compile error (`spawn_mock_localmail_tls` not found).

- [ ] **Step 3: Extract the per-connection handler.** In `mock_localmail.rs`, add a generic
async fn holding the existing read-head/drain-body/route/respond logic currently inline in
`spawn_mock_localmail`'s accept loop:

```rust
/// Serve one localmail connection: read the request head (draining the declared
/// body so the close is a clean FIN, not an RST that truncates the client's
/// read), route it via [`route`], and write the response. Generic over the
/// stream so the plain-TCP and TLS spawns share exactly one implementation.
async fn serve_localmail_conn<S>(sock: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    let head = loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_str = match std::str::from_utf8(&buf[..i]) {
                Ok(s) => s.to_owned(),
                Err(_) => break None,
            };
            let want = (i + 4) + content_length(&header_str);
            while buf.len() < want && buf.len() <= 64 * 1024 {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            break Some(header_str);
        }
        if buf.len() > 64 * 1024 {
            break None;
        }
    };
    let (status, ctype, body): (&str, &str, Vec<u8>) = match head.as_deref() {
        Some(h) => route(h),
        None => ("400 Bad Request", "text/plain", b"bad request".to_vec()),
    };
    let resp_head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(resp_head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
}
```
Replace the body of `spawn_mock_localmail`'s inner `loop { let (mut sock, _peer) = … }` so
that after accepting it calls `serve_localmail_conn(&mut sock).await;` (drop the now-inlined
read/route/write code). Behaviour is unchanged — the existing `mod tests` cases still pin it.

- [ ] **Step 4: Add `spawn_mock_localmail_tls`:**

```rust
/// A live **self-signed-HTTPS** localmail mock at `https://127.0.0.1:<port>`.
/// Returns the mock (aborts its listener on drop) and the cert PEM (the caller
/// writes it wherever the egress proxy's upstream extra CA must live). Serves the
/// identical `/v1` shapes as [`spawn_mock_localmail`] — the force-routed MITM path
/// can reach it once the proxy is given this cert as its upstream extra CA (#491).
pub async fn spawn_mock_localmail_tls() -> (MockLocalmail, String) {
    let (cert_der, key_der, cert_pem) = crate::tls_origin::generate_loopback_cert();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("build localmail tls server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("https://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (tcp, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let mut tls = match acceptor.accept(tcp).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                serve_localmail_conn(&mut tls).await;
            });
        }
    });

    (MockLocalmail { base_url, join: Some(join) }, cert_pem)
}
```

> `tokio-rustls` + `rustls` are already dev/normal deps of `tests-common` (used by
> `tls_origin.rs`). If `spawn_mock_localmail_tls` must be reachable from `core/tests`
> (it is — Tasks 5/6), confirm `tests-common`'s `rustls`/`tokio-rustls` deps are non-dev
> (they are, per `tls_origin`).

- [ ] **Step 5: Run to verify pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common mock_localmail 2>&1 | tail -20
```
Expected: `tls_mock_serves_search_results_over_tls` PASS + the three existing plain-HTTP mock tests still PASS.

- [ ] **Step 6: Commit**

```sh
git add tests-common/src/mock_localmail.rs
git commit -m "tests-common: HTTPS spawn_mock_localmail_tls sharing the /v1 router (#491)"
```

---

## Task 5: Hermetic round-trip tier + negative control (mail_e2e.rs)

**Files:**
- Modify: `core/tests/mail_e2e.rs`

**Interfaces:**
- Consumes: `spawn_mock_localmail_tls` (Task 4), `NetWorkerSpawn.upstream_extra_ca` (Task 2), `spawn_forced_net_worker`, `dispatch`, `mail_entry`, the tier-1b helpers already imported.

- [ ] **Step 1: Add a shared driver helper + the two tests.** Append to `mail_e2e.rs`:

```rust
/// Drive a force-routed `mail.search` through a real MITM egress sidecar to a
/// self-signed HTTPS localmail mock. `with_extra_ca` toggles whether the proxy is
/// given the mock's cert as its upstream extra CA. Returns the dispatch result
/// (error mapped to String) and the captured egress decisions. Shared by the
/// positive round-trip and the negative control so they differ only in the one
/// variable under test.
async fn run_forced_mail_search_over_tls(
    proxy: &std::path::Path,
    bin_dir: &std::path::Path,
    with_extra_ca: bool,
) -> (
    Result<serde_json::Value, String>,
    Vec<kastellan_core::egress::audit::EgressAuditRow>,
) {
    use std::sync::{Arc, Mutex};
    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_sandbox::Net;
    use kastellan_tests_common::egress_forcing::short_scratch_root;
    use kastellan_tests_common::mock_localmail::spawn_mock_localmail_tls;

    let (mock, cert_pem) = spawn_mock_localmail_tls().await;
    // Write the mock's cert where the sandboxed proxy can fs_read it.
    let ca_dir = tempfile::tempdir().expect("ca tempdir");
    let ca_path = ca_dir.path().join("localmail-ca.pem");
    std::fs::write(&ca_path, &cert_pem).expect("write ca pem");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        bin_dir, "mailrt-d", "mailrt-l",
        &format!("kastellan-supervisor-test-pg-mailrt-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let (_token_dir, token_file) = write_token_file();
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    let mail_policy =
        mail_entry(worker_path.clone(), &mock.base_url, &token_file.to_string_lossy()).policy;
    let allowlist: Vec<String> = match &mail_policy.net {
        Net::Allowlist(v) => v.clone(),
        other => panic!("mail must be Net::Allowlist, got {other:?}"),
    };

    let scratch_root = short_scratch_root(&format!("mailrt-{}", unique_suffix()));
    let rows = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let rows = Arc::clone(&rows);
        move |row: kastellan_core::egress::audit::EgressAuditRow| rows.lock().unwrap().push(row)
    };

    let worker_str = worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec { policy: &mail_policy, program: &worker_str, args: &[], wall_clock_ms: None };
    let backend = backend();
    let params = NetWorkerSpawn {
        backend: backend.as_ref(),
        sidecar_backend: backend.as_ref(),
        proxy_bin: proxy,
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "mail",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: false, // MITM ON — mail's real posture
        upstream_extra_ca: with_extra_ca.then_some(ca_path.as_path()),
    };
    let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
        .expect("force-routed mail worker + sidecar spawn");

    let result = dispatch(
        &pool, &Vault::new(), &mut worker, "mail", "mail.search",
        serde_json::json!({"query": "invoice"}),
    )
    .await
    .map_err(|e| e.to_string());

    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&scratch_root);
    let captured = std::mem::take(&mut *rows.lock().unwrap());
    (result, captured)
}

/// Hermetic full round-trip: the REAL mail worker, force-routed in MITM mode,
/// drives mail.search through the sidecar to a self-signed HTTPS localmail mock;
/// the proxy MITM-terminates and re-originates TLS validated against the
/// operator-provided upstream extra CA. Asserts the results round-trip AND
/// tls_intercepted: true. The #491 deliverable tier 1b could not cover.
#[test]
fn force_routed_search_round_trips_through_mitm_sidecar() {
    use kastellan_tests_common::egress_proxy_bin_or_skip;
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else { return; };
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    if !workspace_target_binary("kastellan-worker-mail").exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return;
    }

    dispatch_runtime().block_on(async {
        let (result, rows) = run_forced_mail_search_over_tls(&proxy, &bin_dir, true).await;
        let value = result.expect("mail.search must round-trip through the MITM sidecar");
        assert!(value["results"].is_array(), "expected results array, got {value}");
        assert!(
            rows.iter().any(|r| r.action == "egress.allowed"
                && r.payload["tls_intercepted"] == serde_json::Value::Bool(true)),
            "expected an MITM-intercepted allow decision (tls_intercepted: true); got {:?}",
            rows.iter().map(|r| (r.action.clone(), r.payload.clone())).collect::<Vec<_>>()
        );
    });
}

/// Negative control: the identical round-trip with NO upstream extra CA must
/// FAIL — the proxy re-originates against webpki roots only and rejects the
/// self-signed origin. Proves the extra-CA seam is load-bearing (the round-trip
/// does not "accidentally" work without it).
#[test]
fn force_routed_search_fails_without_upstream_extra_ca() {
    use kastellan_tests_common::egress_proxy_bin_or_skip;
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else { return; };
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    if !workspace_target_binary("kastellan-worker-mail").exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return;
    }

    dispatch_runtime().block_on(async {
        let (result, _rows) = run_forced_mail_search_over_tls(&proxy, &bin_dir, false).await;
        assert!(
            result.is_err(),
            "without the upstream extra CA the MITM re-origination must reject the \
             self-signed origin; got Ok: {result:?}"
        );
    });
}
```

> **Import note:** `run_forced_mail_search_over_tls` uses `probe_and_pool`, `write_token_file`,
> `dispatch_runtime`, `bring_up_pg_cluster`, `workspace_target_binary`, `unique_suffix`,
> `backend`, `mail_entry`, `dispatch`, `Vault`, `WorkerSpec` — all already imported at the top
> of `mail_e2e.rs`. Add only `tempfile` (already a dev-dep of `core`).

- [ ] **Step 2: Compile-check on the Mac (tests skip-as-pass locally)**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test mail_e2e --no-run 2>&1 | tail -10
```
Expected: compiles. (The tiers skip-as-pass without live PG + real sandbox on the Mac.)

- [ ] **Step 3: DGX — prove it fails against un-seamed code, then passes.** This is the
load-bearing verification. On the DGX (see Task 8 for the build prelude), run the two tests.
First confirm the **negative control passes** (round-trip errors without the CA) and the
**positive passes** (round-trip + tls_intercepted). To prove the positive is real, the
negative control IS the proof — it demonstrates the same flow fails without the seam.

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace 2>&1 | tail -3 && \
  cargo test -p kastellan-core --test mail_e2e force_routed_search -- --nocapture 2>&1 | tail -40'
```
Expected: `force_routed_search_round_trips_through_mitm_sidecar` PASS and
`force_routed_search_fails_without_upstream_extra_ca` PASS.

- [ ] **Step 4: Commit**

```sh
git add core/tests/mail_e2e.rs
git commit -m "test(mail): hermetic force-routed MITM round-trip + negative control (#491)"
```

---

## Task 6: Live `#[ignore]` DGX tier (mail_e2e.rs)

**Files:**
- Modify: `core/tests/mail_e2e.rs`

**Interfaces:**
- Consumes: the same machinery; the real localmail on the DGX (self-signed cert at `~/.config/localmail/tls/cert.pem`).

- [ ] **Step 1: Add the live tier** to `mail_e2e.rs`:

```rust
/// Live #[ignore] DGX tier: the same MITM round-trip against the REAL localmail
/// running on the DGX (self-signed cert), validating the extra-CA seam against a
/// real cert + the real archive. Env-gated — skip-as-pass unless the operator
/// sets all three live vars. Run on the DGX:
///
///   KASTELLAN_MAIL_LIVE_ENDPOINT=https://127.0.0.1:8443 \
///   KASTELLAN_MAIL_LIVE_CA=$HOME/.config/localmail/tls/cert.pem \
///   KASTELLAN_MAIL_LIVE_TOKEN=<bearer from POST /v1/auth/login> \
///   cargo test -p kastellan-core --test mail_e2e -- --ignored --nocapture \
///     force_routed_search_against_real_localmail
///
/// The endpoint host MUST match a SAN in the cert (inspect it on the DGX). Prefer
/// 127.0.0.1 (loopback — dialable via the proxy's allowlisted-IP carve-out); a
/// private LAN IP (10.0.0.3) may hit the SSRF block. Token is pre-obtained to keep
/// the password out of the test process.
#[test]
#[ignore = "live DGX localmail; set KASTELLAN_MAIL_LIVE_ENDPOINT/CA/TOKEN"]
fn force_routed_search_against_real_localmail() {
    use std::sync::{Arc, Mutex};
    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_sandbox::Net;
    use kastellan_tests_common::egress_forcing::short_scratch_root;
    use kastellan_tests_common::egress_proxy_bin_or_skip;

    let (Some(endpoint), Some(ca), Some(token)) = (
        std::env::var("KASTELLAN_MAIL_LIVE_ENDPOINT").ok(),
        std::env::var("KASTELLAN_MAIL_LIVE_CA").ok(),
        std::env::var("KASTELLAN_MAIL_LIVE_TOKEN").ok(),
    ) else {
        eprintln!("\n[SKIP] live localmail vars unset (KASTELLAN_MAIL_LIVE_ENDPOINT/CA/TOKEN)\n");
        return;
    };
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else { return; };
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] mail worker binary not built\n");
        return;
    }

    dispatch_runtime().block_on(async {
        use std::os::unix::fs::PermissionsExt;
        let token_dir = tempfile::tempdir().expect("token tempdir");
        let token_file = token_dir.path().join("mail-token");
        std::fs::write(&token_file, token.trim()).expect("write token");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();

        let ca_path = std::path::PathBuf::from(&ca);
        assert!(ca_path.exists(), "KASTELLAN_MAIL_LIVE_CA does not exist: {ca}");

        let suffix = unique_suffix();
        let cluster = bring_up_pg_cluster(
            &bin_dir, "maillive-d", "maillive-l",
            &format!("kastellan-supervisor-test-pg-maillive-{suffix}"),
        );
        let pool = probe_and_pool(&cluster.conn_spec).await;

        let mail_policy =
            mail_entry(worker_path.clone(), &endpoint, &token_file.to_string_lossy()).policy;
        let allowlist: Vec<String> = match &mail_policy.net {
            Net::Allowlist(v) => v.clone(),
            other => panic!("mail must be Net::Allowlist, got {other:?}"),
        };

        let scratch_root = short_scratch_root(&format!("maillive-{}", unique_suffix()));
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let rows = Arc::clone(&rows);
            move |row: kastellan_core::egress::audit::EgressAuditRow| rows.lock().unwrap().push(row)
        };

        let worker_str = worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &mail_policy, program: &worker_str, args: &[], wall_clock_ms: None };
        let backend = backend();
        let params = NetWorkerSpawn {
            backend: backend.as_ref(),
            sidecar_backend: backend.as_ref(),
            proxy_bin: &proxy,
            spec: &spec,
            allowlist: &allowlist,
            worker_name: "mail",
            secret_fingerprints: &[],
            cert_pins_json: None,
            disable_mitm: false,
            upstream_extra_ca: Some(ca_path.as_path()),
        };
        let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
            .expect("force-routed mail worker + sidecar spawn");

        let value = dispatch(
            &pool, &Vault::new(), &mut worker, "mail", "mail.search",
            serde_json::json!({"query": "invoice"}),
        )
        .await
        .expect("live mail.search must round-trip through the MITM sidecar");
        assert!(value["results"].is_array(), "expected results array, got {value}");
        assert!(
            rows.lock().unwrap().iter().any(|r| r.action == "egress.allowed"
                && r.payload["tls_intercepted"] == serde_json::Value::Bool(true)),
            "expected an MITM-intercepted allow decision against live localmail"
        );

        let _ = worker.close();
        pool.close().await;
        let _ = std::fs::remove_dir_all(&scratch_root);
    });
}
```

- [ ] **Step 2: Compile-check**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test mail_e2e --no-run 2>&1 | tail -10
```
Expected: compiles.

- [ ] **Step 3: Run live on the DGX.** First inspect the cert's SANs and obtain a token:

```sh
ssh dgx 'openssl x509 -in ~/.config/localmail/tls/cert.pem -noout -text | grep -A2 "Subject Alternative Name"'
# obtain a bearer (username/password are the mcp-agent credentials):
ssh dgx 'curl -sk https://127.0.0.1:8443/v1/auth/login -H "content-type: application/json" \
  -d "{\"username\":\"mcp-agent\",\"password\":\"<PW>\"}" | python3 -c "import sys,json;print(json.load(sys.stdin)[\"token\"])"'
```
Choose `KASTELLAN_MAIL_LIVE_ENDPOINT` to match a cert SAN (prefer `https://127.0.0.1:8443`
if a 127.0.0.1 SAN exists). Then run the ignored test:

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace 2>&1 | tail -2 && \
  KASTELLAN_MAIL_LIVE_ENDPOINT=https://127.0.0.1:8443 \
  KASTELLAN_MAIL_LIVE_CA=$HOME/.config/localmail/tls/cert.pem \
  KASTELLAN_MAIL_LIVE_TOKEN=<token> \
  cargo test -p kastellan-core --test mail_e2e -- --ignored --nocapture \
    force_routed_search_against_real_localmail 2>&1 | tail -30'
```
Expected: PASS (results array + tls_intercepted). If it fails on TLS server-name, adjust the
endpoint host to a cert SAN; if on SSRF, use the loopback address; document what worked.

- [ ] **Step 4: Commit**

```sh
git add core/tests/mail_e2e.rs
git commit -m "test(mail): live #[ignore] DGX tier — force-routed round-trip vs real localmail (#491)"
```

---

## Task 7: Follow-up issue + institutional-fact correction

**Files:**
- (No source change.) File a GitHub issue; the HANDOVER/ROADMAP/memory corrections land in the session-end update (Task 8).

- [ ] **Step 1: File the deferred-production-wiring follow-up issue**

```sh
gh issue create --title "mail/egress: wire upstream extra-CA into ForceRoutingConfig::from_env (deployed force-routed self-signed origins)" \
  --body "Follow-up from #491 (PR: force-routed mail round-trip e2e).

#491 added the egress-proxy **upstream extra-CA capability** (\`build_upstream_client_config\`
extra-CA param → \`KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA\` → \`proxy_policy\`/\`spawn_sidecar\`
→ \`NetWorkerSpawn.upstream_extra_ca\`) and proved it with a hermetic e2e + a live DGX tier.
It deliberately did **not** wire the production operator config: \`force_route.rs\` passes
\`upstream_extra_ca: None\`, so a **deployed** force-routed mail worker still cannot reach a
self-signed localmail.

**This issue:** wire an operator config into \`ForceRoutingConfig::from_env\` — recommended a
**host-keyed** \`{origin-host: ca-path}\` map (mirroring how \`cert_pins\` are host-selected via
\`pins_for(&allowlist)\`), so only the matching worker's sidecar trusts the CA (minimal blast
radius). Add: parse + validate at from_env; select the CA(s) whose host is in the worker's
allowlist in \`spawn_worker_maybe_forced\`; a loud startup log; fail-closed on a bad path;
docs in \`kastellan.env\`. Then a deployed force-routed mail worker reaches a self-signed
localmail end-to-end.

Spec: \`docs/superpowers/specs/2026-07-25-mail-worker-force-routed-roundtrip-design.md\`." \
  --repo hherb/kastellan
```

- [ ] **Step 2: Note the issue number** for the session-end HANDOVER update.

---

## Task 8: Full verification + session-end docs

- [ ] **Step 1: DGX full-workspace gate** (authoritative). Build workspace first so the mail
+ egress-proxy binaries are fresh, then full test + clippy, capturing to `~` (never `/tmp`):

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  setsid bash -lc "cargo build --workspace && \
    cargo test --workspace -- --nocapture && \
    cargo clippy --workspace --all-targets -- -D warnings; echo DONE_EXIT=\$?" \
    > ~/dgx-491.log 2>&1 </dev/null & echo started'
# poll:
ssh dgx 'grep -c DONE_EXIT ~/dgx-491.log; tail -5 ~/dgx-491.log'
```
Expected: `DONE_EXIT=0`, test summary `<N> passed / 0 failed`, clippy clean, **0 `[SKIP]`**
lines (grep `ssh dgx 'grep -c "\[SKIP\]" ~/dgx-491.log'` → 0). Record the new passed/ignored
counts (mock round-trip + negative control add **+2 always-run**; the live tier adds **+1
ignored**).

- [ ] **Step 2: macOS hygiene** for the Mac-testable crates:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-egress-proxy 2>&1 | tail -5
cargo test -p kastellan-tests-common 2>&1 | tail -5
cargo clippy -p kastellan-worker-egress-proxy -p kastellan-tests-common --all-targets 2>&1 | tail -5
# cross-clippy the cfg-touched core egress bits for the Linux direction:
cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu 2>&1 | tail -5 || true
```

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md** (rule 8), folding in:
  - This PR (#491): the upstream extra-CA seam + hermetic round-trip + negative control + live DGX tier; the deferred-wiring follow-up issue number from Task 7.
  - The **rule-7 reconciliation**: mark #490 (`efc1001b`) **MERGED** (the header still says "PR OPEN").
  - The **institutional-fact correction**: the standing "force-routed/MITM path reaches self-signed localmail" deployment claim was **false before this PR** — the seam is what makes it reachable (and the deployed path awaits the Task-7 follow-up). Correct the wording in HANDOVER + any docstring that repeats it.
  - Update the DGX test-count baseline. Prune both docs toward ≤500 lines.

- [ ] **Step 4: Open the PR** (link #491) once the DGX gate is green:

```sh
git push -u origin feat/491-mail-force-routed-roundtrip
gh pr create --repo hherb/kastellan --base main \
  --title "test(mail): full force-routed MITM round-trip e2e + upstream extra-CA seam (#491)" \
  --body "<summary: capability, hermetic round-trip + negative control, live DGX tier, deferred prod wiring follow-up #<n>, DGX counts>"
```

---

## Self-Review (run against the spec)

**Spec coverage:**
- Capability (§Component 1) → Task 1 (pins/main) + Task 2 (spawn/net_worker plumbing). ✓
- HTTPS mock + cert reuse (§Component 2) → Task 3 (cert-gen) + Task 4 (TLS mock). ✓
- Hermetic round-trip + negative control (§Component 3) → Task 5. ✓
- Live `#[ignore]` DGX tier (§decision 4 / live tier) → Task 6. ✓
- Deferred prod wiring → follow-up issue → Task 7. ✓
- Rule-7 docs reconciliation + institutional-fact correction → Task 8 Step 3. ✓
- Fail-closed / unset ⇒ byte-identical / prove-negative → Task 1 tests, Task 2 tests, Task 5 negative control, Task 8 [SKIP]=0 gate. ✓

**Placeholder scan:** none — every code step is complete; `<PW>`/`<token>`/`<n>` in Task 6/7/8
are operator-supplied runtime values, not plan gaps.

**Type consistency:** `build_upstream_client_config(Option<&str>, Option<&Path>)`,
`add_extra_ca_pem(&mut RootCertStore, &[u8]) -> Result<(), PinError>`,
`proxy_policy(..., upstream_extra_ca: Option<&Path>)` (trailing) and `spawn_sidecar(...,
upstream_extra_ca)` (trailing), `NetWorkerSpawn.upstream_extra_ca: Option<&Path>`,
`spawn_mock_localmail_tls() -> (MockLocalmail, String)`, `generate_loopback_cert() ->
(CertificateDer, PrivateKeyDer, String)`, `serve_localmail_conn<S>` — all consistent across
tasks. `tls_intercepted` asserted from `payload["tls_intercepted"] == Bool(true)` (matches
`audit.rs::decision_to_audit`). ✓
