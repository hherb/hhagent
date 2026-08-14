//! The `EnvironmentFile=` grammar, in one place.
//!
//! Deliberately `cfg`-free and shared rather than per-backend. The launchd
//! backend folds these pairs into a plist (launchd has no `EnvironmentFile=`
//! directive) and `kastellan-core`'s installer uses the same parser to diff the
//! env file it is about to overwrite. A second parser for one file format is
//! the drift shape #479 and #520 each cost a review round; and shared code is
//! compiled and tested on **both** hosts, while per-backend code is invisible
//! to CI (there is no macOS job at all) — the same reasoning that folded the
//! two backends' staging helpers into one `atomic_write` in #511.
//!
//! **Why this module owns file I/O too.** [`fold_env_files`] resolves an
//! ordered [`EnvFileRef`] list into a flat env map. It used to live inside the
//! macOS-only launchd backend, which meant the *ordering* and *optionality*
//! semantics of #458 — half of a cross-platform guarantee — compiled and ran
//! only when somebody happened to run `cargo test` on a Mac. Same argument as
//! the parser above; the I/O is incidental to it.

use std::path::Path;

use crate::{EnvFileRef, SupervisorError};

/// systemd's `WHITESPACE` set, and deliberately **not** [`char::is_whitespace`].
///
/// Rust's trims are Unicode-aware; systemd's are ASCII. Measured 2026-08-14 on
/// the DGX user manager (systemd 255), which is the only reason this const
/// exists — three rows came back different:
///
/// | line | systemd | a Unicode trim would give |
/// | --- | --- | --- |
/// | `U=\u{a0}x` | `\u{a0}x` | `x` |
/// | `V=x\u{a0}` | `x\u{a0}` | `x` |
/// | `\u{a0}Y=y` | *no variable at all* | `Y=y` |
/// | `W=\tx\t` | `x` | `x` (agree) |
/// | `X=x␣␣␣` | `x` | `x` (agree) |
///
/// The third row is the worst of them: a Unicode trim **invents a variable
/// systemd never creates**, which is the same failure the `export FOO=bar`
/// guard below exists to prevent. The first two are #552's shape — a value that
/// differs from the live environment produces a false `NOT fully applied` on
/// Linux and a genuinely different plist value on macOS.
///
/// Not exotic: a non-breaking space is what a copy-paste out of rendered
/// documentation or a chat window leaves behind, and `docs/deploy/operator-env.md`
/// hands operators a block to copy.
const SYSTEMD_WHITESPACE: [char; 4] = [' ', '\t', '\n', '\r'];

/// Parse an `EnvironmentFile`-style buffer into ordered `(KEY, value)` pairs.
///
/// Pure (no I/O). Implements the subset of systemd's `EnvironmentFile=` grammar
/// that operators actually write, **measured against a live systemd user manager
/// on 2026-08-09** rather than recalled:
///
/// | line | systemd | here |
/// | --- | --- | --- |
/// | `A="a b"` | `a b` | `a b` |
/// | `A='a b'` | `a b` | `a b` |
/// | `A=  c  ` | `c` | `c` |
/// | `A='  x  '` | `  x  ` | `  x  ` |
/// | `A=f"g` | `f"g` | `f"g` |
/// | `A="a` | `a` | `a` |
/// | `A="a'` | `a'` | `a'` |
/// | `A=""` | *empty* | *empty* |
/// | `export A=h` | dropped | dropped |
/// | `;A=i` | dropped | dropped |
/// | `#A=i` | dropped | dropped |
///
/// Re-measured 2026-08-13/14 (systemd 255) for [#552], which added two families
/// of row. **A value continuing past a closing quote:** `A="a b" # note` →
/// `a b# note`, `A="a" "b" c` → `abc` — `#` does not introduce a comment
/// mid-value, and multiple quoted sections concatenate with no separator; see
/// [`unquote`] for that table. **And the whitespace class itself**, which the
/// same probe run showed is ASCII where Rust's trims are Unicode-aware:
/// `A=\u{a0}x` keeps its non-breaking space and `\u{a0}A=x` declares **nothing
/// at all**, where a `char::is_whitespace` trim would have silently produced
/// `x` and invented an `A`. See [`SYSTEMD_WHITESPACE`].
///
/// [#552]: https://github.com/hherb/kastellan/issues/552
///
/// Quote-stripping and value-trimming are **not** cosmetic. Before #528 this
/// took values verbatim, justified by "the installer writes plain values" — a
/// premise #458 retired, because `kastellan.env.local` is the first env file a
/// *human* writes by hand, and humans quote. On Linux systemd did the stripping
/// and here nothing did, so one overlay file produced two different runtime
/// environments on the two first-class platforms.
///
/// Not implemented, because the installer never emits them and systemd's own
/// handling is more elaborate than an operator overlay warrants: backslash line
/// continuations, and C-style escapes inside double quotes. A value needing
/// either is out of contract on both platforms.
pub fn parse_env_file(contents: &str) -> Vec<(String, String)> {
    parse_env_file_reporting(contents).pairs
}

