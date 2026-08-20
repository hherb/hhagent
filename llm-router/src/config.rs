//! [`RouterConfig`] — the static description of which backends the
//! router can reach and what to call by default.
//!
//! The config is populated from environment variables (test-friendly
//! seam, same shape as `KASTELLAN_DATA_DIR` / `KASTELLAN_STATE_DIR` in
//! `core`) with a default local-backend URL — per *host runtime* by
//! contract, one value on every OS today — so a fresh checkout works
//! without any setup on a machine that has the expected runtime
//! installed:
//!
//! * **Linux:** `http://127.0.0.1:8000/v1` — the default vLLM /
//!   SGLang OpenAI-compat port.
//! * **macOS:** `http://127.0.0.1:8000/v1` — the default oMLX port.
//!   oMLX serves MLX-quantised models through the same
//!   OpenAI-compatible surface and is materially faster than Ollama
//!   on Apple silicon, the gap widening with model size. Ollama
//!   (`:11434`) and llama.cpp's `--api` server remain supported —
//!   set `KASTELLAN_LLM_LOCAL_URL`.
//!
//! ## Environment variables
//!
//! | Var | Purpose | Default |
//! | --- | --- | --- |
//! | `KASTELLAN_LLM_LOCAL_URL` | Base URL of the local backend (no trailing `/`) | `http://127.0.0.1:8000/v1`, see above |
//! | `KASTELLAN_LLM_LOCAL_MODEL` | Default model name passed to the local backend | `local-default` |
//! | `KASTELLAN_LLM_EMBEDDING_URL` | Base URL of the embedding backend | falls back to local URL |
//! | `KASTELLAN_LLM_EMBEDDING_MODEL` | Default model name passed to the embedding backend | `embedding-default` |
//! | `KASTELLAN_LLM_FRONTIER_URL` | Base URL of the frontier backend | unset (frontier disabled) |
//! | `KASTELLAN_LLM_FRONTIER_MODEL` | Default model on the frontier backend | unset |
//! | `KASTELLAN_LLM_GUARD_URL` | Base URL of the model-based guard backend (Shieldstral) | unset (guard tier disabled) |
//! | `KASTELLAN_LLM_GUARD_MODEL` | Default model on the guard backend | unset |
//! | `KASTELLAN_LLM_TIMEOUT_MS` | Request timeout, milliseconds | 180_000 |
//! | `KASTELLAN_LLM_DISABLE_THINKING` | Suppress the local model's thinking block | `1` (on) |
//!
//! `KASTELLAN_LLM_DISABLE_THINKING` accepts `1`/`true`/`yes`/`on` and
//! `0`/`false`/`no`/`off` (trimmed, case-insensitive) and **rejects any
//! other non-empty value with a config error**. Empty is the one
//! exception, and it is not special-cased here: `read_env` treats an
//! empty value as *absent* for every var in this table, so a stray
//! `export KASTELLAN_LLM_DISABLE_THINKING=` falls through to the
//! default — which for this var is `on`, i.e. the safe direction.
//! Rejecting the rest is a deliberate
//! divergence from `kastellan-core`'s canonical `env_flag_enabled`
//! dialect, which reads an unrecognised value as *off*: those flags all
//! default off, so a typo there fails safe, whereas this one defaults
//! **on** — a typo read as "off" would silently restore the runaway
//! thinking this switch exists to prevent, and nothing downstream names
//! thinking when it does. (`llm-router` cannot call `env_flag_enabled`
//! regardless: `core` depends on this crate, not the reverse.)
//!
//! The frontier URL/model are deliberately *not* defaulted. Phase 0
//! refuses to dispatch to the frontier even when set; setting the
//! env vars is purely a forward-compatible seam so Phase 5 can wire
//! the policy gate without re-plumbing.
//!
//! Authentication keys for the frontier backend are *not* read from
//! env. They live in `db::secrets` (cf. the secrets-at-rest slice
//! shipped 2026-05-10) and will be fetched at dispatch time when
//! Phase 5's policy gate lands. Reading them from env at config-load
//! time would defeat the purpose of the keyring-wrapped at-rest
//! encryption.

use std::time::Duration;

use crate::error::RouterError;

