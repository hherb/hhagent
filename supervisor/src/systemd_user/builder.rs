//! Pure unit-file builders + name validator for the `systemd --user` backend.
//!
//! No I/O, no environment access, no `systemctl` invocation — every
//! function here is a deterministic `&input → String`/`Result`, so it is
//! unit-testable in isolation from the live user manager. Lifted out of
//! `systemd_user.rs` when that file outgrew the 500-LOC cap; the driver
//! ([`super::SystemdUser`], `probe`, the `systemctl` helpers) stays in the
//! parent and re-exports [`build_unit_file`], [`build_target_unit`], and
//! [`validate_service_name`] via `pub use builder::{…}` so their public
//! paths (`systemd_user::build_unit_file`, …) are unchanged.
//!
//! ### Unit-file shape
//!
//! ```ini
//! [Unit]
//! Description=kastellan service: <name>
//! After=<dep>.service                   # one line per spec.after entry (omitted when empty)
//! PartOf=<target>.target                # only when spec.part_of is set
//!
//! [Service]
//! Type=simple
//! ExecStart=/abs/program "arg one" arg2
//! Environment="KEY=value with spaces"
//! WorkingDirectory=/abs/dir
//! StandardOutput=append:/abs/log/out
//! StandardError=append:/abs/log/err
//! Restart=on-failure                    # only when keep_alive=true
//! RestartSec=5
//! RestartSteps=8                        # only when restart_backoff is set
//! RestartMaxDelaySec=300                # only when restart_backoff is set
//! TimeoutStopSec=10
//!
//! [Install]
//! WantedBy=<target>.target              # spec.part_of.target when set, else default.target
//! ```
//!
//! Each section's directives are emitted in a deterministic order so the
//! generated file is diffable and unit-testable.

use crate::{ServiceSpec, TargetSpec};

/// Default seconds before SIGKILL after SIGTERM on stop.
///
/// 10 s matches systemd's own default and is short enough that test
/// teardown doesn't hang if the inner process ignores SIGTERM.
const DEFAULT_TIMEOUT_STOP_SEC: u32 = 10;

/// Default seconds between restart attempts when `keep_alive=true`.
///
/// Resists tight crash loops without being so long that recovery from
/// transient errors is annoyingly slow.
const DEFAULT_RESTART_SEC: u32 = 5;