/// What [`parse_env_file`] kept, **and what it threw away**.
///
/// The pairs alone are not enough to report on an overlay honestly. Every line
/// this grammar refuses silently shrinks the key count, and that count is the
/// only feedback an operator gets: a six-key overlay with one `export` line
/// reads as `5 keys, all present in this process` — a green line for a file
/// whose sixth key never existed. Carrying the refused line *numbers* alongside
/// the pairs lets the report say so without ever naming a line's contents,
/// which would defeat the values-never-logged rule.
pub struct ParsedEnv {
    pub pairs: Vec<(String, String)>,
    /// 1-based numbers of lines that were neither blank nor a comment and yet
    /// declared nothing.
    pub ignored_lines: Vec<usize>,
}

/// [`parse_env_file`], plus the line numbers it refused. Pure.
pub fn parse_env_file_reporting(contents: &str) -> ParsedEnv {
    let mut pairs = Vec::new();
    let mut ignored_lines = Vec::new();
    for (n, raw) in contents.lines().enumerate() {
        let lineno = n + 1;
        let line = raw.trim_matches(SYSTEMD_WHITESPACE);
        // `;` is a comment introducer to systemd just as `#` is; treating it as
        // a key named `;FOO` invented a variable the Linux side never sees.
        // A blank or commented line declares nothing *on purpose*, so it is not
        // "ignored" in the sense the operator needs to hear about.
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            ignored_lines.push(lineno);
            continue;
        };
        let k = k.trim_matches(SYSTEMD_WHITESPACE);
        // systemd ignores an assignment whose name is not a bare identifier —
        // notably `export FOO=bar`, which shell users write by reflex. Silently
        // accepting it here created an env var literally named "export FOO".
        //
        // The test stays Unicode-aware (`char::is_whitespace`) while the trim
        // above is ASCII, and the asymmetry is deliberate: a key that keeps a
        // U+00A0 after the ASCII trim is one systemd drops outright (measured),
        // so rejecting it here is what agrees — and it is reported by line
        // number rather than silently skipped.
        if k.is_empty() || k.contains(char::is_whitespace) {
            ignored_lines.push(lineno);
            continue;
        }
        pairs.push((k.to_string(), unquote(v.trim_matches(SYSTEMD_WHITESPACE))));
    }
    ParsedEnv { pairs, ignored_lines }
}