pub const DEFAULT_LOCAL_MODEL: &str = "local-default";
pub const DEFAULT_EMBEDDING_MODEL: &str = "embedding-default";
/// Overall per-request timeout (the `reqwest` total `.timeout()`), in
/// milliseconds. This bounds **generation**, not just connect — a local
/// agentic plan over a 26B-class model with a multi-KB system prompt was
/// measured at ~86 s on the DGX, and the previous 30 s value cut those
/// calls off mid-generation, surfacing as a misleading
/// `RouterError::Transport("error sending request …")` (a `reqwest`
/// timeout displays exactly like a send failure). A dead backend still
/// fails fast via the separate 5 s `connect_timeout`, so this generous
/// value only ever bites a genuinely slow/hung generation. Operators
/// override with `KASTELLAN_LLM_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT_MS: u64 = 180_000;

/// Default base URL for the local backend — per host runtime by contract,
/// one value on every OS today.
///
/// Pure function (no env reads, no I/O). Returned as `&'static str`
/// so it composes into [`RouterConfig::default`] without an
/// allocation.
///
/// **Every OS currently resolves to the same value**, because the
/// default runtimes happen to agree on the port: vLLM/SGLang on Linux,
/// oMLX on macOS, and `:8000` as the least-bad guess elsewhere (better
/// to point at *something* than to require an env var — an unsupported
/// host then fails fast with connection-refused, which is the right
/// signal). It was per-OS while macOS defaulted to Ollama's `:11434`;
/// git history has the migration, this comment does not need to.
///
/// The function is kept — rather than inlined to a constant — because
/// the *contract* is "whatever this host's default local runtime
/// listens on", and that is what callers depend on. It is the seam
/// where a future divergence goes; a `cfg!` chain whose arms all agree
/// is not (clippy's `if_same_then_else` rejects it, correctly).
pub fn default_local_url_for_os() -> &'static str {
    "http://127.0.0.1:8000/v1"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterConfig {
    pub local_url: String,
    pub local_model: String,
    /// Base URL for the embedding backend. Defaults to `local_url`
    /// so a single OpenAI-compat server (oMLX, Ollama, or vLLM with
    /// both chat and embed loaded) works without setting two env vars.
    pub embedding_url: String,
    /// Default model name passed in the `model` field of
    /// `POST /embeddings`. Defaults to `"embedding-default"` — a
    /// placeholder that vLLM will reject with 4xx in production,
    /// forcing the operator to set `KASTELLAN_LLM_EMBEDDING_MODEL`
    /// explicitly (loud failure preferred to silent fallback).
    pub embedding_model: String,
    /// Set if and only if the operator has expressed intent to use a
    /// frontier backend. Phase 0 still refuses to dispatch even when
    /// set — the policy gate lands in Phase 5.
    pub frontier_url: Option<String>,
    pub frontier_model: Option<String>,
    /// Base URL for the model-based guard tier (Shieldstral on
    /// llama.cpp). `None` means the tier is unconfigured.
    ///
    /// **Never falls back to `local_url`.** That endpoint serves the
    /// planner model, which would answer the guard's
    /// `<Instruct>`/`<Query>` prompt with fluent prose rather than a
    /// calibrated yes/no logit pair — producing a number that looks
    /// exactly like a score and means nothing. Unconfigured yields an
    /// explicit "unmeasured", never a probability.
    pub guard_url: Option<String>,
    /// Model name sent to [`RouterConfig::guard_url`]. `None` means
    /// unconfigured; both must be set for the tier to be usable.
    pub guard_model: Option<String>,
    pub timeout: Duration,
    /// Ask the local backend's chat template to suppress the model's
    /// thinking block on every chat completion (see
    /// [`crate::messages::ChatRequest::without_thinking`]). Defaults to
    /// `true`: a reasoning model left to think freely on a large prompt
    /// overruns any sane request timeout, and the failure surfaces as a
    /// transport error or an empty completion rather than as anything
    /// that names thinking. Backends without the switch ignore the key,
    /// so the default is inert for them.
    ///
    /// Set `KASTELLAN_LLM_DISABLE_THINKING=0` to let the model think —
    /// appropriate when the local model is not a reasoning model, or
    /// when reasoning quality matters more than latency.
    pub disable_thinking: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        let default_url = default_local_url_for_os().to_string();
        Self {
            local_url: default_url.clone(),
            local_model: DEFAULT_LOCAL_MODEL.to_string(),
            embedding_url: default_url,
            embedding_model: DEFAULT_EMBEDDING_MODEL.to_string(),
            frontier_url: None,
            frontier_model: None,
            guard_url: None,
            guard_model: None,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            disable_thinking: true,
        }
    }
}

