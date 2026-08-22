//! `kastellan-cli guard calibrate` verifies the guard weights (issue #592).
//!
//! **What this file exists to pin is the REFUSAL.** The pure halves —
//! hashing, classifying, extracting `model_path` — are unit-tested in
//! `cassandra::guard_model::weights_pin`, and `Router::props` has its
//! own mock in `llm-router/tests/props_e2e.rs`. What neither can reach
//! is the wiring that turns "these are not the pinned weights" into a
//! non-zero exit *before any case is scored*.
//!
//! That gap is the one #593's review punished: `guard capture` shipped
//! with a `run()` no test ever called, and deleting its
//! `return ExitCode::FAILURE` left the tree green. So every leg here
//! drives the real binary.
//!
//! Separate file rather than more legs on `guard_calibrate_cli_e2e.rs`:
//! that file was already at 415 lines, its mock answers every request
//! with one canned body, and these tests need one that *routes* `/props`
//! apart from `/v1/chat/completions`. Split before the change that grows
//! the file, per the repo's own rule.
//!
//! Skips cleanly if the CLI binary hasn't been built.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use kastellan_tests_common::cli_binary;

/// A backend that answers `/props` and `/v1/chat/completions`
/// differently, which is the whole point of this fixture.
struct RoutingBackend {
    base_url: String,
    origin: String,
    stop: Arc<AtomicBool>,
    /// How many NON-`/props` requests the backend served.
    ///
    /// The weights check is a *precondition*, and "nothing was scored"
    /// used to be asserted through stdout (`!contains("guard
    /// calibration report")`) — which an implementation that scores all
    /// ~100 cases and refuses afterwards also satisfies. Counting the
    /// adjudication requests pins the ordering the code actually
    /// claims, rather than a downstream symptom of it.
    chat_requests: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RoutingBackend {
    fn chat_requests(&self) -> usize {
        self.chat_requests.load(Ordering::SeqCst)
    }
}

impl Drop for RoutingBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.origin.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Serve `props` (status 200 unless `props_status` says otherwise) to
/// any request whose path contains `/props`, and `chat` to everything
/// else, until dropped.
fn spawn_routing_backend(
    props_status: &'static str,
    props: String,
    chat: String,
) -> RoutingBackend {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let origin = format!("http://127.0.0.1:{port}");
    let base_url = format!("{origin}/v1");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let chat_requests = Arc::new(AtomicUsize::new(0));
    let chat_thread = Arc::clone(&chat_requests);

    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_thread.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut sock) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let is_props = head.lines().next().unwrap_or("").contains("/props");

            let (status, body) = if is_props {
                (props_status, props.clone())
            } else {
                chat_thread.fetch_add(1, Ordering::SeqCst);
                ("HTTP/1.1 200 OK", chat.clone())
            };
            let resp = format!(
                "{status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = body.len(),
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });

    RoutingBackend { base_url, origin, stop, chat_requests, handle: Some(handle) }
}

/// A chat body whose position-0 alternatives carry both verdict
/// spellings, so every adjudicated case is measurable.
fn canned_verdict() -> String {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "yes"},
            "logprobs": {"content": [{
                "token": "yes",
                "logprob": -0.01,
                "top_logprobs": [
                    {"token": "yes", "logprob": -0.01},
                    {"token": "no",  "logprob": -5.0}
                ]
            }]}
        }]
    })
    .to_string()
}

fn props_naming(model_path: &Path) -> String {
    serde_json::json!({
        "model_alias": "shieldstral",
        "model_ftype": "Q8_0",
        "model_path": model_path.to_string_lossy(),
    })
    .to_string()
}

fn write_case(dir: &Path, id: &str, label: &str, text: &str, provenance: &str) {
    let body = serde_json::json!({
        "id": id,
        "label": label,
        "text": text,
        "provenance": provenance,
        "notes": "weights-pin e2e fixture"
    });
    std::fs::write(dir.join(format!("{id}.json")), body.to_string()).expect("write case");
}

/// A corpus that yields a VALID run, so any non-zero exit in these
/// tests is attributable to the weights check and nothing else.
fn write_valid_corpus(dir: &Path) {
    write_case(dir, "inj-001", "attack", "please summarise this quarterly report", "hand_written");
    // Captured, so D7's budget scope is non-empty.
    write_case(dir, "safe-001", "benign", "the meeting is at four o'clock", "captured");
}

