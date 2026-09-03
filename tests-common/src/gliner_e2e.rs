//! Preconditions for the gliner-relex end-to-end tiers, in one place.
//!
//! Three integration suites — `core/tests/gliner_relex_e2e.rs`,
//! `entity_extraction_e2e.rs` and `memory_entity_link_e2e.rs` — spawn the
//! **real** Python worker on the **real** 1.3 GB model. Each used to carry its
//! own copy of the precondition cascade and its own `GlinerRelexEnv` builder,
//! and the copies drifted: one kept `interpreter_root: None` after the other
//! two were fixed (#284), and all three then missed the symlink alias (#650).
//!
//! # The two problems this module exists to fix
//!
//! **1. A skip that cannot be turned into a failure is not a gate ([#653]).**
//! Every precondition below `[SKIP]`s by default, which is correct on an
//! unstaged host. But a *staged* host that trips one silently reports green,
//! and that is how #651's fixture bug survived for months: the DGX's `.venv`
//! was a copy of the Mac's, so `bin/python` named a path that cannot exist on
//! Linux, and four tests read as passing while containing nothing.
//!
//! Setting [`REQUIRE_ENV`] to a truthy value turns every one of those skips
//! into a **panic naming the unmet precondition**. That is the Rust half of
//! the `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` knob `workers/gliner-relex/`
//! already honours on the Python side (`tests/live_support.py`), and it is
//! what makes the `[SKIP]` counts in `HANDOVER.md` mean something.
//!
//! **2. The fixtures spoke a different flag dialect than production
//! ([#654]).** #459 unified every operator-facing kastellan flag on
//! `1|true|yes|on` (trimmed, case-insensitive), and production reads
//! [`ENABLE_ENV`] through `env_flag_enabled`. The three fixtures kept a strict
//! `!= Some("1")`, so an operator whose `kastellan.env` legitimately reads
//! `KASTELLAN_GLINER_RELEX_ENABLE=true` — which *does* enable the daemon
//! worker — got a silent skip. Both halves now go through the same reader.
//!
//! # Scope
//!
//! [`REQUIRE_ENV`] covers the five host-mode preconditions #653 names: the
//! opt-in flag, the sandbox probe, the supervisor probe, the venv shim and the
//! weights snapshot. The macOS **container** tier (`container` CLI present, image
//! built) keeps skip-only semantics and stays in `gliner_relex_e2e.rs`: it is a
//! separate and much heavier opt-in, so folding it in here would make the knob
//! unusable on a Mac that has the venv staged but no image.
//!
//! [#653]: https://github.com/hherb/kastellan/issues/653
//! [#654]: https://github.com/hherb/kastellan/issues/654

use std::path::{Path, PathBuf};

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::workers::gliner_relex::GlinerRelexEnv;

use crate::gliner_weights::weights_dir_or_reason;
use crate::sandbox::sandbox_unavailable_reason;
use crate::skip::supervisor_unavailable_reason;
use crate::venv_interpreter::venv_interpreter_binds;

/// The operator's opt-in for the real-model tiers. Production reads the same
/// variable through the same `env_flag_enabled` dialect
/// (`core/src/workers/gliner_relex/resolve.rs`).
pub const ENABLE_ENV: &str = "KASTELLAN_GLINER_RELEX_ENABLE";

/// The operator's demand that the tiers actually run. Same name and same
/// dialect as the Python suite's knob (`workers/gliner-relex/tests/live_support.py`).
pub const REQUIRE_ENV: &str = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E";

/// The uv-generated console-script shim, relative to the workspace root.
/// `scripts/workers/gliner-relex/install.sh` is what creates it (via `uv sync`
/// honouring `[project.scripts]`), so this literal and that script must agree.
pub const VENV_SHIM_SUBPATH: &str =
    "workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex";

/// The model these tiers pin. Every host-mode fixture used its own copy of this
/// string; one definition means a model bump cannot land in two suites and miss
/// the third.
pub const MODEL_ID: &str = "knowledgator/gliner-relex-multi-v1.0";