impl RouterConfig {
    /// Read the config from environment variables, falling back to
    /// [`RouterConfig::default`] on any unset key.
    ///
    /// Returns [`RouterError::Config`] only for **invalid** values
    /// (e.g. a non-numeric `KASTELLAN_LLM_TIMEOUT_MS`); an *unset* var
    /// is always fine and just means "use the default".
    pub fn from_env() -> Result<Self, RouterError> {
        let mut cfg = Self::default();

        if let Some(v) = read_env("KASTELLAN_LLM_LOCAL_URL")? {
            cfg.local_url = v.clone();
            // local_url change also drives the embedding fallback —
            // re-sync embedding_url unless the operator has already
            // overridden it explicitly below.
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_LOCAL_MODEL")? {
            cfg.local_model = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_EMBEDDING_URL")? {
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_EMBEDDING_MODEL")? {
            cfg.embedding_model = v;
        }
        cfg.frontier_url = read_env("KASTELLAN_LLM_FRONTIER_URL")?;
        cfg.frontier_model = read_env("KASTELLAN_LLM_FRONTIER_MODEL")?;
        cfg.guard_url = read_env("KASTELLAN_LLM_GUARD_URL")?;
        cfg.guard_model = read_env("KASTELLAN_LLM_GUARD_MODEL")?;
        if let Some(v) = read_env("KASTELLAN_LLM_TIMEOUT_MS")? {
            let ms: u64 = v.parse().map_err(|_| {
                RouterError::Config(format!(
                    "KASTELLAN_LLM_TIMEOUT_MS must be a non-negative integer, got {v:?}"
                ))
            })?;
            cfg.timeout = Duration::from_millis(ms);
        }
        if let Some(v) = read_env("KASTELLAN_LLM_DISABLE_THINKING")? {
            cfg.disable_thinking = match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => {
                    return Err(RouterError::Config(format!(
                        "KASTELLAN_LLM_DISABLE_THINKING must be one of \
                         1/0/true/false/yes/no/on/off, got {v:?}"
                    )))
                }
            };
        }
        Ok(cfg)
    }

    /// Derive a config that talks to the **guard** endpoint, or `None`
    /// when the tier is unconfigured.
    ///
    /// `Router::dispatch_local` reads `local_url`, so "reach the guard"
    /// is expressed as a config whose `local_url` *is* the guard's.
    /// That keeps the dispatch path and the backend enum untouched.
    ///
    /// Requires **both** `guard_url` and `guard_model`: a URL without a
    /// model would send `local_model` to a server that does not serve
    /// it, and the resulting 4xx would read as an outage rather than as
    /// the misconfiguration it is.
    ///
    /// Pure — no I/O, no env read.
    pub fn for_guard(&self) -> Option<RouterConfig> {
        let url = self.guard_url.as_ref()?;
        let model = self.guard_model.as_ref()?;
        Some(RouterConfig {
            local_url: url.clone(),
            local_model: model.clone(),
            ..self.clone()
        })
    }
}

