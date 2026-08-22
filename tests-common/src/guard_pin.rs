//! Structural and behavioural guards for the Shieldstral guard-weights
//! pin (issue #592).
//!
//! The sha256 of the guard model is written down **twice** — once in
//! `core/src/cassandra/guard_model/weights_pin/mod.rs`, which is what
//! `guard calibrate` checks automatically, and once in
//! `scripts/eval/lib/guard-weights.sh`, which is the operator's
//! pre-flight on a host where the tool is not built.
//!
//! The duplication is forced: `kastellan-core` is published to
//! crates.io and cannot `include_str!` a path outside its own
//! directory. That is the same constraint
//! `kastellan_sandbox::guest_kernel_pin` lives under, and this module is
//! the same answer — the sibling of [`crate::microvm`]'s
//! `rust_and_bash_kernel_pins_agree` and `bash_with_pin`.
//!
//! It lives in `tests-common` for one concrete reason: **CI runs
//! `cargo test -p kastellan-tests-common` on every PR.** A pin that
//! drifted between the two files would otherwise only surface on an
//! occasional operator run — and the drift arrives in exactly the PR
//! that changes one of them, which is the case a per-PR gate must
//! catch.
//!
//! # Why the bash half is EXECUTED here, not grepped
//!
//! The first version asserted only that the file *contained* the
//! comparison. That is satisfied by a script that records the sum and
//! never acts on it: flip `return 1` to `return 0` in the mismatch
//! branch and a text-only gate stays green while the pre-flight accepts
//! every file. Given #596 shipped four fail-opens that no test could
//! reach, that is the wrong shape of guard for this file. `bash_with_pin`
//! is ported from [`crate::microvm`] so the accept and reject paths are
//! driven for real, against a 5-byte fixture instead of a 3.6 GB model.

