//! ---------------------------------------------------------------------------
//! Outage bookkeeping (#517 review). Escalation is a log line and nothing else,
//! so these drive `note_outage` — the pure half of `escalate_if_due` — with
//! scripted `Instant`s. That is the only seam from which the sequence that
//! matters here (died → recovered → worked for hours → flapped) is observable.
//! ---------------------------------------------------------------------------

use std::time::Duration;

use super::*;

/// The regression: a channel that died after working must NOT leave the
/// escalator timing an outage, because a successful restart never clears it.
///
/// The escalator learns about health only when a *stable* channel dies, so an
/// outage opened eagerly at that death survives the restart that fixed it. The
/// next flap then reports every healthy hour in between as downtime — in the
/// one line whose text asserts that nothing sent to the channel has been
/// received for that long, and past its threshold on the very first event, so
/// it fires immediately rather than after five minutes of real silence.
#[test]
fn a_death_that_recovers_leaves_no_outage_open() {
    let death = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    // A channel that had been working stops. The restart after it succeeds, so
    // nothing else ever touches the escalator.
    assert_eq!(note_outage(&mut esc, Outage::Ends, death), None);

    // Four healthy hours later it flaps. This is the FIRST event of a new
    // outage, which is zero seconds old.
    let flap = death + Duration::from_secs(4 * 3600);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, flap),
        None,
        "four hours the channel spent WORKING must not be reported as downtime"
    );

    // And it escalates on its own schedule, timed from the flap rather than
    // from the death four hours earlier.
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, flap + Duration::from_secs(301)),
        Some(Duration::from_secs(301)),
        "the new outage is timed from its own first event"
    );
}

/// The price of the guarantee above, stated as a test so it is a decision
/// rather than an oversight: when the restart does NOT succeed, the outage is
/// dated from the first failed attempt instead of from the death — one backoff
/// delay late (1 s for the first restart, 60 s at the cap).
#[test]
fn a_death_whose_restart_fails_times_the_outage_from_that_failure() {
    let death = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    note_outage(&mut esc, Outage::Ends, death);
    // The restart one second later fails, and every attempt after it.
    let first_failure = death + Duration::from_secs(1);
    assert_eq!(note_outage(&mut esc, Outage::Continues, first_failure), None);

    // 300 s after the DEATH is only 299 s after the first failed attempt, so
    // the loud line is still one second away.
    assert_eq!(note_outage(&mut esc, Outage::Continues, death + Duration::from_secs(300)), None);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, first_failure + Duration::from_secs(300)),
        Some(Duration::from_secs(300)),
        "dated from the first failed restart, not the death"
    );
}

/// A death that was already inside an outage (a flap) extends it rather than
/// re-dating it — otherwise a channel that comes up for a second every minute
/// would reset the clock forever and never escalate at all.
#[test]
fn a_flapping_death_extends_the_outage_it_is_already_in() {
    let base = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    note_outage(&mut esc, Outage::Continues, base);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, base + Duration::from_secs(301)),
        Some(Duration::from_secs(301)),
        "downtime is measured from the outage's first event, flaps included"
    );
}
