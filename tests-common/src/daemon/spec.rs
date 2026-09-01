//! What a test daemon is configured with, as data (issue [#634]).
//!
//! [`bring_up_daemon`](super::bring_up_daemon) used to take six
//! positional arguments, three of them adjacent `&str`s. Four
//! integration tests wanted a seventh, eighth and ninth — a different
//! LLM model, a different LLM timeout, a longer readiness budget — and
//! rather than grow that signature they each hand-rolled the whole
//! bring-up. That is how the tree ended up with four copies of ~70
//! identical lines, and how the stderr-on-failure fix in [#635] landed
//! in one copy while the shared helper carrying three other e2es kept
//! the defect.
//!
//! So the parameters became a struct. The point is not brevity: it is
//! that a builder call names every value at the call site, which the
//! positional form did not. Transposing two adjacent `&str` arguments
//! compiles in silence — the same hazard
//! [#632](https://github.com/hherb/kastellan/issues/632) was filed
//! about one crate over.
//!
//! **Pure throughout.** Nothing here creates a directory, installs a
//! unit or opens a socket; [`DaemonSpec::service_spec`] turns a spec
//! plus three already-existing paths into a
//! [`ServiceSpec`](kastellan_supervisor::ServiceSpec) and nothing else.
//! That is what lets the assertions below run as `tests-common` unit
//! tests — the one thing `linux-check.yml` executes on **every PR**,
//! where the daemon e2es these values feed are DGX-gated and run on no
//! PR at all.
//!
//! [#634]: https://github.com/hherb/kastellan/issues/634
//! [#635]: https://github.com/hherb/kastellan/pull/635

use std::path::{Path, PathBuf};
use std::time::Duration;

use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::ServiceSpec;

/// The planner model every test daemon gets unless it asks for another.
///
/// Only the live-LLM callers override it; the inert-mock ones never
/// dial the router, so the value is arbitrary but must be *present* —
/// an unset model is a router config error and the daemon refuses to
/// boot.
pub const DEFAULT_LLM_MODEL: &str = "test-local-model";

/// The planner timeout every test daemon gets unless it asks for
/// another, in milliseconds as the env var spells it.
///
/// Loose enough for a slow CI runner; against the inert mock, which
/// answers synchronously on accept, a request is sub-millisecond.
pub const DEFAULT_LLM_TIMEOUT_MS: &str = "5000";

/// How long to wait for `"scheduler spawned"` unless the caller asks
/// for longer.
///
/// ⚠️ **10 s holds only because a test daemon does not configure the
/// guard tier.** `scheduler spawned` is logged *after*
/// `GuardTier::from_router_config`, which on a configured tier spends
/// up to `PROBE_BUDGET_MS` on the fatal `/props` call plus
/// `PROBE_TOTAL_BUDGET_MS + PROBE_BUDGET_MS` on the boot probe — ~80 s
/// since #626 doubled the total, where it was ~40 s before. A caller
/// that *does* configure a guard must either pin
/// `KASTELLAN_LLM_GUARD_TIMEOUT_MS` (which routes around the probe
/// entirely) or raise this with
/// [`DaemonSpec::ready_timeout`].
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The OpenAI-compat path segment appended to an [`LlmEndpoint::Base`].
const COMPAT_SEGMENT: &str = "/v1";

/// Where the daemon's planner router should dial.
///
/// Two variants rather than one string **because the tree genuinely
/// holds both shapes, and they are not interchangeable**. The mock-LLM
/// callers own a bare `http://127.0.0.1:<port>` and want the on-wire
/// OpenAI-compat shape; `observation_capture` reads
/// `KASTELLAN_LLM_LOCAL_URL` from the operator's environment, whose
/// documented value already ends in `/v1`
/// (`http://127.0.0.1:8000/v1`). Appending to the latter yields
/// `/v1/v1` and a router that dials nothing.
///
/// A single `&str` parameter cannot tell those apart, so the choice
/// would live in each caller's head. Here it is a type, and a call site
/// says which it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEndpoint {
    /// An OpenAI-compat **base**; [`COMPAT_SEGMENT`] is appended.
    Base(String),
    /// A **complete** URL that already carries its compat segment.
    /// Used verbatim.
    Verbatim(String),
}

impl LlmEndpoint {
    /// The value that reaches `KASTELLAN_LLM_LOCAL_URL`.
    fn url(&self) -> String {
        match self {
            Self::Base(base) => format!("{base}{COMPAT_SEGMENT}"),
            Self::Verbatim(url) => url.clone(),
        }
    }
}

/// Everything [`bring_up_daemon`](super::bring_up_daemon) needs.
///
/// Built with [`DaemonSpec::new`] plus the setters below. The four
/// values in `new` are the ones no caller can omit; everything else has
/// a default that matches what the shared helper did before this type
/// existed, so a migrated caller that sets nothing extra behaves
/// identically.
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    label: String,
    suffix: String,
    data_dir: PathBuf,
    user: String,
    llm: LlmEndpoint,
    llm_model: String,
    llm_timeout_ms: String,
    ready_timeout: Duration,
    extra_env: Vec<(String, String)>,
}