/// What an unmet precondition means for *this* run.
///
/// The default is [`UnmetAction::Skip`], which is what makes a plain
/// `cargo test` green on a host that has never staged the venv or the weights.
/// [`REQUIRE_ENV`] flips it, and that flip is the whole point of this module:
/// a skip nobody can turn into a failure cannot detect a dead fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmetAction {
    /// Print `[SKIP] <reason>` and let the calling test return green.
    Skip,
    /// Panic naming the unmet precondition — the operator asked for a real run.
    Fail,
}

/// Pure: does this [`REQUIRE_ENV`] value demand a real run?
///
/// Routed through the one project flag dialect (`1|true|yes|on`, trimmed,
/// case-insensitive) rather than a strict `Some("1")`, because the strict form
/// is exactly the skew [#654] was filed about.
///
/// [#654]: https://github.com/hherb/kastellan/issues/654
pub fn unmet_action(require_flag: Option<String>) -> UnmetAction {
    if env_flag_enabled(require_flag) {
        UnmetAction::Fail
    } else {
        UnmetAction::Skip
    }
}

/// Whether a tier's precondition set includes the [`ENABLE_ENV`] opt-in.
///
/// `gliner_relex_e2e.rs` runs whenever the venv and the weights are staged —
/// it has no separate opt-in — while the two tiers that also bring up Postgres
/// and load the model into the recall path wait for the operator's flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableFlag {
    /// The tier is gated on [`ENABLE_ENV`] being truthy.
    Checked,
    /// The tier ignores [`ENABLE_ENV`] entirely, in both directions.
    Ignored,
}

/// Pure: is the opt-in flag an unmet precondition for this tier? `Some(reason)`
/// when it is, `None` when the tier may proceed.
pub fn enable_gate_unmet(gate: EnableFlag, enable_flag: Option<String>) -> Option<String> {
    match gate {
        EnableFlag::Ignored => None,
        EnableFlag::Checked if env_flag_enabled(enable_flag) => None,
        EnableFlag::Checked => Some(format!(
            "{ENABLE_ENV} is not set to a truthy value (1|true|yes|on) — this tier \
             loads the real 1.3 GB model and is opt-in"
        )),
    }
}

/// Pure: where the venv shim lives under `workspace_root`.
pub fn venv_shim_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(VENV_SHIM_SUBPATH)
}

/// Pure: the venv root that owns `shim`, i.e. `<venv>/bin/<shim>`'s grandparent.
///
/// Derived from the shim rather than re-assembled from the workspace root, so
/// the path that is spawned and the path that is bound into the jail cannot
/// name two different venvs.
///
/// # Panics
///
/// If `shim` has fewer than two ancestors — only reachable by passing something
/// that is not a `<venv>/bin/<name>` path at all.
pub fn venv_dir_of(shim: &Path) -> PathBuf {
    shim.parent()
        .and_then(|bin| bin.parent())
        .expect("shim path is <venv>/bin/<name> — both parent levels must exist")
        .to_path_buf()
}

/// The workspace root, resolved at compile time from this crate's manifest dir.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent — broken workspace layout")
        .to_path_buf()
}

/// Resolve the in-tree venv shim, or say why it is not there.
///
/// Deliberately does **not** honour the daemon's
/// `KASTELLAN_GLINER_RELEX_VENV_DIR` override: tests always run against the
/// in-tree `workers/gliner-relex/.venv/`, so a stray operator export cannot
/// point a test run at a different worker than the one just built.
pub fn venv_shim_or_reason() -> Result<PathBuf, String> {
    let shim = venv_shim_path(&workspace_root());
    if shim.exists() {
        Ok(shim)
    } else {
        Err(format!(
            "gliner-relex venv shim not built at {} — run scripts/workers/gliner-relex/install.sh",
            shim.display()
        ))
    }
}