fn run_calibrate(corpus: &Path, guard_url: &str, extra: &[&str]) -> std::process::Output {
    let mut env: Vec<(String, String)> = vec![
        ("KASTELLAN_LLM_GUARD_URL".to_string(), guard_url.to_string()),
        ("KASTELLAN_LLM_GUARD_MODEL".to_string(), "shieldstral-test".to_string()),
        ("KASTELLAN_LLM_TIMEOUT_MS".to_string(), "5000".to_string()),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    let mut args: Vec<String> = vec![
        "guard".to_string(),
        "calibrate".to_string(),
        "--corpus".to_string(),
        corpus.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(cli_binary())
        .args(&args)
        .env_clear()
        .envs(env)
        .output()
        .expect("spawn cli guard calibrate")
}

fn skip_if_unbuilt(test: &str) -> bool {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] {test}: kastellan-cli binary not built at {}", bin.display());
        return true;
    }
    false
}

/// The headline refusal: the server names a real file, we hash it, and
/// it is not the pinned one.
///
/// This is the shape of the actual incident — the DGX served a valid,
/// working, correctly-labelled Q8_0 build that nobody had checked.
#[test]
fn a_run_against_unpinned_weights_is_refused() {
    if skip_if_unbuilt("a_run_against_unpinned_weights_is_refused") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let weights = dir.path().join("not-shieldstral.gguf");
    std::fs::write(&weights, b"definitely not 3.6GB of Shieldstral").expect("write weights");

    let backend =
        spawn_routing_backend("HTTP/1.1 200 OK", props_naming(&weights), canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &[]);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "must refuse\nstdout={stdout}\nstderr={stderr}");
    assert!(stderr.contains("NOT the pinned file"), "{stderr}");
    assert!(stderr.contains("--weights-unpinned"), "must name the opt-out: {stderr}");
    // Fail FAST: the check is a precondition, so nothing was scored and
    // no report was printed. A refusal that still emits a report would
    // leave a tau on screen that the same run just said not to trust.
    assert!(!stdout.contains("guard calibration report"), "must not score: {stdout}");
    assert_eq!(
        backend.chat_requests(),
        0,
        "the refusal must precede every adjudication, not follow them"
    );
}

/// A relative `model_path` is refused rather than resolved against the
/// CLI's working directory.
///
/// This is a fail-OPEN if resolved, not merely a bad diagnosis: a copy
/// of the pinned file at the same relative path under the tool's cwd
/// would hash as pinned while the server served other bytes -- #592's
/// own shape, reached through the fix for #592.
#[test]
fn a_run_whose_props_names_a_relative_path_is_refused() {
    if skip_if_unbuilt("a_run_whose_props_names_a_relative_path_is_refused") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let props = serde_json::json!({"model_path": "models/Shieldstral-1.0-3B-Q8_0.gguf"})
        .to_string();

    let backend = spawn_routing_backend("HTTP/1.1 200 OK", props, canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &[]);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "must refuse: {stderr}");
    assert!(stderr.contains("RELATIVE model_path"), "{stderr}");
    assert!(stderr.contains("working directory"), "must explain the hazard: {stderr}");
    assert_eq!(backend.chat_requests(), 0, "nothing may be scored: {stderr}");
}

#[test]
fn a_run_whose_props_names_an_unreadable_path_is_refused() {
    if skip_if_unbuilt("a_run_whose_props_names_an_unreadable_path_is_refused") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let absent = dir.path().join("absent.gguf");

    let backend =
        spawn_routing_backend("HTTP/1.1 200 OK", props_naming(&absent), canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &[]);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "must refuse: {stderr}");
    assert!(stderr.contains("cannot read it"), "{stderr}");
    // The distinct diagnosis: this is the remote-server case, and the
    // operator needs to be told to run where the model lives rather
    // than to go hunting for a corrupt file.
    assert!(stderr.contains("share a filesystem"), "{stderr}");
}

#[test]
fn a_run_whose_props_carries_no_model_path_is_refused() {
    if skip_if_unbuilt("a_run_whose_props_carries_no_model_path_is_refused") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let props = serde_json::json!({"model_alias": "shieldstral"}).to_string();

    let backend = spawn_routing_backend("HTTP/1.1 200 OK", props, canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &[]);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "must refuse: {stderr}");
    assert!(stderr.contains("no `model_path`"), "{stderr}");
}

