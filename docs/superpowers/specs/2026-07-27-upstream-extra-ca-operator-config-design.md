# Upstream extra-CA operator config — design (#492)

**Status:** implemented, PR on `feat/492-upstream-extra-ca-wiring`.
**Predecessor:** [#491 / PR #493](https://github.com/hherb/kastellan/pull/493) —
`2026-07-25-mail-worker-force-routed-roundtrip-design.md`.

## 1. Problem

A force-routed worker never dials an origin itself. Its traffic goes to a
per-worker egress-proxy sidecar, which terminates the worker's TLS (MITM),
inspects it, and opens a **second** TLS connection to the real origin — the
*re-origination (upstream) leg*. That leg validates the origin's certificate
against the **webpki root store** (the public CAs).

A private, self-signed origin — the operator's own localmail — therefore cannot
be reached force-routed at all. #491 established the *capability* to hand the
proxy one extra trust anchor for that leg
(`build_upstream_client_config(pins, extra_ca_path)` ←
`KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA` ← `proxy_policy`/`spawn_sidecar` ←
`NetWorkerSpawn.upstream_extra_ca`) and proved it end-to-end, but deliberately
stopped short of production: `force_route.rs` passes `None`.

This slice supplies the missing operator configuration.

## 2. Non-goals

* No change to the #491 capability, the proxy, or the containment boundary.
* No per-host verifier in rustls (`ServerCertVerifier` selecting a root set by
  SNI). That is the most correct way to scope the anchor and the most work; §4
  reaches the same safety property far more cheaply for the shapes we support.
* No certificate *validity* checking in core. The proxy stays the authority on
  the PEM's content; core does one shallow startup probe (§5).

## 3. Shape of the config

`KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`, a JSON object keyed by origin host:

```
KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA={"10.0.0.3":"/home/me/.config/localmail/tls/cert.pem"}
```

Host-keyed, mirroring `KASTELLAN_EGRESS_CERT_PINS` and its
`pins_for(&allowlist)` selection, so the two operator configs read the same way
and share `host_of_endpoint`. Unset / blank / `{}` → no anchors, and every code
path is byte-identical to before.

## 4. The trust-scope rule (the crux)

`build_upstream_client_config` adds the anchor to that sidecar's **whole**
upstream `RootCertStore`. So host-keying bounds *which sidecar* holds the
anchor, but **not which hosts within it**: if a worker's allowlist mixed the
private origin with a public host, the operator's CA could impersonate that
public host.

The PR #493 review asked for this to be *enforced* rather than documented, on
the grounds that "safe because of how we happen to use it" stops being true the
moment the thing becomes operator-settable. `select_ca_for_allowlist` therefore
hands out an anchor only when **all** of these hold, and refuses otherwise:

| Rule | Refusal | Why |
| --- | --- | --- |
| Exactly one configured origin appears in the worker's allowlist | `MultipleKeyedHosts` | A sidecar takes at most one anchor; choosing silently would be arbitrary. |
| That origin is the **only** host in the allowlist | `MixedAllowlist` | The anchor is trusted for every host the sidecar can reach. |
| The origin is a **private/loopback IP literal** | `NotPrivateOrigin` | See below. |

**Why an IP literal specifically.** Two independent reasons converge:

1. Widening trust is only defensible for an origin the operator physically
   controls; a public address is not ours to vouch for.
2. The egress proxy's SSRF guard denies any *hostname* that resolves into a
   private range, and lets only operator-allowlisted **IP literals** through its
   carve-out (`proxy::decide`). A name-keyed private origin would therefore be
   unreachable no matter what we trusted — so accepting one would create exactly
   the false reachability belief this family of issues exists to correct.

Privateness is decided by `kastellan_net_classify::is_denied_range` — the same
predicate the proxy's SSRF guard uses, so the two cannot drift. This is core's
first use of that crate; it is a pure in-workspace dependency.

**A refusal fails the spawn.** `spawn_worker_maybe_forced` maps a selection
error to `ToolHostError::Io` naming the env var and the worker. Silently
dropping the anchor was rejected: the operator would then have configured an
anchor, received no error, and still be unable to reach their origin.

## 5. Startup validation

`from_env` parses the JSON and then **reads every named PEM immediately**:

* unreadable → `UpstreamCaFileError::Unreadable`, daemon aborts;
* no `-----BEGIN CERTIFICATE-----` block → `NoCertificate`, daemon aborts
  (catches the realistic typo of pasting the private key);
* otherwise a `tracing::warn!` records the widened origin, the path, the
  certificate count, and the `CA:TRUE` trap.

Post-#493 the proxy already fails closed on a bad PEM *and* that failure is
visible to the host. The value added here is **when**: at daemon startup rather
than on the first force-routed dispatch.

The probe is deliberately shallow. It cannot prove the anchor will validate the
origin, and it deliberately does not reject `CA:TRUE` — a genuine CA that signed
a separate leaf is `CA:TRUE` and is a perfectly good anchor. Distinguishing the
good case from the bad one requires knowing what the *origin serves*, which no
amount of reading the file can tell us. So the trap is documented loudly instead
(§6) rather than guessed at. Adding an X.509 parser to the trusted core for a
diagnostic it cannot conclusively make was judged a poor trade.

## 6. The `CA:TRUE` trap, and why it gets three homes

A self-signed certificate marked `basicConstraints CA:TRUE` **and served as its
own end-entity certificate** is rejected by rustls-webpki at handshake time with
`CaUsedAsEndEntity`, even though `openssl verify` accepts it. `openssl req
-x509` produces exactly that shape by default, and **the live DGX localmail cert
is this shape** — the #491 live tier reached the origin (the SSRF carve-out
dialled the private literal, the worker↔proxy MITM leg was fine) and failed only
on re-origination.