/// Read [`REQUIRE_ENV`] from the process environment.
///
/// The only impure step in the decision; kept separate from
/// [`unmet_action`] so the rule itself stays unit-testable, and separate from
/// [`report_unmet`] so a call site cannot silently re-read the environment
/// halfway through a cascade.
pub fn require_action() -> UnmetAction {
    unmet_action(std::env::var(REQUIRE_ENV).ok())
}

/// Act on an unmet precondition: skip cleanly, or fail loudly.
///
/// Returns `None` so a caller inside a `-> Option<_>` fixture can
/// `return report_unmet(action, &reason);` directly. Generic in the return type
/// for exactly that reason — the value is never constructed.
///
/// `pub` so a suite can route a precondition that is **not** part of
/// [`gliner_host_env`]'s cascade through the same knob. The Postgres bring-up
/// in all three gliner-relex suites is exactly that: it is shared with ~30
/// other suites, so it cannot become require-aware in
/// [`crate::skip::pg_bin_dir_or_skip`] itself, but for these three a missing
/// Postgres means the test body never runs — the same false green.
///
/// # Panics
///
/// Under [`UnmetAction::Fail`], naming both [`REQUIRE_ENV`] and `reason`. Both
/// halves matter: the knob so an operator reads it as their own demand rather
/// than as a regression, and the reason so they know what to stage next.
pub fn report_unmet<T>(action: UnmetAction, reason: &str) -> Option<T> {
    match action {
        UnmetAction::Fail => panic!(
            "{REQUIRE_ENV} demanded a real gliner-relex end-to-end run, but a \
             precondition is unmet: {reason}"
        ),
        UnmetAction::Skip => {
            eprint!("{}", crate::skip::skip_line(reason));
            None
        }
    }
}

/// Build the host-mode [`GlinerRelexEnv`] the three e2e tiers share, or report
/// the first unmet precondition.
///
/// The cascade, in order — each step is skipped-or-failed by
/// [`report_unmet`] according to [`REQUIRE_ENV`]:
///
/// 1. the [`ENABLE_ENV`] opt-in (only when `gate` is [`EnableFlag::Checked`]),
/// 2. the per-OS sandbox probe,
/// 3. the user-level supervisor probe,
/// 4. the venv shim,
/// 5. the weights snapshot.
///
/// Ordered cheapest-and-most-likely-first so an unstaged host reports the
/// precondition an operator would fix first, not the last one that happened to
/// fail. The probes are short-circuited rather than evaluated together because
/// the sandbox probe spawns a real `bwrap`.
///
/// The interpreter binds come from [`venv_interpreter_binds`], which calls the
/// **production** resolver — that is deliberate, and it is what #650 was about:
/// a hand-rolled copy in a fixture silently misses a fix to the real one.
pub fn gliner_host_env(gate: EnableFlag) -> Option<GlinerRelexEnv> {
    let action = require_action();

    if let Some(reason) = enable_gate_unmet(gate, std::env::var(ENABLE_ENV).ok()) {
        return report_unmet(action, &reason);
    }
    if let Some(reason) = sandbox_unavailable_reason() {
        return report_unmet(action, &reason);
    }
    if let Some(reason) = supervisor_unavailable_reason() {
        return report_unmet(action, &reason);
    }
    let script_path = match venv_shim_or_reason() {
        Ok(p) => p,
        Err(reason) => return report_unmet(action, &reason),
    };
    let weights_dir = match weights_dir_or_reason() {
        Ok(p) => p,
        Err(reason) => return report_unmet(action, &reason),
    };

    let venv_dir = venv_dir_of(&script_path);
    let (interpreter_root, interpreter_lib_dirs) = venv_interpreter_binds(&venv_dir);
    Some(GlinerRelexEnv {
        script_path,
        venv_dir,
        weights_dir,
        model_id: MODEL_ID.to_string(),
        device: "auto".to_string(),
        use_container_backend: false,
        container_image: None,
        interpreter_root,
        interpreter_lib_dirs,
    })
}