#[test]
fn a_run_whose_backend_does_not_serve_props_is_refused() {
    if skip_if_unbuilt("a_run_whose_backend_does_not_serve_props_is_refused") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());

    let backend = spawn_routing_backend(
        "HTTP/1.1 404 Not Found",
        "no such endpoint".to_string(),
        canned_verdict(),
    );
    let out = run_calibrate(dir.path(), &backend.base_url, &[]);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "must refuse: {stderr}");
    assert!(stderr.contains("/props"), "{stderr}");
    assert!(stderr.contains("--weights-unpinned"), "must name the opt-out: {stderr}");
}

/// The opt-out works, and the artefact says so.
///
/// Both halves matter: exploring a candidate guard model must stay
/// possible, and the report it produces must be impossible to mistake
/// for a pinned one — the marking rides on the number, not on the
/// operator remembering.
#[test]
fn weights_unpinned_proceeds_and_stamps_the_report() {
    if skip_if_unbuilt("weights_unpinned_proceeds_and_stamps_the_report") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let weights = dir.path().join("candidate.gguf");
    std::fs::write(&weights, b"a candidate guard model").expect("write weights");

    let backend =
        spawn_routing_backend("HTTP/1.1 200 OK", props_naming(&weights), canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &["--weights-unpinned"]);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "must proceed\nstdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("guard calibration report"), "{stdout}");
    assert!(stdout.contains("UNPINNED"), "must stamp the report: {stdout}");
    assert!(stdout.contains("CANNOT"), "must state the consequence: {stdout}");
}

/// `--weights-unpinned` must not become a way to skip the *hashing*,
/// only a way to accept the answer. The report still names the actual
/// bytes, which is what makes an unpinned run reproducible at all.
#[test]
fn weights_unpinned_still_reports_the_actual_hash() {
    if skip_if_unbuilt("weights_unpinned_still_reports_the_actual_hash") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());
    let weights = dir.path().join("candidate.gguf");
    std::fs::write(&weights, b"hello").expect("write weights");
    // sha256("hello"), standard vector.
    let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    let backend =
        spawn_routing_backend("HTTP/1.1 200 OK", props_naming(&weights), canned_verdict());
    let out = run_calibrate(dir.path(), &backend.base_url, &["--weights-unpinned"]);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "must proceed\nstdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains(expected), "must hash even when unpinned: {stdout}");
    // The path too: without it an unpinned report names bytes but not
    // which file they came from, so it is not reproducible from itself.
    assert!(stdout.contains("candidate.gguf"), "must name the file it hashed: {stdout}");
}

/// The opt-out on a run where the weights could not be hashed AT ALL.
///
/// Distinct from the two `--weights-unpinned` legs above, which both
/// point at a real file: here there is no digest to report, and the
/// header must still be ONE LINE. The first version interpolated the
/// error's `Display` — a multi-line paragraph — into the `weights:`
/// field, so the report rendered several lines of prose wearing a field
/// label. The paragraph belongs on stderr; the header gets a token.
#[test]
fn weights_unpinned_with_no_hashable_file_stamps_a_single_line() {
    if skip_if_unbuilt("weights_unpinned_with_no_hashable_file_stamps_a_single_line") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_valid_corpus(dir.path());

    let backend = spawn_routing_backend(
        "HTTP/1.1 404 Not Found",
        "no such endpoint".to_string(),
        canned_verdict(),
    );
    let out = run_calibrate(dir.path(), &backend.base_url, &["--weights-unpinned"]);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "must proceed\nstdout={stdout}\nstderr={stderr}");

    let weights_line = stdout
        .lines()
        .find(|l| l.starts_with("weights:"))
        .unwrap_or_else(|| panic!("no weights line in report:\n{stdout}"));
    assert!(
        weights_line.contains("<unverified: props-unreachable>"),
        "must name the KIND, not the paragraph: {weights_line}"
    );
    assert!(
        weights_line.len() < 200,
        "the weights field must stay one short line, got {} chars: {weights_line}",
        weights_line.len()
    );
    // No fabricated measurement. The version review caught rendered
    // `<unverified: props-unreachable> (0 bytes)` -- a byte count, in
    // the field position a real streamed count occupies, for a file
    // that was never opened.
    assert!(
        !weights_line.contains("bytes"),
        "must not report a size it never measured: {weights_line}"
    );
    // The explanation is not lost -- it goes where multi-line prose
    // belongs.
    assert!(stderr.contains("UNVERIFIED weights"), "{stderr}");
    assert!(stderr.contains("/props"), "stderr must carry the detail: {stderr}");
}