/// Strip systemd's quoting from a value.
///
/// A quote only matters where a *section* begins: systemd enters a quoted state
/// there and leaves it at the matching close, or at end-of-line if there is
/// none. After a close it **skips the whitespace run** and then either opens
/// another quoted section — concatenated with no separator — or resumes
/// unquoted accumulation, which is verbatim to end-of-line. A quote reached in
/// the unquoted state is literal.
///
/// Measured on a live user manager (systemd 255) for [#552], and the
/// measurement **overturned that issue's own predicted answer**: it expected
/// `A="a b" # note` to yield `a b # note` and proposed "append the remainder"
/// as the fix, but systemd yields `a b# note` — the space after the closing
/// quote is dropped while the one in `A="a b"x y` → `a bx y` is kept. Appending
/// the remainder verbatim would have produced a third wrong answer and left the
/// false `NOT fully applied` warning in place for exactly the trailing-comment
/// case operators are taught to write.
///
/// | value | systemd | here |
/// | --- | --- | --- |
/// | `"a b" # note` | `a b# note` | `a b# note` |
/// | `"x"y` | `xy` | `xy` |
/// | `"a b"   x` | `a bx` | `a bx` |
/// | `"a b"x y` | `a bx y` | `a bx y` |
/// | `"a b" "c d"` | `a bc d` | `a bc d` |
/// | `"a" "b" c` | `abc` | `abc` |
/// | `"a" 'b'` | `ab` | `ab` |
/// | `"a"#c` | `a#c` | `a#c` |
/// | `'a b' # note` | `a b# note` | `a b# note` |
/// | `a "b c"` | `a "b c"` | `a "b c"` |
///
/// The earlier "matched pair only" rule was the obvious reading and was wrong
/// for the operator-typo cases too: systemd turns `A="a` into `a` and `A="a'`
/// into `a'`, where a pair-only rule keeps both quotes.
///
/// The post-close skip uses [`SYSTEMD_WHITESPACE`], and that class is measured
/// too (2026-08-14, the follow-up probe this docstring previously flagged as
/// owed): `"a"\tx` → `ax` (tab **is** skipped), `"a"\u{a0}x` → `a\u{a0}x` and
/// `"a"\u{b}␣x` → `a\u{b}␣x` (a non-breaking space and a vertical tab are
/// **not** — they end the skip and begin the unquoted tail). So systemd's class
/// really is ASCII, and a Unicode-aware `trim_start` here would have been wrong
/// rather than merely unverified.
///
/// [#552]: https://github.com/hherb/kastellan/issues/552
fn unquote(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    loop {
        let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            // Unquoted state: verbatim to end-of-line, quotes included. This is
            // also the empty-`rest` exit, so the loop always terminates — every
            // other arm consumes at least the opening quote.
            out.push_str(rest);
            return out;
        };
        let after_open = &rest[q.len_utf8()..];
        let Some(close) = after_open.find(q) else {
            // Unterminated: systemd runs the section to end-of-line rather than
            // treating the quote as literal (measured, and the reason the
            // pair-only rule was wrong).
            out.push_str(after_open);
            return out;
        };
        out.push_str(&after_open[..close]);
        rest = after_open[close + q.len_utf8()..].trim_start_matches(SYSTEMD_WHITESPACE);
    }
}

/// Merge `from` into `into`, with `from` winning on key collision (matching
/// systemd's `EnvironmentFile=`-after-`Environment=` override order, and the
/// later-file-wins order between two `EnvironmentFile=` directives — both
/// measured on a live systemd user manager, not assumed). Existing keys keep
/// their position with the value replaced; new keys are appended.
///
/// A key repeated *within* one batch resolves to its **last** occurrence, for
/// the same reason: the first push creates the slot, each later one overwrites
/// it in place.
pub fn merge_env(into: &mut Vec<(String, String)>, from: Vec<(String, String)>) {
    for (k, v) in from {
        if let Some(slot) = into.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            into.push((k, v));
        }
    }
}

/// Resolve an ordered [`EnvFileRef`] list over `into`, later files winning.
///
/// The backend-neutral half of #458: read each file in declared order and merge
/// its pairs, so an operator's `kastellan.env.local` overrides the
/// `kastellan.env` the installer regenerates. An absent **optional** file is
/// skipped — that is the normal state of the overlay — while an absent
/// **required** one is an error, and a file that exists but cannot be read is
/// an error either way. `NotFound` is the only forgiven kind: silently treating
/// an unreadable overlay as an empty one would be #458 wearing a new hat, since
/// the operator wrote the file and believes it applies.
///
/// Used by the launchd backend, which has no `EnvironmentFile=` directive and
/// must bake the values into the plist at install time. systemd does this
/// resolution itself at service start, from the directives
/// `systemd_user::builder` renders — so this function is macOS's route to the
/// same guarantee, and lives here so its semantics are tested on both hosts.
pub fn fold_env_files(
    into: &mut Vec<(String, String)>,
    files: &[EnvFileRef],
) -> Result<(), SupervisorError> {
    for ef in files {
        let contents = match std::fs::read_to_string(&ef.path) {
            Ok(c) => c,
            Err(e) if ef.optional && e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(SupervisorError::Io(format!(
                    "read environment_file {}: {e}",
                    ef.path.display()
                )))
            }
        };
        merge_env(into, parse_env_file(&contents));
    }
    Ok(())
}