It fails **late and opaquely**, as a `mitm_failed: …` egress decision rather
than a startup error, so it is written down in the three places an operator
looks: `build_upstream_client_config` (#491), the startup WARN, and now
`kastellan.env` — which is where an operator actually looks. Working shapes: a
real CA that signed a separate origin leaf, or a self-signed leaf with
`CA:FALSE`.

**Consequence for deployment:** this slice makes the deployed path *possible*;
reaching the DGX localmail end-to-end additionally requires the operator to
regenerate that cert as a `CA:FALSE` leaf. That is an operator action, recorded
in HANDOVER.

## 7. Why a builder, not a 5th constructor parameter

`ForceRoutingConfig::new` has five call sites outside this module, four of them
in `cfg(linux)` Firecracker e2e files the dev Mac cannot compile. A new
positional parameter would mean a mechanical edit to files only the DGX can
check ([[cfg-linux-e2e-deadcode-dgx-clippy]]), for no gain. `with_upstream_cas`
leaves every existing caller byte-identical and keeps the default "no anchor",
and it moves in the direction [#494](https://github.com/hherb/kastellan/issues/494)
argues for rather than against it.

## 8. Test posture

35 unit tests. All six security assertions were **proved to fail** against
deliberately weakened code (trust-scope and private-origin rules deleted) before
being restored — the house rule from #479. The spawn-path test additionally
pins that a refusal happens *before* the backend is touched, by asserting the
error names the env var rather than the sandbox.

`help_block_is_entirely_commented_out` pins that every line of the
`kastellan.env` block stays a comment: if the example line ever lost its `#`, a
fresh install would silently widen upstream trust to a path that does not exist
on that host — a setting nobody asked for, in the one file operators do not
re-read.

## 9. Deferred

* Per-host (SNI-selected) root sets in the verifier — §2.
* Requiring a cert pin alongside an extra CA, so the widened anchor cannot be
  used for anything unpinned. Redundant while §4's single-private-origin rule
  holds; worth revisiting if that rule is ever relaxed.
* A live DGX tier driving `from_env` against the real localmail. Blocked on the
  §6 cert regeneration, not on code.
