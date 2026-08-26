//! Running the boot probe: the IO half of wiring-spec D9, and issue
//! [#624]'s sampling loop.
//!
//! Split out of [`super::boot`] when #624 pushed that file past the
//! 500-LOC cap — the same movement `timeout.rs` made at 503 when it
//! pushed `classify_pin` down to `basis.rs`, and for the same reason
//! beyond the line count: [`super::boot`] is about whether a configured
//! tier is **usable**, and this is about **measuring** one. The boot
//! sequence calls [`run_probe`] once and is otherwise free of sockets.
//!
//! Almost nothing here is worth a test with a server, and that is by
//! design: which sample wins and when to stop are pure and live in
//! [`timeout::summarise`] and [`timeout::more_samples_wanted`]. What is
//! left is a loop, a clock, and the one predicate only the transport can
//! answer ([`is_timeout`]).
//!
//! [#624]: https://github.com/hherb/kastellan/issues/624

use kastellan_llm_router::RouterError;

use super::super::timeout::{self, ProbeOutcome, ProbeSummary};
use super::super::GuardClient;
use super::{error_kind, GuardErrorKind};

/// Run the boot probe — [`timeout::PROBE_SAMPLES`] samples — and fold
/// them into one summary.
///
/// The IO half of D9, and the only part of issue [#624]'s fix that
/// touches IO: everything about *which* sample wins and *when to stop*
/// is pure and lives in [`timeout::summarise`] and
/// [`timeout::more_samples_wanted`], so this function contributes a
/// loop and a clock and nothing else worth testing with a server.
///
/// **Each sample gets its own cache-buster.** Reusing one across
/// samples would send byte-identical prompts and serve every sample
/// after the first from the prefix cache — which on a backend that does
/// not report `cached_tokens` reads as an enormous throughput that
/// [`timeout::summarise`] would then *prefer*. See
/// [`timeout::sample_cache_buster`].
///
/// [#624]: https://github.com/hherb/kastellan/issues/624
pub(super) async fn run_probe(client: &GuardClient, cache_buster: &str) -> ProbeSummary {
    let started = std::time::Instant::now();
    let mut samples = Vec::with_capacity(timeout::PROBE_SAMPLES);
    while timeout::more_samples_wanted(samples.len(), elapsed_ms(started)) {
        let buster = timeout::sample_cache_buster(cache_buster, samples.len());
        samples.push(run_one_sample(client, &buster).await);
    }
    timeout::summarise(&samples)
}

/// Wall clock since `started`, saturating at [`u64::MAX`] ms.
///
/// Split out so the cast is in one place: `Duration::as_millis` is
/// `u128` and [`timeout::more_samples_wanted`] takes `u64`, and an
/// `as u64` truncation of a value that large would wrap a very long
/// probe back under the budget and loop again.
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// One probe sample: send the document, classify what came back.
///
/// A transport failure here is [`ProbeOutcome::Failed`] — **except** the
/// request timeout that [`timeout::PROBE_BUDGET_MS`] imposes, which is
/// [`ProbeOutcome::Saturated`], because an overrun budget is a
/// measurement of slowness rather than a missing measurement. A
/// *connect* timeout is `Failed`, not `Saturated`: see [`is_timeout`].
async fn run_one_sample(client: &GuardClient, cache_buster: &str) -> ProbeOutcome {
    let document = timeout::probe_document(cache_buster);
    match client.timed_probe(&document).await {
        Ok(reading) => timeout::probe_sample(reading),
        Err(e) => {
            // **Logged here, because this is the only place the reason
            // exists.** `ProbeOutcome::Failed` carries it, and
            // `derive_guard_timeout` then drops it on the floor — so
            // without this line the diagnosis is formatted and thrown
            // away. It matters more than its `info!`-shaped basis
            // suggests: /props answered (the tier got this far), so a
            // failure HERE is a failure of the exact call every dispatch
            // will make, and predicts a tier that fails open on all of
            // them. `TimeoutBasis::coverage_finding` says so at `warn!`.
            tracing::warn!(
                target: "kastellan::guard_model",
                error = %e,
                timed_out = is_timeout(&e),
                "guard boot probe failed"
            );
            timeout::probe_error_outcome(is_timeout(&e), e.to_string(), timeout::PROBE_BUDGET_MS)
        }
    }
}

/// Did this router error come from the **request** budget running out?
///
/// Asks `reqwest` directly rather than matching its `Display` text.
/// The text form works today — `RouterError::Transport` appends
/// `" [request timed out]"` via `transport_kind_tag` — but it is the
/// wrong predicate for a load-bearing distinction: a reqwest upgrade
/// that reworded the tag would silently reclassify every probe timeout
/// as [`ProbeOutcome::Failed`], which takes the **floor** instead of
/// the **ceiling** and so hands the slowest hosts the shortest guard
/// timeout. That is a fail-open, and it would show up as nothing at
/// all.
///
/// **A connect timeout is excluded, and the exclusion is the whole
/// reason this is not a one-liner.** `reqwest::Error::is_timeout` walks
/// the source chain for `io::ErrorKind::TimedOut`, and a *connect*
/// timeout puts one there — so `is_timeout()` and `is_connect()` are
/// **both** true for it. `Router::with_policy` caps connect at 5 s
/// independently of the request budget, so without this clause a
/// transient 5 s connect stall on a perfectly fast host would be read
/// as [`ProbeOutcome::Saturated`]: derive the 120 s ceiling, fire the
/// "this host cannot adjudicate a worst-case document" warning, and
/// write a throughput nobody measured into `policy / guard_tier.boot`.
///
/// The two errors mean opposite things. A request timeout says *the
/// backend is slow*, which is a measurement. A connect timeout says
/// *the backend was not reachable*, which says nothing about its
/// throughput and must take the floor with every other failure.
///
/// **Defined through [`error_kind::classify`] rather than re-deriving
/// the predicate.** #619's review found this function and
/// `classify_transport` answering the *same* question about the *same*
/// reqwest pair two different ways, ~300 lines apart, with nothing able
/// to notice — the audit row said `timeout` where this said "not a
/// timeout". Now there is one classification and two readings of it:
/// [`GuardErrorKind::ConnectTimeout`] is a distinct arm, and this asks
/// only for the request-budget one. Reordering the classifier can no
/// longer silently change what the probe measures.
fn is_timeout(e: &RouterError) -> bool {
    matches!(error_kind::classify(e), GuardErrorKind::Timeout)
}