/// Reject an `EnvironmentFile=` path systemd would refuse or misread.
///
/// Two rules, both fail-open without the check:
///
///   - **Absolute.** systemd requires it and drops the directive (with a
///     journal warning) otherwise, leaving the service running with **no**
///     environment at all — invisible unless someone reads the journal.
///   - **No control characters.** `systemd_user::builder` emits this directive
///     bare, because a quoted path is one systemd *drops* (#530, measured), so
///     a newline in the path would terminate the directive and turn whatever
///     follows into another one: `/tmp/x\nExecStartPre=/evil` injects an
///     `ExecStartPre`. Quoting used to make that case fail safe as a side
///     effect of breaking the legitimate cases; with quoting gone this check
///     is what carries the guarantee, and it must run before any rendering.
///     `SystemdUser::install` screens its other path fields for control
///     characters — this is the same rule, owned here so the env-file list
///     carries it on **both** backends rather than only on Linux.
///   - **Valid UTF-8.** The renderer writes the path with `to_string_lossy`, so
///     a non-UTF-8 path is emitted with `U+FFFD` substituted — a *different*
///     path, which systemd cannot open. `U+FFFD` is not a control character, so
///     the rule above passes it. On the required `kastellan.env` that fails the
///     unit loudly; on the `-`-prefixed overlay systemd ignores the error and
///     the operator's tuning silently never applies, which is exactly #530's
///     shape. Checking `to_str()` rather than the lossy string is what makes
///     the guard see the bytes the renderer will mangle.
///
/// A space is deliberately *not* rejected: `$HOME` may contain one, systemd
/// accepts it in a bare path, and launchd handles it natively. Refusing it
/// would fail an install that works.
pub fn validate_env_file_path(path: &Path) -> Result<(), SupervisorError> {
    if !path.is_absolute() {
        return Err(SupervisorError::Io(format!(
            "environment_file must be absolute, got {}",
            path.display()
        )));
    }
    let Some(s) = path.to_str() else {
        return Err(SupervisorError::Io(format!(
            "environment_file must be valid UTF-8 (the unit renderer would substitute U+FFFD \
             and systemd would open a different path), got {path:?}"
        )));
    };
    if s.contains(|c: char| c.is_control()) {
        return Err(SupervisorError::Io(format!(
            "environment_file must not contain control characters, got {path:?}"
        )));
    }
    Ok(())
}

/// What an inspection found at the operator overlay's path.
///
/// `Absent` and `Unreadable` are deliberately separate variants
/// ([#531](https://github.com/hherb/kastellan/issues/531)). Absent is the
/// *normal* state — most hosts declare no overlay — while unreadable means the
/// operator wrote a file and believes it applies. Collapsing them is the same
/// mistake [`fold_env_files`] refuses to make.
#[derive(Debug, Eq, PartialEq)]
pub enum OverlayState {
    /// No file at the path. Normal, and not an error.
    Absent,
    /// Read and parsed. `keys` counts the keys the file **declares** — a
    /// repeated key counts once — and `ignored_lines` names the lines the
    /// grammar refused, which is what keeps `keys` from shrinking silently.
    Present { keys: usize, ignored_lines: Vec<usize> },
    /// Exists but could not be read or decoded. Carries the OS reason.
    Unreadable { reason: String },
}

/// Resolve parsed pairs, collapsing a repeated key onto its last value. Pure.
///
/// One definition, because every count and every check downstream has to agree
/// on what "5 keys" means. Counting raw pairs instead would let an overlay with
/// one appended correction report `1 of 6 keys did not reach this process`
/// while declaring five — the numerator deduped, the denominator not.
///
/// Deliberately **not** public and deliberately not called `declared_keys`: it
/// returns pairs, *values included*, and a name promising keys is how a caller
/// ends up logging an operator's settings while believing it logged names. The
/// only things that leave this module are counts, key names and rendered lines.
fn resolved_pairs(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut resolved: Vec<(String, String)> = Vec::new();
    merge_env(&mut resolved, pairs.to_vec());
    resolved
}

