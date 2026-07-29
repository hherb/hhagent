//! Unit tests for the channel-generic polled-worker driver, against a scripted
//! in-process fake — no worker process, no supervisor, no sandbox.
use super::*;
use crate::channel::{ChannelId, ConversationId, OutgoingMessage, PeerId};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Scripted fake worker: a `*.init` method returns a fixed identity, `*.poll`
/// pops the next canned poll RESULT (empty batch when none queued), `*.send`
/// records its params, `*.ack` is accepted (recorded in `log` only — no
/// dedicated field, since only the ack tests care about it). Matched by
/// suffix rather than the literal `t.*` names so the same fake serves both
/// `TEST_SPEC` and the ack-specific specs below (`email.*`, `matrix.*`).
/// While `down` is set every call fails (simulating the supervisor's respawn
/// window, where `PersistentHandle::call` returns `Err`).
struct FakeState {
    down: AtomicBool,
    polls: Mutex<VecDeque<Value>>,
    sends: Mutex<Vec<Value>>,
    init_calls: AtomicUsize,
    /// Every accepted call, in order, as `(method, params)` — the ack tests
    /// use this to assert an ack method was (or was not) invoked, and with
    /// which params.
    log: Mutex<Vec<(String, Value)>>,
}
struct FakeCalls(Arc<FakeState>);
impl WorkerCalls for FakeCalls {
    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.0.down.load(Ordering::SeqCst) {
            anyhow::bail!("persistent worker is restarting");
        }
        self.0.log.lock().unwrap().push((method.to_string(), params.clone()));
        if method.ends_with(".init") {
            self.0.init_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"user_id": "@fake:srv"}));
        }
        if method.ends_with(".poll") {
            return Ok(self
                .0
                .polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"events": []})));
        }
        if method.ends_with(".send") {
            self.0.sends.lock().unwrap().push(params);
            return Ok(json!({}));
        }
        if method.ends_with(".ack") {
            return Ok(json!({}));
        }
        anyhow::bail!("unknown method {method}")
    }
}

fn fake() -> (Arc<FakeState>, Box<dyn WorkerCalls>) {
    let st = Arc::new(FakeState {
        down: AtomicBool::new(false),
        polls: Mutex::new(VecDeque::new()),
        sends: Mutex::new(Vec::new()),
        init_calls: AtomicUsize::new(0),
        log: Mutex::new(Vec::new()),
    });
    (st.clone(), Box::new(FakeCalls(st)))
}

fn test_parse(v: Value) -> anyhow::Result<Vec<PolledEvent>> {
    let evs = v["events"].as_array().cloned().unwrap_or_default();
    evs.into_iter()
        .map(|e| {
            Ok(PolledEvent {
                peer: e["peer"].as_str().ok_or_else(|| anyhow::anyhow!("bad event"))?.into(),
                conversation: e["conversation"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("bad event"))?
                    .into(),
                body: e["body"].as_str().ok_or_else(|| anyhow::anyhow!("bad event"))?.into(),
                evidence: None,
                ack_token: e["ack_token"].as_str().map(String::from),
            })
        })
        .collect()
}
fn test_encode(m: &OutgoingMessage) -> Value {
    json!({"conversation": m.conversation.0, "body": m.body})
}
const TEST_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "t",
    init_method: "t.init",
    poll_method: "t.poll",
    send_method: "t.send",
    ack_method: None,
    poll_timeout_ms: 5,
};

fn spawn_test_driver(
    calls: Box<dyn WorkerCalls>,
) -> (PolledWorkerDriver, Value) {
    PolledWorkerDriver::spawn(TEST_SPEC, calls, test_parse, test_encode, None, ChannelId("t".into()))
        .expect("driver spawn")
}

/// Spec for the ack-bearing tests below: an email-fallback-shaped channel
/// whose worker keeps a server-side polling cursor that must be advanced.
fn spec_with_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "email",
        init_method: "email.init",
        poll_method: "email.poll",
        send_method: "email.send",
        ack_method: Some("email.ack"),
        poll_timeout_ms: 50,
    }
}

/// Spec shaped like Matrix's real [`crate::channel::matrix::wire::MATRIX_POLLED_SPEC`]:
/// `ack_method: None`. Used to pin that a spec without an ack method never
/// triggers an ack call, regardless of what the events carry.
fn spec_without_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "matrix",
        init_method: "matrix.init",
        poll_method: "matrix.poll",
        send_method: "matrix.send",
        ack_method: None,
        poll_timeout_ms: 50,
    }
}

fn encode_test_ack(cursor: &str) -> Value {
    json!({ "cursor": cursor })
}