/// The operator-facing bash half of the guard-weights pin.
pub const GUARD_WEIGHTS_LIB: &str = "scripts/eval/lib/guard-weights.sh";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// sha256 of the 5 bytes `hello`, from the standard test vectors.
    ///
    /// Lets the accept and reject paths be exercised against a 5-byte
    /// file instead of the 3.6 GB model, so these stay unit tests.
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// The repository root, derived from this crate's manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests-common has a workspace parent")
            .to_path_buf()
    }

    /// Run `snippet` under `bash` with the guard-weights pin sourced.
    ///
    /// `set -euo pipefail` is deliberate on two counts: the pin is a
    /// *library*, so sourcing it must define functions and nothing else
    /// — a stray top-level command would break every test here, which is
    /// the intended alarm — and `pipefail` is precisely the setting the
    /// pin itself cannot assume its callers have, so running under it
    /// keeps the file honest about not depending on it.
    ///
    /// Ported verbatim in shape from [`crate::microvm`]'s
    /// `bash_with_pin`.
    fn bash_with_pin(snippet: &str) -> std::process::Output {
        let lib = repo_root().join(GUARD_WEIGHTS_LIB);
        let script = format!("set -euo pipefail; source '{}'; {snippet}", lib.display());
        std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("bash is available on both dev hosts")
    }

    /// `(stdout, stderr)` of a snippet, for the assertions below.
    fn run(snippet: &str) -> (String, String) {
        let out = bash_with_pin(snippet);
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// A snippet that makes a `hello` fixture, runs the pre-flight
    /// against it with `$pin` as the expected sum, and prints `RC=<n>`.
    fn check_hello_against(pin: &str) -> (String, String) {
        run(&format!(
            "KASTELLAN_GUARD_WEIGHTS_SHA256={pin}; \
             d=$(mktemp -d); printf hello >\"$d/f\"; rc=0; \
             require_guard_weights \"$d/f\" || rc=$?; echo \"RC=$rc\"; rm -rf \"$d\""
        ))
    }

    // ---------------- the two files must agree ----------------

    /// Pull `NAME="value"` out of the shared bash pin.
    ///
    /// Deliberately strict about the shape: if the assignment is
    /// reformatted, this panics rather than quietly matching nothing
    /// and letting the comparison pass vacuously — a test that can
    /// silently stop testing is worse than no test. Same reasoning, and
    /// same wording, as `microvm::tests::bash_pin_value`.
    ///
    /// Lives inside `mod tests` (as its `microvm` sibling does) because
    /// only tests use it: at module level it is `dead_code` in a
    /// non-test build, which `-D warnings` rejects.
    fn bash_pin_value(body: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let line = body
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{GUARD_WEIGHTS_LIB} has no line starting `{prefix}`"));
        line[prefix.len()..]
            .strip_suffix('"')
            .unwrap_or_else(|| panic!("malformed assignment in {GUARD_WEIGHTS_LIB}: {line}"))
            .to_string()
    }

    fn pin_body() -> String {
        std::fs::read_to_string(repo_root().join(GUARD_WEIGHTS_LIB))
            .unwrap_or_else(|e| panic!("read {GUARD_WEIGHTS_LIB}: {e}"))
    }

    /// The Rust check and the bash pre-flight must agree, or one of
    /// them is verifying against a stale sum — which is #592's own
    /// failure mode one level up: a pin that everybody believes and
    /// nothing reconciles.
    #[test]
    fn rust_and_bash_guard_pins_agree() {
        let body = pin_body();

        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUARD_WEIGHTS_SHA256"),
            kastellan_core::cassandra::guard_model::weights_pin::PINNED_SHA256,
            "the guard-weights sha256 drifted between {GUARD_WEIGHTS_LIB} and \
             core/src/cassandra/guard_model/weights_pin/mod.rs -- bump both together"
        );
        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUARD_WEIGHTS_SIZE_BYTES")
                .parse::<u64>()
                .expect("size pin is a number"),
            kastellan_core::cassandra::guard_model::weights_pin::PINNED_SIZE_BYTES,
            "the guard-weights size drifted between {GUARD_WEIGHTS_LIB} and \
             core/src/cassandra/guard_model/weights_pin/mod.rs -- bump both together"
        );
    }

    // ---------------- the bash half must WORK ----------------

    #[test]
    fn guard_weights_pin_exists_and_sources_cleanly() {
        let lib = repo_root().join(GUARD_WEIGHTS_LIB);
        assert!(lib.is_file(), "missing the shared guard-weights pin: {}", lib.display());
        let out = bash_with_pin("true");
        assert!(
            out.status.success(),
            "sourcing the pin must have no side effects; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The accept path. Without this, mutating the mismatch branch's
    /// `return 1` into `return 0` leaves every other test green while
    /// the pre-flight blesses any file it is pointed at.
    #[test]
    fn the_bash_pin_accepts_a_file_whose_hash_matches() {
        let (stdout, stderr) = check_hello_against(HELLO_SHA256);
        assert!(stdout.contains("RC=0"), "must accept a matching file: {stdout}{stderr}");
        assert!(stderr.is_empty(), "an accepted file must say nothing: {stderr}");
    }

    /// The reject path, and the half the accept test cannot prove:
    /// together they pin that the comparison actually *gates* the
    /// return value rather than merely appearing in the file.
    #[test]
    fn the_bash_pin_rejects_a_file_whose_hash_differs() {
        let (stdout, stderr) = check_hello_against(&"0".repeat(64));
        assert!(stdout.contains("RC=1"), "must reject: {stdout}{stderr}");
        assert!(stderr.contains("NOT the pinned guard model"), "{stderr}");
        assert!(stderr.contains(HELLO_SHA256), "must name the actual hash: {stderr}");
    }

    /// #592's own diagnosis: a same-size mismatch is a different
    /// quantiser run of the RIGHT model, which is the case that looks
    /// correct and is not. A different size is simply the wrong file.
    #[test]
    fn the_bash_pin_distinguishes_a_same_size_mismatch_from_a_different_file() {
        let (_, same_size) = run(&format!(
            "KASTELLAN_GUARD_WEIGHTS_SHA256={pin}; KASTELLAN_GUARD_WEIGHTS_SIZE_BYTES=5; \
             d=$(mktemp -d); printf hello >\"$d/f\"; \
             require_guard_weights \"$d/f\" || true; rm -rf \"$d\"",
            pin = "0".repeat(64)
        ));
        assert!(same_size.contains("DIFFERENT QUANTISER RUN"), "{same_size}");

        let (_, other_size) = check_hello_against(&"0".repeat(64));
        assert!(other_size.contains("different file"), "{other_size}");
        assert!(!other_size.contains("DIFFERENT QUANTISER RUN"), "{other_size}");
    }

    /// A missing file and a missing argument are distinct, and neither
    /// is a mismatch — the same "we could not look" vs "we looked and
    /// it was wrong" split the Rust half keeps in its error variants.
    #[test]
    fn the_bash_pin_separates_a_missing_file_from_a_missing_argument() {
        let (stdout, stderr) =
            run("rc=0; require_guard_weights \"\" || rc=$?; echo \"RC=$rc\"");
        assert!(stdout.contains("RC=2"), "no path is a usage error: {stdout}{stderr}");
        assert!(stderr.contains("no path given"), "{stderr}");

        let (stdout, stderr) = run(
            "d=$(mktemp -d); rc=0; require_guard_weights \"$d/absent\" || rc=$?; \
             echo \"RC=$rc\"; rm -rf \"$d\"",
        );
        assert!(stdout.contains("RC=1"), "{stdout}{stderr}");
        assert!(stderr.contains("no such file"), "{stderr}");
        assert!(stderr.contains("fetch it from"), "must say where to get it: {stderr}");
    }

    /// A hasher that fails must be reported as a READ failure.
    ///
    /// The bug this pins: `sha256sum "$f" | cut …` returns `cut`'s
    /// status, always 0, so the caller's `|| return 1` was dead code,
    /// `$actual` came back empty, and the operator was told the file
    /// was "a different file altogether" — sent hunting for the wrong
    /// model over what was a permissions problem. It failed closed only
    /// because the empty string is not the pin.
    #[test]
    fn the_bash_pin_reports_a_read_failure_as_a_read_failure() {
        let (stdout, stderr) = run(&format!(
            "if [ \"$(id -u)\" = 0 ]; then echo SKIP_ROOT; else \
             KASTELLAN_GUARD_WEIGHTS_SHA256={HELLO_SHA256}; \
             d=$(mktemp -d); printf hello >\"$d/f\"; chmod 000 \"$d/f\"; rc=0; \
             require_guard_weights \"$d/f\" || rc=$?; echo \"RC=$rc\"; \
             chmod 644 \"$d/f\"; rm -rf \"$d\"; fi"
        ));
        if stdout.contains("SKIP_ROOT") {
            eprintln!("[SKIP] the_bash_pin_reports_a_read_failure_as_a_read_failure: running as root");
            return;
        }
        assert!(stdout.contains("RC=1"), "must fail closed: {stdout}{stderr}");
        assert!(stderr.contains("READ failure"), "must not be diagnosed as a wrong model: {stderr}");
        assert!(
            !stderr.contains("different file altogether"),
            "a read error must not be reported as the wrong file: {stderr}"
        );
    }

    /// The one shape that would fail OPEN: with an empty pin the
    /// comparison becomes `[ "$actual" = "" ]`, which succeeds for a
    /// file that was never hashed.
    #[test]
    fn the_bash_pin_refuses_an_empty_pin_rather_than_matching_nothing() {
        let (stdout, stderr) = check_hello_against("\"\"");
        assert!(stdout.contains("RC=2"), "an empty pin must be a usage error: {stdout}{stderr}");
        assert!(stderr.contains("pin constant is empty"), "{stderr}");
    }

    /// Whichever hasher this host has, it must be computing SHA-256.
    ///
    /// A bare `shasum` defaults to **SHA-1**, and a text assertion for
    /// the word "shasum" cannot tell the two apart. Which branch runs
    /// depends on what the host has on `PATH` — both dev hosts happen
    /// to carry GNU `sha256sum`, so in practice this covers that arm
    /// and [`every_shasum_invocation_requests_sha256`] covers the
    /// other. Stated rather than implied, because a test whose name
    /// suggests it proves both would be the overclaim this PR is full
    /// of fixing.
    #[test]
    fn the_bash_pin_computes_sha256_on_this_host() {
        let (stdout, stderr) = run(
            "d=$(mktemp -d); printf hello >\"$d/f\"; \
             _kastellan_sha256_of \"$d/f\"; rm -rf \"$d\"",
        );
        assert_eq!(stdout.trim(), HELLO_SHA256, "not a SHA-256: {stdout}{stderr}");
    }

    /// Verify-only: this must never fetch or repair, the same rule
    /// `require_guest_kernel` follows. A text assertion because it is a
    /// claim about *absence*, which execution cannot demonstrate.
    #[test]
    fn the_bash_pin_never_fetches() {
        let body = pin_body();
        assert!(
            !body.contains("curl") && !body.contains("wget"),
            "{GUARD_WEIGHTS_LIB} must VERIFY only -- never fetch"
        );
    }

    /// Every `shasum` **invocation** must carry `-a 256`.
    ///
    /// Absence-shaped, and scoped to command substitution — which is
    /// how this file actually calls it — because a text scan cannot
    /// safely tell a call from the word appearing in an `echo`.
    /// [`the_bash_pin_computes_sha256_on_this_host`] proves the branch
    /// this host takes; this covers the one it does not, since a bare
    /// `shasum` is SHA-1 and no grep for the word can see the
    /// difference.
    #[test]
    fn every_shasum_invocation_requests_sha256() {
        let body = pin_body();
        let mut invocations = 0;
        for line in body.lines() {
            let code = line.trim_start();
            if code.starts_with('#') || !code.contains("$(shasum") {
                continue;
            }
            invocations += 1;
            assert!(
                code.contains("$(shasum -a 256"),
                "a bare `shasum` is SHA-1, not SHA-256: {line}"
            );
        }
        assert_eq!(
            invocations, 1,
            "expected exactly one `shasum` call site in {GUARD_WEIGHTS_LIB}; \
             if the shape changed, this scan stopped seeing it"
        );
    }
}