/// Read an env var, treating *unset* and *empty* both as "absent".
///
/// Empty-string is treated as absent so a stray `export
/// KASTELLAN_LLM_FRONTIER_URL=` (common when an operator clears a value
/// without unsetting it) does not poison the config with an unusable
/// empty URL. The fail-loudly path is the typed parse in
/// [`RouterConfig::from_env`] for `KASTELLAN_LLM_TIMEOUT_MS`.
fn read_env(key: &str) -> Result<Option<String>, RouterError> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        Ok(_) => Ok(None),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(RouterError::Config(format!("env var {key} is not valid Unicode")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex serialising every env-touching test in this module.
    /// `cargo test` runs unit tests on multiple threads inside the
    /// same process, and `std::env::set_var` is process-global. The
    /// secret-rest tests in `db/src/secrets.rs` and the audit-tail
    /// tests in `core/src/audit_tail.rs` do not touch env so this is
    /// a llm-router-local concern; the secrets module solved the
    /// same problem the same way.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that sets a list of env vars on construction and
    /// restores their prior values on Drop, even if the test panics.
    /// Mutating process-global state mid-test is unavoidable here
    /// because [`RouterConfig::from_env`] reads `std::env`; using
    /// `temp-env` would add a dev-dep for a five-line helper.
    struct EnvScope {
        prior: Vec<(String, Option<String>)>,
    }

    impl EnvScope {
        fn new(pairs: &[(&str, Option<&str>)]) -> Self {
            let mut prior = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                prior.push((k.to_string(), std::env::var(k).ok()));
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            Self { prior }
        }
    }

    impl Drop for EnvScope {
        fn drop(&mut self) {
            for (k, v) in &self.prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn clear_all() -> EnvScope {
        EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", None),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_EMBEDDING_URL", None),
            ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
            ("KASTELLAN_LLM_FRONTIER_URL", None),
            ("KASTELLAN_LLM_FRONTIER_MODEL", None),
            ("KASTELLAN_LLM_GUARD_URL", None),
            ("KASTELLAN_LLM_GUARD_MODEL", None),
            ("KASTELLAN_LLM_TIMEOUT_MS", None),
            ("KASTELLAN_LLM_DISABLE_THINKING", None),
        ])
    }

    /// Pin the default URL so a port change is deliberate.
    ///
    /// Deliberately **not** a `cfg!` chain, and the reason is stronger
    /// than "the arms would be duplicates": `default_local_url_for_os`
    /// now contains no conditional compilation at all, so a run on any
    /// single host proves the value for *every* target. A `cfg!` chain
    /// here would assert less, not more — on a given host two of its
    /// three arms are dead code.
    ///
    /// Should a host's runtime diverge again, the `cfg!` split belongs
    /// here *and* in the function together. Note that nothing mechanical
    /// enforces that pairing: this crate's tests do not run in CI (see
    /// `.github/workflows/linux-check.yml`), so a macOS-only arm added to
    /// one side would pass the Linux gate untouched. Pin both platforms'
    /// values through `const`s if that day comes — the pattern is
    /// `install::plan::both_platform_default_sets_are_pinned_and_paired`.
    #[test]
    fn default_local_url_is_port_8000_on_every_os() {
        assert_eq!(default_local_url_for_os(), "http://127.0.0.1:8000/v1");
    }

    #[test]
    fn default_constants_are_pinned() {
        // Operators read these via the public re-exports; rotating
        // them silently would surprise a config audit.
        assert_eq!(DEFAULT_LOCAL_MODEL, "local-default");
        assert_eq!(DEFAULT_TIMEOUT_MS, 180_000);
    }

    #[test]
    fn default_config_uses_per_os_url_no_frontier_180s_timeout() {
        let cfg = RouterConfig::default();
        assert_eq!(cfg.local_url, default_local_url_for_os());
        assert_eq!(cfg.local_model, DEFAULT_LOCAL_MODEL);
        assert!(cfg.frontier_url.is_none());
        assert!(cfg.frontier_model.is_none());
        assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        // Default-ON: a reasoning model left to think freely overruns the
        // request timeout on a large prompt, and the resulting failure
        // names neither thinking nor the model.
        assert!(cfg.disable_thinking);
    }

    #[test]
    fn disable_thinking_accepts_both_boolean_spellings() {
        let _lock = ENV_LOCK.lock().unwrap();
        // `from_env` reads every router var, so an ambient
        // `KASTELLAN_LLM_TIMEOUT_MS=abc` would fail this test for an
        // unrelated reason. Clear the lot first.
        let _all = clear_all();
        for (raw, want) in [
            ("0", false),
            ("false", false),
            ("FALSE", false),
            ("no", false),
            ("off", false),
            ("1", true),
            ("true", true),
            ("  True  ", true),
            ("yes", true),
            ("on", true),
        ] {
            let _scope = EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some(raw))]);
            let cfg = RouterConfig::from_env().unwrap();
            assert_eq!(cfg.disable_thinking, want, "for input {raw:?}");
        }
    }

    #[test]
    fn from_env_rejects_non_boolean_disable_thinking() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Clear first, so the error we assert on can only have come from
        // this var (see `disable_thinking_accepts_both_boolean_spellings`).
        let _all = clear_all();
        let _scope =
            EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some("maybe"))]);
        let err = RouterConfig::from_env()
            .expect_err("a non-boolean must fail loudly, not silently default");
        assert!(
            err.to_string().contains("KASTELLAN_LLM_DISABLE_THINKING"),
            "error should name the offending var: {err}"
        );
    }

    /// The one value that is neither accepted nor rejected: empty.
    ///
    /// `read_env` treats an empty var as absent for every router var, so
    /// a stray `export KASTELLAN_LLM_DISABLE_THINKING=` does NOT error —
    /// it falls through to the default. Pinned because the module docs
    /// otherwise read as "rejects anything that is not one of the eight
    /// spellings", and because the fall-through direction is the
    /// load-bearing part: this var defaults **on**, so an emptied value
    /// keeps thinking suppressed rather than silently re-enabling it.
    #[test]
    fn empty_disable_thinking_falls_through_to_the_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _all = clear_all();
        let _scope = EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some(""))]);
        let cfg = RouterConfig::from_env()
            .expect("an empty value is absent, not invalid");
        assert!(
            cfg.disable_thinking,
            "empty must fall through to the default (on), not to off"
        );
    }

    #[test]
    fn from_env_with_no_vars_set_equals_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = clear_all();
        let cfg = RouterConfig::from_env().unwrap();
        assert_eq!(cfg, RouterConfig::default());
    }

    #[test]
    fn from_env_overrides_each_field() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", Some("http://10.0.0.1:9000/v1")),
            ("KASTELLAN_LLM_LOCAL_MODEL", Some("Qwen/Qwen2.5-7B-Instruct")),
            ("KASTELLAN_LLM_FRONTIER_URL", Some("https://api.anthropic.com/v1")),
            ("KASTELLAN_LLM_FRONTIER_MODEL", Some("claude-opus-4-7")),
            ("KASTELLAN_LLM_TIMEOUT_MS", Some("5000")),
        ]);
        let cfg = RouterConfig::from_env().unwrap();
        assert_eq!(cfg.local_url, "http://10.0.0.1:9000/v1");
        assert_eq!(cfg.local_model, "Qwen/Qwen2.5-7B-Instruct");
        assert_eq!(cfg.frontier_url.as_deref(), Some("https://api.anthropic.com/v1"));
        assert_eq!(cfg.frontier_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(cfg.timeout, Duration::from_millis(5_000));
    }

    #[test]
    fn from_env_treats_empty_string_as_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", Some("")),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_FRONTIER_URL", Some("")),
            ("KASTELLAN_LLM_FRONTIER_MODEL", None),
            ("KASTELLAN_LLM_TIMEOUT_MS", Some("")),
        ]);
        let cfg = RouterConfig::from_env().unwrap();
        // Empty fell back to the per-OS default rather than producing an
        // unusable empty URL.
        assert_eq!(cfg.local_url, default_local_url_for_os());
        assert!(cfg.frontier_url.is_none());
        assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    }

    #[test]
    fn from_env_rejects_non_numeric_timeout() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", None),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_FRONTIER_URL", None),
            ("KASTELLAN_LLM_FRONTIER_MODEL", None),
            ("KASTELLAN_LLM_TIMEOUT_MS", Some("not-a-number")),
        ]);
        let err = RouterConfig::from_env().unwrap_err();
        match err {
            RouterError::Config(msg) => {
                assert!(msg.contains("KASTELLAN_LLM_TIMEOUT_MS"), "msg={msg}");
                assert!(msg.contains("not-a-number"), "msg={msg}");
            }
            other => panic!("expected RouterError::Config, got {other:?}"),
        }
    }

    #[test]
    fn router_config_default_embedding_model_is_embedding_default() {
        let cfg = RouterConfig::default();
        assert_eq!(cfg.embedding_model, "embedding-default");
    }

    #[test]
    fn router_config_default_embedding_url_falls_back_to_local_url() {
        // No env vars touched here; the constructor default uses the
        // per-OS default for *both* local_url and embedding_url so an
        // oMLX-on-macOS deployment works with one URL set.
        let cfg = RouterConfig::default();
        assert_eq!(cfg.embedding_url, cfg.local_url);
    }

    #[test]
    fn router_config_from_env_reads_embedding_url_when_set() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", None),
            ("KASTELLAN_LLM_EMBEDDING_URL", Some("http://127.0.0.1:9999/v1")),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.embedding_url, "http://127.0.0.1:9999/v1");
    }

    #[test]
    fn router_config_from_env_reads_embedding_model_when_set() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", None),
            ("KASTELLAN_LLM_EMBEDDING_URL", None),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_EMBEDDING_MODEL", Some("BAAI/bge-m3")),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.embedding_model, "BAAI/bge-m3");
    }

    #[test]
    fn router_config_from_env_embedding_url_overrides_local_url() {
        // Pin the load-bearing override contract: when both env vars are
        // set, EMBEDDING_URL wins for embedding_url; local_url is
        // unaffected. A refactor that swaps the two from_env blocks
        // would break this contract silently otherwise.
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", Some("http://local:8080/v1")),
            ("KASTELLAN_LLM_EMBEDDING_URL", Some("http://embed:9999/v1")),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.local_url, "http://local:8080/v1");
        assert_eq!(cfg.embedding_url, "http://embed:9999/v1");
    }

    #[test]
    fn router_config_from_env_local_url_drives_embedding_url_when_embedding_unset() {
        // The fallback path: with only LOCAL_URL set, embedding_url
        // resolves to the same value (the load-bearing semantic that
        // makes oMLX-on-macOS work with one env var set).
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_LOCAL_URL", Some("http://local:8080/v1")),
            ("KASTELLAN_LLM_EMBEDDING_URL", None),
            ("KASTELLAN_LLM_LOCAL_MODEL", None),
            ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.local_url, "http://local:8080/v1");
        assert_eq!(cfg.embedding_url, "http://local:8080/v1");
    }

    #[test]
    fn for_guard_is_none_unless_both_url_and_model_are_set() {
        let mut cfg = RouterConfig::default();
        assert!(cfg.for_guard().is_none(), "unconfigured must yield None");

        cfg.guard_url = Some("http://127.0.0.1:8080/v1".to_string());
        assert!(cfg.for_guard().is_none(), "url alone is not enough");

        cfg.guard_url = None;
        cfg.guard_model = Some("shieldstral".to_string());
        assert!(cfg.for_guard().is_none(), "model alone is not enough");
    }

    /// The whole point of the seam: a configured guard must NOT inherit
    /// the planner's endpoint. That endpoint serves a different model,
    /// which would answer the guard prompt with prose and yield a number
    /// that looks exactly like a score and means nothing.
    #[test]
    fn for_guard_overrides_local_url_and_model_and_never_falls_back() {
        let mut cfg = RouterConfig::default();
        let planner_url = cfg.local_url.clone();
        cfg.guard_url = Some("http://127.0.0.1:8080/v1".to_string());
        cfg.guard_model = Some("shieldstral-1.0-3b-q8".to_string());

        let guard = cfg.for_guard().expect("configured");
        assert_eq!(guard.local_url, "http://127.0.0.1:8080/v1");
        assert_eq!(guard.local_model, "shieldstral-1.0-3b-q8");
        assert_ne!(guard.local_url, planner_url, "must not be the planner endpoint");
        assert_eq!(guard.timeout, cfg.timeout, "timeout is inherited");
        assert_eq!(
            guard.disable_thinking, cfg.disable_thinking,
            "thinking suppression is inherited: measured byte-identical on Shieldstral"
        );
    }

    #[test]
    fn from_env_reads_guard_url_and_model() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_GUARD_URL", Some("http://127.0.0.1:8080/v1")),
            ("KASTELLAN_LLM_GUARD_MODEL", Some("shieldstral-1.0-3b-q8")),
        ]);
        let cfg = RouterConfig::from_env().expect("valid");
        assert_eq!(cfg.guard_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert_eq!(cfg.guard_model.as_deref(), Some("shieldstral-1.0-3b-q8"));
    }

    #[test]
    fn from_env_leaves_guard_unset_when_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("KASTELLAN_LLM_GUARD_URL", None),
            ("KASTELLAN_LLM_GUARD_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("valid");
        assert!(cfg.guard_url.is_none());
        assert!(cfg.guard_model.is_none());
    }
}