#[test]
fn spawn_surfaces_identity_via_one_init_call() {
    let (st, calls) = fake();
    let (driver, identity) = spawn_test_driver(calls);
    assert_eq!(identity["user_id"], "@fake:srv");
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 1, "exactly one init (login proof)");
    drop(driver);
}

#[test]
fn polled_events_are_forwarded_as_incoming_messages() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "hello"}
    ]}));
    let (mut driver, _identity) = spawn_test_driver(calls);
    let msg = driver.inbound_rx.blocking_recv().expect("inbound message");
    assert_eq!(msg.channel, ChannelId("t".into()));
    assert_eq!(msg.peer, PeerId("@me:srv".into()));
    assert_eq!(msg.conversation, ConversationId("!room:srv".into()));
    assert_eq!(msg.body, "hello");
}

#[test]
fn malformed_poll_result_is_skipped_not_fatal() {
    let (st, calls) = fake();
    // First a batch test_parse rejects, then a good one: the driver must skip
    // the bad batch (worker bug, not a death) and forward the good one.
    st.polls.lock().unwrap().push_back(json!({"events": [{"peer": 42}]}));
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "after-bad"}
    ]}));
    let (mut driver, _identity) = spawn_test_driver(calls);
    let msg = driver.inbound_rx.blocking_recv().expect("inbound message");
    assert_eq!(msg.body, "after-bad");
}

#[test]
fn init_failure_fails_spawn() {
    let (st, calls) = fake();
    st.down.store(true, Ordering::SeqCst);
    let res = PolledWorkerDriver::spawn(
        TEST_SPEC,
        calls,
        test_parse,
        test_encode,
        None,
        ChannelId("t".into()),
    );
    assert!(res.is_err(), "init error must fail the spawn (login proof)");
}

/// Bounded wait for a condition, so a regression fails the test rather than
/// hanging the suite.
fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition not reached within 5s");
}

#[test]
fn outbound_message_is_delivered_encoded() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    driver
        .outbound_tx
        .send(OutgoingMessage {
            channel: ChannelId("t".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "pong".into(),
        })
        .unwrap();
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    let sent = st.sends.lock().unwrap();
    assert_eq!(sent[0], json!({"conversation": "!room:srv", "body": "pong"}));
}

#[test]
fn pending_send_is_retained_across_a_down_window_and_delivered_once() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    // Worker goes down (supervisor respawn window: every call errors).
    st.down.store(true, Ordering::SeqCst);
    driver
        .outbound_tx
        .send(OutgoingMessage {
            channel: ChannelId("t".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "survives".into(),
        })
        .unwrap();
    // Give the driver a few retry slices while down: nothing may be delivered.
    std::thread::sleep(Duration::from_millis(600));
    assert!(st.sends.lock().unwrap().is_empty(), "no delivery while worker is down");
    // Worker comes back: the retained message must arrive exactly once.
    st.down.store(false, Ordering::SeqCst);
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    std::thread::sleep(Duration::from_millis(100)); // catch double-delivery
    let sent = st.sends.lock().unwrap();
    assert_eq!(sent.len(), 1, "retained send must be delivered exactly once");
    assert_eq!(sent[0]["body"], "survives");
}

#[test]
fn dropping_endpoints_stops_the_driver_thread() {
    let (_st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(inbound_rx);
    drop(outbound_tx);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver thread must exit once both endpoints are dropped");
}

#[test]
fn dropping_endpoints_during_a_down_window_stops_the_driver_thread() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    st.down.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100)); // let it enter the retry loop
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(inbound_rx);
    drop(outbound_tx);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver must exit from the retry loop when endpoints drop");
}

#[test]
fn ack_is_called_after_the_event_reaches_the_bus() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi", "ack_token": "42"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");

    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
    let log = st.log.lock().unwrap();
    let entry = log.iter().find(|(m, _)| m == "email.ack").cloned().unwrap();
    assert_eq!(entry.1["cursor"], "42", "ack must carry the event's own cursor");
}

#[test]
fn no_ack_method_means_no_ack_call() {
    // Matrix must be untouched: its spec has ack_method: None.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!r", "body": "hi"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(),
        calls,
        test_parse,
        test_encode,
        None,
        ChannelId("matrix".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "@me:srv");
    // No ack ever fires here, so there is no bus-side signal to wait on;
    // give the (busy-spinning) driver a few loop iterations before asserting
    // absence, same pattern as the retention test above.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
        "a spec without ack_method must never call ack"
    );
}

#[test]
fn an_event_without_an_ack_token_is_not_acked() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        ChannelId("email".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");
    std::thread::sleep(Duration::from_millis(150));
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
}