impl DaemonSpec {
    /// `label` distinguishes co-running tests' temp dirs and service
    /// names (`"l3run"` → `kastellan-supervisor-test-core-l3run-<suffix>`);
    /// `suffix` is the per-run uniquifier; `data_dir` is the per-test
    /// Postgres cluster's data directory; `user` is the `USER` the
    /// daemon runs as.
    pub fn new(
        label: impl Into<String>,
        suffix: impl Into<String>,
        data_dir: impl Into<PathBuf>,
        user: impl Into<String>,
        llm: LlmEndpoint,
    ) -> Self {
        Self {
            label: label.into(),
            suffix: suffix.into(),
            data_dir: data_dir.into(),
            user: user.into(),
            llm,
            llm_model: DEFAULT_LLM_MODEL.to_string(),
            llm_timeout_ms: DEFAULT_LLM_TIMEOUT_MS.to_string(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            extra_env: Vec::new(),
        }
    }

    /// Override [`DEFAULT_LLM_MODEL`] — for a caller driving a real LLM.
    pub fn llm_model(mut self, model: impl Into<String>) -> Self {
        self.llm_model = model.into();
        self
    }

    /// Override [`DEFAULT_LLM_TIMEOUT_MS`]. Takes the string the env
    /// var carries, not a `Duration`, because the callers that need it
    /// read it straight out of an operator-supplied env var and never
    /// parse it.
    pub fn llm_timeout_ms(mut self, ms: impl Into<String>) -> Self {
        self.llm_timeout_ms = ms.into();
        self
    }

    /// Override [`DEFAULT_READY_TIMEOUT`] — read its ⚠️ before raising
    /// this, because the reason a caller needs longer is usually a
    /// configured guard tier, and pinning the guard timeout is the
    /// better fix.
    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// Add one test-specific environment variable.
    ///
    /// Applied **after** the common keys, so an entry naming a key this
    /// type already sets wins. `mail_daemon_e2e` relies on exactly that
    /// to point a live-LLM run at a real model, and until now the
    /// guarantee was a comment at that call site rather than anything
    /// tested — `extra_env_wins_over_a_default_it_names` below is what
    /// makes it a property.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Add several test-specific environment variables, in order.
    pub fn envs(mut self, vars: impl IntoIterator<Item = (String, String)>) -> Self {
        self.extra_env.extend(vars);
        self
    }

    /// The supervisor unit name this spec installs under.
    pub fn service_name(&self) -> String {
        format!(
            "kastellan-supervisor-test-core-{}-{}",
            self.label, self.suffix
        )
    }

    /// The temp-root infixes for this daemon's log and state dirs.
    pub(crate) fn log_dir_infix(&self) -> String {
        format!("cli-{}-clog", self.label)
    }

    pub(crate) fn state_dir_infix(&self) -> String {
        format!("cli-{}-state", self.label)
    }

    pub(crate) fn ready_timeout_value(&self) -> Duration {
        self.ready_timeout
    }

    /// Turn this spec plus three already-created paths into the
    /// supervisor unit to install.
    ///
    /// Pure: no directory is created, no unit is written, no socket is
    /// opened. The caller owns the three paths' lifetimes (they are
    /// `PathGuard`-backed temp roots) precisely so this half can be
    /// asserted without a supervisor or a filesystem.
    ///
    /// The `stdout_log`/`stderr_log` fields `core_service_spec` derives
    /// from `CORE_SERVICE_NAME` are **replaced**, because this spec
    /// renames the unit and the log file names follow the unit name.
    pub(crate) fn service_spec(
        &self,
        binary: &Path,
        core_log_dir: &Path,
        state_dir: &Path,
    ) -> ServiceSpec {
        let mut spec = core_service_spec(binary, core_log_dir);
        spec.name = self.service_name();
        assert!(
            spec.name.len() <= 200,
            "service name must stay under the supervisor's 200-char cap, got {}: {}",
            spec.name.len(),
            spec.name
        );
        spec.stdout_log = Some(core_log_dir.join(format!("{}.out", spec.name)));
        spec.stderr_log = Some(core_log_dir.join(format!("{}.err", spec.name)));

        spec.env.push((
            "KASTELLAN_DATA_DIR".into(),
            self.data_dir.to_string_lossy().into_owned(),
        ));
        spec.env.push(("USER".into(), self.user.clone()));
        spec.env.push((
            "KASTELLAN_STATE_DIR".into(),
            state_dir.to_string_lossy().into_owned(),
        ));

        // Prompts: the daemon's prompt loader fails closed if the dir
        // is missing. `CARGO_MANIFEST_DIR` is `tests-common/` here,
        // whose parent is the workspace root — the same `<root>/prompts`
        // a test crate would resolve.
        let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("prompts");
        spec.env.push((
            "KASTELLAN_PROMPTS_DIR".into(),
            workspace_prompts.to_string_lossy().into_owned(),
        ));

        spec.env
            .push(("KASTELLAN_LLM_LOCAL_URL".into(), self.llm.url()));
        spec.env
            .push(("KASTELLAN_LLM_LOCAL_MODEL".into(), self.llm_model.clone()));
        spec.env.push((
            "KASTELLAN_LLM_TIMEOUT_MS".into(),
            self.llm_timeout_ms.clone(),
        ));

        // LAST, so a caller naming a key set above overrides it. See
        // `env`'s doc.
        spec.env.extend(self.extra_env.iter().cloned());

        spec
    }
}

#[cfg(test)]
mod tests;