/// Read the operator overlay at `path` and classify it. Does I/O; the
/// rendering half is pure, so the wording is testable without a filesystem.
pub fn inspect_overlay(path: &Path) -> OverlayState {
    match std::fs::read_to_string(path) {
        Ok(c) => {
            let parsed = parse_env_file_reporting(&c);
            OverlayState::Present {
                keys: resolved_pairs(&parsed.pairs).len(),
                ignored_lines: parsed.ignored_lines,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OverlayState::Absent,
        Err(e) => OverlayState::Unreadable { reason: e.to_string() },
    }
}

/// `", 2 lines ignored (lines 3, 6)"`, or nothing when the file parsed cleanly.
///
/// Line **numbers**, never line contents: a refused line is often a mistyped
/// assignment, and its right-hand side is exactly what must not reach a
/// plaintext log.
fn ignored_suffix(ignored: &[usize]) -> String {
    match ignored.len() {
        0 => String::new(),
        1 => format!(", 1 line ignored (line {})", ignored[0]),
        n => format!(
            ", {n} lines ignored (lines {})",
            ignored.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// One install-transcript line saying what was found at the overlay path.
///
/// Pure. **Names keys and counts, never values** — the install transcript is
/// plaintext, and the overlay is where endpoints and token-file paths live.
///
/// Always names the path, including in the `Absent` case, because that is the
/// diagnostic: an operator whose heredoc landed in `~/.config/kastellan.env.local`
/// (missing directory component) can only see the mistake if the tool says
/// where it actually looked.
pub fn render_overlay_found(path: &Path, state: &OverlayState) -> String {
    let p = path.display();
    match state {
        // A file that parses to nothing gets its own wording rather than
        // "(0 keys) — applied": an operator who wrote `export KEY=value` out of
        // shell reflex has a file that cannot apply, and the generic line reads
        // as confirmation that it did. Naming the likeliest cause costs one
        // clause and is the whole diagnostic.
        OverlayState::Present { keys: 0, ignored_lines } => format!(
            "operator overlay: {p} declares NO keys{} — nothing in it can apply; note that \
             `export KEY=value` and lines without `=` are ignored",
            ignored_suffix(ignored_lines)
        ),
        OverlayState::Present { keys, ignored_lines } => format!(
            "operator overlay: {p} ({keys} keys{}) — applied after kastellan.env, so these values win",
            ignored_suffix(ignored_lines)
        ),
        OverlayState::Absent => {
            format!("operator overlay: none at {p} — tuned settings put there survive a reinstall; see docs/deploy/operator-env.md")
        }
        OverlayState::Unreadable { reason } => {
            format!("operator overlay: {p} UNREADABLE ({reason}) — kastellan cannot confirm any of its values applied")
        }
    }
}

/// Overlay keys whose declared value is **not** what `live` reports.
///
/// Pure: `live` is the environment lookup, so the whole check is testable
/// without touching the process environment. The daemon passes
/// `|k| std::env::var(k).ok()`.
///
/// This is the end-to-end confirmation #531 asks for, and it is a stronger
/// claim than "the file exists": it compares what the operator wrote against
/// what the process actually inherited, which is the only thing that matters.
/// It catches a dropped `EnvironmentFile=` directive (#530's fail-open), a
/// mis-pathed overlay, and — because it compares *values*, not just presence —
/// an overlay listed BEFORE the generated file, where `kastellan.env` wins and
/// the operator's tuning is silently overridden, which is #458 itself.
///
/// A key repeated within the file is judged on its **last** value and named at
/// most once, matching [`merge_env`] and systemd: appending a correction rather
/// than editing in place is how an overlay naturally accumulates, and judging
/// the superseded value would report a key as unapplied exactly when the
/// correction did apply.
fn unapplied_keys(
    keys: &[(String, String)],
    live: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut resolved: Vec<(String, String)> = Vec::new();
    merge_env(&mut resolved, keys.to_vec());
    resolved
        .into_iter()
        .filter(|(k, v)| live(k).as_ref() != Some(v))
        .map(|(k, _)| k)
        .collect()
}

/// One daemon-startup line saying whether the overlay actually took effect.
///
/// Pure, and **names keys, never values** — same rule as
/// [`render_overlay_found`], and it bites harder here because the daemon log is
/// a plaintext file with none of `audit_log`'s role gating.
///
/// The two outcomes are worded differently rather than parameterised so an
/// operator skimming the log can tell them apart without reading the numbers.
fn render_overlay_applied(
    path: &Path,
    total: usize,
    unapplied: &[String],
    ignored_lines: &[usize],
) -> String {
    let p = path.display();
    let ign = ignored_suffix(ignored_lines);
    if unapplied.is_empty() {
        format!("operator overlay applied: {p} ({total} keys, all present in this process{ign})")
    } else {
        format!(
            "operator overlay NOT fully applied: {p} — {} of {total} keys did not reach this process{ign}: {}",
            unapplied.len(),
            unapplied.join(", ")
        )
    }
}

/// The daemon-side verdict on the overlay: one read, one classification.
///
/// Separate from [`OverlayState`] because the two callers ask different
/// questions. The installer has no process environment to compare against and
/// genuinely wants "what is at this path"; the daemon wants "did it take
/// effect", which needs the values — and having *one* type serve both meant the
/// daemon re-read and re-parsed the file after `inspect_overlay` had already
/// done so, opening a window in which the two halves of one log line described
/// different file contents, plus a second error path with its own hand-rolled
/// wording that no test covered.
#[derive(Debug, Eq, PartialEq)]
pub enum OverlayCheck {
    /// No file at the path. Normal, and not an error.
    Absent,
    /// Exists but could not be read or decoded.
    Unreadable { reason: String },
    /// Read, parsed, and compared against the live environment.
    Checked { declared: usize, unapplied: Vec<String>, ignored_lines: Vec<usize> },
}

/// How loudly [`render_overlay_check`]'s line deserves to be logged.
///
/// Returned rather than logged here so the whole decision stays pure and
/// testable: which outcomes are `warn!`-worthy is a judgement about operator
/// attention, and it was previously spread across the daemon's match arms where
/// no test could observe it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OverlaySeverity {
    Info,
    Warn,
}

/// Read the overlay at `path` and compare every key it declares against `live`.
///
/// Pure but for the one read: `live` is the environment lookup, so the whole
/// verdict is testable without touching the process environment. The daemon
/// passes `|k| std::env::var(k).ok()`.
///
/// This is the end-to-end confirmation [#531] asks for, and it is a stronger
/// claim than "the file exists": it compares what the operator wrote against
/// what the process actually inherited, which is the only thing that matters.
/// It catches a dropped `EnvironmentFile=` directive (#530's fail-open), a
/// mis-pathed overlay, and — because it compares *values*, not just presence —
/// an overlay listed BEFORE the generated file, where `kastellan.env` wins and
/// the operator's tuning is silently overridden, which is #458 itself.
///
/// [#531]: https://github.com/hherb/kastellan/issues/531
pub fn check_overlay(path: &Path, live: impl Fn(&str) -> Option<String>) -> OverlayCheck {
    match std::fs::read_to_string(path) {
        Ok(c) => {
            let parsed = parse_env_file_reporting(&c);
            let declared = resolved_pairs(&parsed.pairs);
            OverlayCheck::Checked {
                declared: declared.len(),
                unapplied: unapplied_keys(&declared, live),
                ignored_lines: parsed.ignored_lines,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OverlayCheck::Absent,
        Err(e) => OverlayCheck::Unreadable { reason: e.to_string() },
    }
}

/// One daemon-startup line, plus how loudly to say it. Pure, and **names keys,
/// never values** — the daemon log is a plaintext file with none of
/// `audit_log`'s role gating.
///
/// Three things are `Warn`, and the first two are the ones a naive
/// implementation gets wrong:
///
///   - **Zero declared keys.** `unapplied.is_empty()` is vacuously true over an
///     empty set, so the obvious `if unapplied.is_empty() { info! }` reports an
///     overlay that cannot possibly apply as `all present in this process`.
///     That is #531's own defect one level down.
///   - **Any ignored line.** The key count is the operator's only feedback, and
///     it is computed from a parser that discards its own rejects; a six-key
///     file with one `export` line otherwise reads as a clean five.
///   - Keys that did not reach the process, the case this all exists for.
pub fn render_overlay_check(path: &Path, check: &OverlayCheck) -> (OverlaySeverity, String) {
    match check {
        OverlayCheck::Absent => {
            (OverlaySeverity::Info, render_overlay_found(path, &OverlayState::Absent))
        }
        OverlayCheck::Unreadable { reason } => (
            OverlaySeverity::Warn,
            render_overlay_found(path, &OverlayState::Unreadable { reason: reason.clone() }),
        ),
        OverlayCheck::Checked { declared: 0, ignored_lines, .. } => (
            OverlaySeverity::Warn,
            render_overlay_found(
                path,
                &OverlayState::Present { keys: 0, ignored_lines: ignored_lines.clone() },
            ),
        ),
        OverlayCheck::Checked { declared, unapplied, ignored_lines } => {
            let severity = if unapplied.is_empty() && ignored_lines.is_empty() {
                OverlaySeverity::Info
            } else {
                OverlaySeverity::Warn
            };
            (severity, render_overlay_applied(path, *declared, unapplied, ignored_lines))
        }
    }
}

#[cfg(test)]
mod tests;