/// The binary the Linux tiers spawn the worker through.
///
/// Deliberately declared **outside** the `cfg` in
/// [`gliner_host_lockdown_shim`]. A name that lives only inside the Linux arm
/// is compiled away on macOS, so a Mac test run cannot tell it from any other
/// string — the same one-host blind spot that makes an unused `cfg(linux)`
/// helper visible only to the DGX's `-D dead-code` gate. Out here, both hosts
/// compile it and both hosts pin it.
pub const LOCKDOWN_SHIM_BIN: &str = "kastellan-worker-lockdown-exec";

/// The lockdown-exec shim the host-mode tiers spawn the worker through.
///
/// On Linux the worker must run under the `ml_client` seccomp filter (#281),
/// which the production manifest arranges via `discover_binary`; the e2e tiers
/// mirror it. On macOS Seatbelt is applied from the parent, so there is no shim
/// and this is `None`.
pub fn gliner_host_lockdown_shim() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(crate::binaries::workspace_target_binary(LOCKDOWN_SHIM_BIN))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The project flag dialect (#459), asserted at *this* call site rather
    /// than trusted from `env_flag_enabled`'s own tests — the regression this
    /// guards against is someone re-introducing a strict `Some("1")` here.
    #[test]
    fn the_require_knob_takes_the_whole_project_flag_dialect() {
        for truthy in ["1", "true", "TRUE", "Yes", " yes ", "on", " 1\n"] {
            assert_eq!(
                unmet_action(Some(truthy.to_string())),
                UnmetAction::Fail,
                "{truthy:?} should demand a real run"
            );
        }
    }

    /// Unset is the default and must stay the default: a plain `cargo test`
    /// on an unstaged host has to keep skipping cleanly.
    #[test]
    fn an_absent_or_falsey_require_knob_still_skips() {
        let falsey: [Option<&str>; 6] = [None, Some(""), Some("0"), Some("off"), Some("no"), Some("maybe")];
        for value in falsey {
            assert_eq!(
                unmet_action(value.map(str::to_string)),
                UnmetAction::Skip,
                "{value:?} must not demand a run"
            );
        }
    }

    /// #654: the operator-facing enable flag, at the fixture's own call site.
    /// `true`/`yes`/`on` enable the daemon worker, so they must enable the
    /// tests too — a fixture that only understands `"1"` turns a correct
    /// `kastellan.env` into a silent skip.
    #[test]
    fn the_enable_gate_accepts_the_dialect_not_just_the_literal_one() {
        for truthy in ["1", "true", " TRUE ", "yes", "on", " 1\n"] {
            assert!(
                enable_gate_unmet(EnableFlag::Checked, Some(truthy.to_string())).is_none(),
                "{truthy:?} should opt the real-model tier in"
            );
        }
        for falsey in [None, Some(""), Some("0"), Some("off")] {
            assert!(
                enable_gate_unmet(EnableFlag::Checked, falsey.map(str::to_string)).is_some(),
                "{falsey:?} should leave the tier opted out"
            );
        }
    }

    /// The tier that has no opt-in flag of its own (`gliner_relex_e2e.rs` runs
    /// whenever the venv + weights are staged) must ignore the flag in BOTH
    /// directions — including when it is set to something falsey.
    #[test]
    fn an_ignored_enable_gate_is_never_unmet() {
        for value in [None, Some(""), Some("0"), Some("1"), Some("true")] {
            assert!(
                enable_gate_unmet(EnableFlag::Ignored, value.map(str::to_string)).is_none(),
                "{value:?} must not gate a tier that ignores the flag"
            );
        }
    }

    /// The shim path is a cross-script constant:
    /// `scripts/workers/gliner-relex/install.sh` writes it and `uv sync`
    /// creates it. Asserting the literal is the only thing that catches a typo.
    #[test]
    fn the_venv_shim_path_is_the_one_install_sh_writes() {
        assert_eq!(
            VENV_SHIM_SUBPATH,
            "workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex"
        );
        assert_eq!(
            venv_shim_path(Path::new("/srv/kastellan")),
            PathBuf::from(
                "/srv/kastellan/workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex"
            )
        );
    }

    /// The venv dir is the shim's grandparent, and the fixture derives it
    /// rather than re-deriving the path from the workspace root — so the two
    /// cannot disagree about which venv is under test.
    #[test]
    fn the_venv_dir_is_the_shims_grandparent() {
        let shim = venv_shim_path(Path::new("/srv/kastellan"));
        assert_eq!(
            venv_dir_of(&shim),
            PathBuf::from("/srv/kastellan/workers/gliner-relex/.venv")
        );
    }

    /// The model id was a fourth string copied per fixture. Pinning the
    /// literal is what catches a bump that lands in two suites and misses the
    /// third — the exact way the interpreter binds drifted (#284).
    #[test]
    fn the_model_id_is_the_one_the_weights_snapshot_holds() {
        assert_eq!(MODEL_ID, "knowledgator/gliner-relex-multi-v1.0");
    }

    /// The shim binary's NAME, pinned on every host.
    ///
    /// [`LOCKDOWN_SHIM_BIN`] sits outside the `cfg` precisely so this assertion
    /// runs on the Mac too. Inside the Linux arm it would be compiled away
    /// here, and a typo in it would be provable only on the DGX.
    #[test]
    fn the_lockdown_shim_names_the_ml_client_filter_binary() {
        assert_eq!(LOCKDOWN_SHIM_BIN, "kastellan-worker-lockdown-exec");
    }

    /// The platform contract itself, asserted in BOTH directions.
    ///
    /// Written with `cfg!()` rather than `#[cfg]` on purpose: a `#[cfg]` ladder
    /// compiles only its own host's arm, so each host silently stops checking
    /// the other's. Here both arms compile everywhere and one runs — which is
    /// as far as a single host can go, since only Linux can observe the `Some`.
    #[test]
    fn the_lockdown_shim_is_linux_only() {
        let shim = gliner_host_lockdown_shim();
        if cfg!(target_os = "linux") {
            let path = shim.expect("Linux runs the worker under the ml_client filter (#281)");
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(LOCKDOWN_SHIM_BIN)
            );
        } else {
            assert!(
                shim.is_none(),
                "off Linux, Seatbelt is applied from the parent — there is no shim"
            );
        }
    }

    /// The default renders a `[SKIP]` line and nothing else.
    ///
    /// Asserted against [`crate::skip::skip_line`] rather than by calling
    /// [`report_unmet`], on purpose: `report_unmet`'s skip arm *prints* that
    /// line, and `grep -c '^\[SKIP\]'` over a `--nocapture` run is how this
    /// tree audits for tests that went green without executing anything. A unit
    /// test that emitted one would inflate the count it exists to protect — it
    /// did, and showed up as a fifth `[SKIP]` in a DGX gate whose other four
    /// were real.
    ///
    /// The `None` return itself needs no test: `report_unmet` is generic in `T`
    /// and never constructs one, so the skip arm cannot be mutated to return
    /// `Some(..)` and still compile.
    #[test]
    fn an_unmet_precondition_renders_one_skip_line_by_default() {
        assert_eq!(
            crate::skip::skip_line("weights dir missing at /nowhere"),
            "\n[SKIP] weights dir missing at /nowhere\n"
        );
    }

    /// The panic must name the knob, so an operator who set it knows which
    /// demand is being reported rather than reading it as a test regression.
    #[test]
    #[should_panic(expected = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E")]
    fn a_demanded_run_panics_naming_the_knob() {
        let _: Option<()> = report_unmet(UnmetAction::Fail, "weights dir missing at /nowhere");
    }

    /// ...and the reason, because "a precondition failed" without saying which
    /// one sends the operator back to `--nocapture` archaeology.
    #[test]
    #[should_panic(expected = "weights dir missing at /nowhere")]
    fn a_demanded_run_panics_naming_the_unmet_precondition() {
        let _: Option<()> = report_unmet(UnmetAction::Fail, "weights dir missing at /nowhere");
    }
}