/// Build the textual contents of a `<name>.service` unit file.
///
/// Pure function: no I/O, no environment access, deterministic output.
/// Returns the full file as a `String` ready to be written to disk.
///
/// The caller is responsible for validating the spec's name with
/// [`validate_service_name`] before calling this — the builder assumes
/// its input is already well-formed.
///
/// # Quoting
///
/// `program` and each entry in `args` are emitted into `ExecStart=`,
/// space-separated. Tokens that contain whitespace, quotes, or
/// backslashes are wrapped in `"..."` with `"` and `\` escaped per
/// systemd's quoting rules. Same for environment values and for the path
/// fields `WorkingDirectory`, `StandardOutput` and `StandardError`:
/// routing them through [`quote_if_needed`] escapes a newline as `\n`, so
/// a control character in a path can never break the line and inject a
/// directive. A clean absolute path is emitted verbatim.
///
/// **`EnvironmentFile=` is the exception and is emitted bare** (#530):
/// systemd parses that directive's whole rvalue as a literal path, so a
/// quoted value is not a path and the directive is silently dropped. It
/// therefore carries no escaping of its own, and **callers must run
/// [`crate::env_file::validate_env_file_path`] on every entry first** —
/// both `SystemdUser::install` and `LaunchAgents::install` do. This
/// function is `pub`, so that is a call-site obligation, not an invariant
/// it can enforce.
pub fn build_unit_file(spec: &ServiceSpec) -> String {
    let mut out = String::with_capacity(512);

    // [Unit] section.
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=kastellan service: {}\n", spec.name));
    // Ordering: one After= per dependency. systemd only *orders* against
    // units present in the same start transaction — harmless if absent.
    for dep in &spec.after {
        out.push_str(&format!("After={dep}.service\n"));
    }
    // PartOf binds this unit's stop/restart to the target's: `systemctl
    // stop <target>.target` propagates to PartOf members.
    if let Some(target) = &spec.part_of {
        out.push_str(&format!("PartOf={target}.target\n"));
    }
    out.push('\n');

    // [Service] section.
    out.push_str("[Service]\n");
    out.push_str("Type=simple\n");

    // ExecStart: program then args, space-separated, each quoted only
    // when the token actually needs it.
    let mut exec_start = String::from("ExecStart=");
    exec_start.push_str(&quote_if_needed(&spec.program.to_string_lossy()));
    for a in &spec.args {
        exec_start.push(' ');
        exec_start.push_str(&quote_if_needed(a));
    }
    exec_start.push('\n');
    out.push_str(&exec_start);

    // Environment: one per line, deterministic order = the order the
    // caller provided. systemd accepts both `Environment=KEY=val` and
    // `Environment="KEY=val with spaces"`; we always use the second
    // form when the value contains anything fragile, the first when not.
    for (k, v) in &spec.env {
        let kv = format!("{k}={v}");
        out.push_str("Environment=");
        out.push_str(&quote_if_needed(&kv));
        out.push('\n');
    }
    // One directive per entry, in order. systemd applies them in file order with
    // a LATER file overriding an earlier one; the `-` prefix makes a missing file
    // non-fatal. Both behaviours were measured on a live user manager (#458).
    //
    // Emitted BARE — the one path field here that must not go through
    // `quote_if_needed` (#530). systemd parses this directive's whole rvalue as
    // a literal path and then demands `path_is_absolute`, so a quoted value is
    // not a path: the directive is dropped, `EnvironmentFile= path is not
    // absolute, ignoring: "…"` goes to the journal, and the unit **still loads
    // and starts** — fail-open, with the daemon getting no environment at all.
    // Measured on the DGX's live user manager 2026-08-13: a bare path
    // containing spaces is applied; double-quoted, single-quoted and
    // `-`-prefixed-quoted forms are all dropped while `systemctl start` returns
    // 0. Bare is therefore correct for every path systemd can express
    // VERBATIM. Not handled: a literal `%`, which systemd expands as a
    // specifier in this rvalue and which would need doubling — pre-existing and
    // equally true of `ExecStart=`/`Environment=`, tracked separately.
    //
    // Injection is not this line's job to prevent, because there is no escaping
    // systemd accepts here: `env_file::validate_env_file_path` refuses control
    // characters, non-UTF-8 and relative paths at both backends' `install`,
    // which is the only supported route to this renderer. `build_unit_file` is
    // `pub`, so that ordering is a call-site obligation rather than something
    // this loop can enforce — see the function doc.
    for ef in &spec.environment_files {
        let prefix = if ef.optional { "-" } else { "" };
        out.push_str(&format!("EnvironmentFile={prefix}{}\n", ef.path.to_string_lossy()));
    }

    // The remaining path fields DO go through `quote_if_needed`, at the emission
    // seam. A clean absolute path (the only legitimate input) is emitted
    // unchanged; a value containing a newline or other fragile char is quoted
    // with the newline escaped as `\n`, so it can never break the line and
    // inject a `[Service]` directive — regardless of whether the caller reached
    // the builder through `SystemdUser::install`'s control-char guard (audit
    // finding #10). Escaping here, not just at the driver, keeps the guarantee
    // at the point the directive is written (cf. launchd's `xml_escape` inside
    // `build_plist`). `EnvironmentFile=` above is the one field that cannot have
    // this, because systemd rejects every quoted form of it.
    if let Some(dir) = &spec.working_dir {
        out.push_str(&format!(
            "WorkingDirectory={}\n",
            quote_if_needed(&dir.to_string_lossy())
        ));
    }

    if let Some(log) = &spec.stdout_log {
        out.push_str(&format!(
            "StandardOutput=append:{}\n",
            quote_if_needed(&log.to_string_lossy())
        ));
    }
    if let Some(log) = &spec.stderr_log {
        out.push_str(&format!(
            "StandardError=append:{}\n",
            quote_if_needed(&log.to_string_lossy())
        ));
    }

    if spec.keep_alive {
        out.push_str("Restart=on-failure\n");
        out.push_str(&format!("RestartSec={}\n", DEFAULT_RESTART_SEC));
        // Optional exponential ramp. RestartSteps/RestartMaxDelaySec need
        // systemd 252+; older systemd logs an "unknown directive" warning at
        // load but still starts the unit, so emitting them is a safe degrade.
        if let Some(b) = &spec.restart_backoff {
            out.push_str(&format!("RestartSteps={}\n", b.steps));
            out.push_str(&format!("RestartMaxDelaySec={}\n", b.max_delay_sec));
        }
    }

    out.push_str(&format!("TimeoutStopSec={}\n", DEFAULT_TIMEOUT_STOP_SEC));
    out.push('\n');

    // [Install] section: this is what `systemctl --user enable` resolves to
    // decide where to drop the symlink, and since #508 the driver enables what
    // it installs. Which units that covers is worth being precise about:
    //
    //   - a standalone service (`install`) and the bundle's `.target`
    //     (`install_target`) are enabled, so for them this section is
    //     load-bearing for reboot survival, not an optional courtesy — the
    //     wrong `WantedBy=` links the unit under the wrong target;
    //   - a target *member* is never enabled (the target's own `Wants=` pulls
    //     it in), so its `WantedBy=<target>.target` is currently inert. It is
    //     emitted anyway so a member stays correct if someone enables it by
    //     hand, and so the two paths through this function agree.
    //
    // See `super::SystemdUser::install` and `install_target`.
    out.push_str("[Install]\n");
    // A target member is wanted by its target; a standalone service is
    // wanted by default.target so `enable` starts it at login.
    match &spec.part_of {
        Some(target) => out.push_str(&format!("WantedBy={target}.target\n")),
        None => out.push_str("WantedBy=default.target\n"),
    }

    out
}

