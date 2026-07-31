# Email channel MITM leg + the `Mitm` posture type — design (#494 + email slice 3)

**Status:** designed 2026-07-31. Closes [#494](https://github.com/hherb/kastellan/issues/494)
and the KNOWN GAP recorded against the email fallback channel (slice 1, PR
[#496](https://github.com/hherb/kastellan/pull/496)).

**Prior art this builds on, directly:** [#491](https://github.com/hherb/kastellan/issues/491)
(the egress proxy's upstream extra-CA seam) and [#492](https://github.com/hherb/kastellan/issues/492)
(the operator config + single-private-origin enforcement that drives it). Both
already work for the `kastellan-worker-mail` **tool**. This spec extends the same
mechanism to the second spawn site, the one long-lived channel transports use.

---

## 1. Problem

`core/src/egress/persistent_net.rs::spawn_net_transport` — the bring-up path for
every long-lived channel worker — hardcodes its sidecar's TLS posture:

```rust
let mut sidecar = spawn_sidecar(
    params.sidecar_backend, params.proxy_bin, params.allowlist, scratch,
    params.worker_name,
    None, // no cert pins
    true, // disable_mitm — transparent tunnel
    true, // long-lived
    None, // upstream_extra_ca: N/A
)?;
```

There is no override. Two consequences:

1. **The email fallback channel cannot reach a self-signed origin at all.**
   `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` governs the *re-origination* leg of a
   MITM'd sidecar; a transparent tunnel has no such leg, so the env var is inert
   here. The worker's own TLS client is webpki-only. The live DGX localmail is
   self-signed, so the channel is unreachable there — force-routed or not. This
   blocks any live deployment of slice 1.
2. **That leg is invisible to the egress boundary.** A transparent tunnel relays
   ciphertext, so the #3b credential-leak scanner and any content-level decision
   cannot see it. The mail *tool*'s leg does not have this hole.

Separately, `spawn_sidecar`/`proxy_policy` have grown to 9 and 8 positional
arguments with **adjacent bare bools** (`disable_mitm`, `long_lived`) — issue
#494. Transposing them compiles silently and yields both the wrong TLS posture
and the wrong CPU governance (a long-lived sidecar handed a cumulative
`RLIMIT_CPU` that SIGKILLs it mid-flight — the #395 failure mode). This change
would be the *fourth* to touch that signature, so it closes #494 on the way
through.

## 2. Non-goals

- **Changing Matrix's posture.** `matrix-sdk` terminates its own TLS through
  `ProxyBridge` and cannot be made to trust a per-instance CA; Matrix stays
  `Transparent`. Same for the three micro-VM/net-demo e2e files.
- **Per-service (`host:port`) anchor keying.** #492's per-**host** keying and its
  documented limitation carry over unchanged. Two private services sharing one
  address are still one origin to the rule.
- **Cert pinning for channel sidecars.** `cert_pins_json` stays `None` on this
  path, as today.
- **The DGX `localmail-serve` restart.** A separate operator step, tracked in
  HANDOVER; it gates a live run, not this code.

## 3. The `Mitm` type

New, pure, in `core/src/egress/spawn.rs`:

```rust
/// The TLS posture of a worker's egress sidecar.
///
/// One value rather than two fields, because the two are not independent: an
/// upstream trust anchor is meaningful ONLY on the re-origination leg, which
/// only exists when the proxy terminates the worker's TLS.
pub enum Mitm<'a> {
    /// The proxy terminates the worker's TLS and re-originates upstream. The
    /// worker trusts the sidecar's per-instance CA; the sidecar validates the
    /// real origin with webpki plus `upstream_extra_ca`, when given one.
    Intercept { upstream_extra_ca: Option<&'a Path> },
    /// The proxy relays ciphertext untouched; the worker validates the origin
    /// itself and never sees our CA. For workers that cannot trust a
    /// per-instance CA (the browser, matrix-sdk).
    Transparent,
}
```

Derives `Debug, Clone, Copy, PartialEq, Eq` (it is a small `Copy` descriptor, and
tests assert on it directly). Deliberately **no `Default`**: every call site
states its posture, so a posture is never acquired by omission.

**What this buys.** `spawn::check_upstream_extra_ca` currently enforces two
preconditions at runtime; one of them —

> a path paired with `disable_mitm` … accepting the pair would leave an operator
> believing a private self-signed origin is reachable when in fact the sidecar
> validates no upstream certificate at all

— becomes **unrepresentable**. `Mitm::Transparent` has no field to put an anchor
in. The function keeps its other rule (the anchor must be an absolute path,
because it is bound into the proxy jail via `fs_read`).

This is the point of the refactor, and it should be stated in the module docs:
the pairing rule was not deleted, it was moved from the runtime into the type.

## 4. `SidecarSpawn` — closing #494

Exactly the struct #494 sketches, with the two fields collapsed:

```rust
pub struct SidecarSpawn<'a> {
    pub binary: &'a Path,
    pub allowlist: &'a [String],
    pub scratch: &'a Path,
    pub worker: &'a str,
    pub cert_pins_json: Option<&'a str>,
    pub mitm: Mitm<'a>,
    /// Lifetime-scoped CPU governance (issue #395): a channel sidecar lives for
    /// weeks, so it gets no cumulative `RLIMIT_CPU`; a per-dispatch sidecar keeps
    /// the bounded cap.
    pub long_lived: bool,
}
```

`proxy_policy(&SidecarSpawn) -> SandboxPolicy` and
`spawn_sidecar(backend: &dyn SandboxBackend, spec: &SidecarSpawn) -> Result<SidecarHandle>`
share it; `backend` stays a separate argument so the one struct serves both (the
policy builder has no use for a backend). Both `#[allow(clippy::too_many_arguments)]`
attributes go.

`long_lived` stays a **bool**, per #494's own sketch. The hazard #494 names is
*adjacent positional* bools; a named struct field cannot be transposed.

`NetWorkerSpawn`'s `disable_mitm: bool` + `upstream_extra_ca: Option<&Path>`
collapse into the same `mitm: Mitm<'a>`.

**No behaviour change.** Commit 1 is a pure refactor across ~15 call sites and
must be reviewable as a rename; the existing suite is its check.

## 5. `spawn_net_transport` gains the posture

`NetTransportSpawn` gets `mitm: Mitm<'a>`, and its existing `extra_ca` is renamed
**`worker_extra_ca`** — it is a *worker-side* origin cert (added to `fs_read` so a
VM RO-share carries it), and the old name reads like a sibling of
`upstream_extra_ca` when it is the opposite side of the connection.

Under `Intercept`, the function derives the CA path the way `spawn_net_worker`
already does (`core/src/egress/net_worker.rs`):

```rust
let ca = uds.parent().map(|d| d.join(CA_FILE_NAME))
    .unwrap_or_else(|| PathBuf::from(CA_FILE_NAME));
let forced = rewrite_worker_policy(base, &uds, Some(ca.as_path()));
```

`rewrite_worker_policy` already encodes the whole trust posture in that one
`Option`: `Some` binds the CA into `fs_read` and announces it via
`KASTELLAN_EGRESS_PROXY_CA`; `None` is the transparent tunnel. The existing
`forced_transparent_policy` helper stays as the `Transparent` arm.

**The worker side needs no change at all.** `kastellan-worker-email-in`'s client
is built by `web-common::http::make_get`, which already prefers
`KASTELLAN_EGRESS_PROXY_CA` when set. That is why this gap is host-side only.

## 6. Wiring: email always intercepts, anchor selected once at boot

`core/src/channel/email/mod.rs` passes
`Mitm::Intercept { upstream_extra_ca }` — **unconditionally**, not "when an
anchor is configured". With no anchor the sidecar is webpki-only, which is the
same posture every force-routed tool worker has; with one it reaches the
self-signed origin. One shape to reason about, and the leak scanner covers the
leg either way.

The anchor is selected **once, before the factory closure is built** (and only
when force-routing is on, i.e. the channel's `egress` is `Some`), from the config
the channel already holds (`EmailEgress.routing: Arc<ForceRoutingConfig>`):

```rust
let upstream_extra_ca: Option<PathBuf> = eg
    .routing
    .upstream_ca_for(&allowlist)?        // Err ⇒ the channel is disabled, loudly
    .map(Path::to_path_buf);             // owned: the closure outlives the borrow
```

`upstream_ca_for` is #492's existing selector, so the channel inherits the
single-private-origin rule for free: `Ok(None)` (no anchor configured for this
origin), `Ok(Some(path))` (exactly one, and it is the only host in the
allowlist), or `Err(MixedAllowlist | MultipleKeyedHosts)`.

**Why at boot and not per respawn.** An `Err` here is a configuration
disagreement, not a transient failure. Selecting at boot makes it disable the
email channel once with a loud `EMAIL CHANNEL DISABLED` `error!` — slice 1's
established invariant (`spawn_email_channel -> Option<ChannelBus>`; the daemon
keeps running, Matrix and the scheduler are untouched). Selecting inside the
factory would instead surface the same misconfiguration as an endlessly
repeating spawn error inside the supervisor's backoff loop. The owned `PathBuf`
also keeps the closure free of a borrow into the `Arc`'s interior.

Every other `NetTransportSpawn` call site passes `Mitm::Transparent`:
`core/src/channel/matrix.rs`, `core/tests/net_demo_egress_e2e.rs`,
`core/tests/net_demo_firecracker_egress_e2e.rs`,
`core/tests/matrix_firecracker_live_e2e.rs`. The Matrix call site gets a comment
saying *why* (matrix-sdk's own TLS through `ProxyBridge`), so the next person
does not "fix" the inconsistency.

## 7. The one pairing that stays a runtime check

`worker_extra_ca` + `Intercept` is refused up front by a pure function, before
anything spawns. Under interception the worker's transport trusts only
`KASTELLAN_EGRESS_PROXY_CA`, so an origin anchor handed to the worker would be
**silently inert** — the same false-belief failure mode #491 was opened to
correct, one layer over.

It stays a runtime check rather than a type-level one on purpose: folding the
worker-side cert into `Mitm::Transparent { worker_extra_ca }` would push a field
into the type that `proxy_policy` — the posture's main consumer — has no use for,
and would carry it into `NetWorkerSpawn` where it is meaningless. One minimal
posture type plus one checked pairing is the smaller total surface.

## 8. Test posture

TDD throughout; each assertion written failing first, and the security-relevant
ones proved to fail against the un-hardened code (the #479 house rule).

**Pure/unit (no sandbox, no PG):**

1. `proxy_policy` + `Transparent` ⇒ `KASTELLAN_EGRESS_PROXY_DISABLE_MITM=1`, no
   upstream-CA env key, no CA in `fs_read`.
2. `proxy_policy` + `Intercept { None }` ⇒ **neither** key present — byte-identical
   to today's default MITM path.
3. `proxy_policy` + `Intercept { Some(ca) }` ⇒ `KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA`
   set **and** `ca` bound into `fs_read` (the proxy reads it before `lock_down`,
   so both halves are load-bearing).
4. `check_upstream_extra_ca` rejects a relative anchor.
5. The new `Intercept` arm inside `spawn_net_transport` (a `forced_intercept_policy`
   sibling of today's `forced_transparent_policy`, so the branch is pure and
   testable without a spawn): `KASTELLAN_EGRESS_PROXY_CA` = `<scratch>/ca.pem`,
   that path in `fs_read`, `/etc/resolv.conf` still dropped.
6. `worker_extra_ca` + `Intercept` ⇒ `Err`, nothing spawned.
7. Email boot: the channel passes `Intercept`; a `MixedAllowlist` selection error
   disables the channel (no `ChannelBus`) and leaves the daemon alive.

**Hermetic e2e:** a MITM round-trip for the *channel* against a loopback TLS
origin (`kastellan-tests-common`'s harness) whose CA is supplied as the operator
anchor — assert round-tripped results **and** `tls_intercepted: true`, plus a
negative control without the anchor that fails with a `mitm_failed` decision so
the seam is provably load-bearing. This mirrors what PR #493 built for the mail
tool. Note this is only possible *because* #491 added the upstream extra-CA knob:
before it, no hermetic self-signed origin was reachable by a MITM'd worker at all.

`tls_intercepted: true` is emitted when the proxy takes the MITM branch, *before*
the upstream handshake — so it is necessary but not sufficient. The round-tripped
body is the load-bearing assertion (#491 trap 3). Any assertion on a *terminal*
egress decision must poll to quiescence: the decision-ingest thread is detached
and races `worker.close()` (#491 trap 4). Scratch roots use
`tests-common::short_scratch_root` or the UDS path overruns `sun_path` on macOS
(#491 trap 5).

## 9. Docs that must change

- **`core/src/install/plan.rs::render_email_help`, TRAP 3** currently tells
  operators that `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` does **not** apply to this
  channel. That becomes false with this change; it is **rewritten**, not amended
  — it must now state that the anchor applies, and restate the two operator
  constraints it inherits from #492: a `CA:FALSE` (or real-CA-signed) cert, and
  one private origin per worker.
- `persistent_net.rs`'s module doc ("The sidecar runs in `disable_mitm` mode; the
  worker does its own end-to-end TLS and receives no CA") describes the old
  hardcoded behaviour and is replaced by the posture-per-caller contract.
- HANDOVER's KNOWN GAP paragraph and the ROADMAP email entry's KNOWN GAP block
  are retired when this lands.

## 10. Verification and merge gate

Mac: `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`,
with the PG-gated suites run separately under `KASTELLAN_PG_BIN_DIR` (a
full-workspace run under that override flakes `embedding_recall_e2e` at PG
bring-up — the standing macOS gotcha).

**The DGX gate is OWED, not run:** that host is offline as of 2026-07-31. It is
authoritative here because the sandbox tiers skip-as-pass on the Mac and macOS
clippy compiles `#[cfg(target_os = "linux")]` items out. The PR states this
plainly and **does not merge** until the DGX run is green. This change adds no
new `cfg`-gated code, which lowers but does not eliminate the risk.

## 11. Considered and rejected

- **Keep the transparent tunnel, hand the *worker* the operator's origin cert**
  (promoting today's test-only `extra_ca` to operator config). Smaller, and it
  achieves reachability — but it puts the trust anchor in the least-trusted
  component and leaves the leg opaque to the egress boundary, which is the
  opposite of the direction #491/#492 took for the mail tool. Rejected on
  consistency and inspectability.
- **Intercept only when an anchor is configured.** Preserves today's bytes when
  unconfigured, at the cost of the channel's posture becoming a function of
  config — two shapes to test and reason about, and leak scanning that covers the
  leg only sometimes.
- **A dedicated operator env switch for the posture.** A third configuration
  surface for one decision, and a wrong setting fails late and opaquely as a
  `mitm_failed` egress decision rather than at startup.
- **`long_lived` as an enum.** Worth little once the field is named; #494's own
  sketch keeps it a bool.

## 12. Deferred

- Per-service (`host:port`) anchor keying, or a per-host rustls verifier — #492's
  explicit non-goal, unchanged.
- Cert pins for channel sidecars.
- [#497](https://github.com/hherb/kastellan/issues/497) bus unification: untouched
  here, still worth doing before a third channel family lands.