/// Build the systemd `.target` unit body for a [`TargetSpec`].
///
/// The target `Wants=` all its members, so `systemctl --user start
/// <name>.target` pulls them in; per-member `After=` lines (emitted by
/// [`build_unit_file`] from each member's `ServiceSpec.after`) order the
/// start. We use `Wants=` (soft) rather than `Requires=` so a single
/// member failing does not tear the whole target down — the agent is
/// still useful if, say, an optional future member is absent.
///
/// Pure: no I/O. Same `TargetSpec` → same body.
pub fn build_target_unit(target: &TargetSpec) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=kastellan service bundle: {}\n", target.name));
    if !target.members.is_empty() {
        let wants: Vec<String> = target
            .members
            .iter()
            .map(|m| format!("{m}.service"))
            .collect();
        out.push_str(&format!("Wants={}\n", wants.join(" ")));
    }
    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=default.target\n");
    out
}

/// Quote a token for systemd unit-file syntax when it contains
/// whitespace, quotes, backslashes, or is empty.
///
/// Returns the original string when no quoting is needed (so the
/// emitted unit file stays human-readable in the common case).
fn quote_if_needed(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars()
            .any(|c| matches!(c, ' ' | '\t' | '"' | '\\' | '\n' | '\r'));
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

// The name gate lives at the crate root (#642), not here. It used to
// be a private `MAX_NAME_LEN` plus a character-identical copy of the
// predicate in each backend, which meant neither host ever ran the
// other's copy and a cross-platform caller could reach neither.
//
// Only the predicate is re-exported, so `systemd_user::validate_service_name`
// — the path this module's `install` already uses — still resolves. The
// cap itself is NOT: `builder` is a private module, so a `pub use` of a
// constant nothing here names is simply an unused import.
pub use crate::service_name::validate_service_name;

#[cfg(test)]
mod tests;
