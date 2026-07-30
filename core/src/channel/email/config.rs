//! Operator/daemon configuration parsing for the email channel — the
//! env-gated [`EmailConfig`] builder.
//!
//! Config is deliberately **all-or-nothing**: [`EmailConfig::from_env`]
//! returns `Ok(None)` when `KASTELLAN_EMAIL_ENDPOINT` is unset (the channel
//! is simply absent — the daemon starts byte-identical to a build without
//! it), but once the endpoint IS set, every other field is mandatory and a
//! missing or blank one is `Err`, never a silent partial start. A
//! half-configured channel is worse than no channel: e.g. a missing
//! authserv-id would make `gate::trusted_dmarc_pass` reject every inbound
//! message (an empty configured id never matches), which looks exactly like
//! a delivery bug rather than the misconfiguration it actually is.
//!
//! **What that `Err` costs, and what it does not.** It refuses **the email
//! channel**, not the daemon: `main::email_boot::spawn_email_channel` turns
//! it into a loud `error!` and the daemon comes up with the email channel
//! absent (design spec §6, and the final whole-branch review's Important 5).
//! The fallback channel exists precisely because Matrix has no homeserver
//! failover, so letting a typo in the fallback's config take the primary
//! channel and the scheduler down with it inverts the whole point. The
//! error therefore names **every** missing variable at once, not just the
//! first — it is a message an operator reads and acts on, not an abort code.
//!
//! `EmailConfig::from_env` is a thin wrapper over the pure, injectable-getter
//! [`parse_email_config`] (mirrors `matrix/config.rs`'s
//! `parse_daemon_spawn_config` pattern) so the required/optional/blank
//! contract is unit-tested without mutating the process environment.

use std::path::{Path, PathBuf};

/// Exe-relative sibling default for the worker binary (cargo `target/debug` +
/// flat installs), matching `kastellan-worker-email-in`'s actual binary name
/// (`workers/email-in/Cargo.toml`).
const DEFAULT_WORKER_BIN_NAME: &str = "kastellan-worker-email-in";

/// Fully resolved config for the email channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailConfig {
    /// Base URL of the localmail `serve` instance (loopback or LAN/VPN).
    pub endpoint: String,
    /// Named localmail subscription this channel polls/acks (design D7 — the
    /// polling cursor lives server-side, keyed by this name).
    pub subscription: String,
    /// The agent's own email address on this channel.
    pub address: String,
    /// The authserv-id our own MX writes into `Authentication-Results` — the
    /// value `gate::trusted_dmarc_pass` requires an exact match against.
    pub authserv_id: String,
    /// Path to the `0600` file holding the localmail bearer token.
    pub token_file: PathBuf,
    /// Path to the `kastellan-worker-email-in` binary.
    pub worker_bin: PathBuf,
}

impl EmailConfig {
    /// Read config from the process environment. `Ok(None)` when
    /// `KASTELLAN_EMAIL_ENDPOINT` is unset or blank (channel absent). `Err`
    /// when it is set but any other required field is missing/blank, or the
    /// worker binary cannot be resolved (see [`parse_email_config`]). The
    /// `Err` disables the CHANNEL, never the daemon — see the module docs.
    ///
    /// Env contract:
    /// - `KASTELLAN_EMAIL_ENDPOINT` (gate) — e.g. `https://127.0.0.1:8443`.
    /// - `KASTELLAN_EMAIL_SUBSCRIPTION`, `KASTELLAN_EMAIL_ADDRESS`,
    ///   `KASTELLAN_EMAIL_AUTHSERV_ID`, `KASTELLAN_EMAIL_TOKEN_FILE` (required
    ///   once the gate is set).
    /// - `KASTELLAN_EMAIL_WORKER_BIN` (optional) — default: the running
    ///   daemon binary's own directory, joined with
    ///   `kastellan-worker-email-in`.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        parse_email_config(|k| std::env::var(k).ok(), exe_dir.as_deref())
    }
}

/// Pure builder behind [`EmailConfig::from_env`] over an injectable getter,
/// so the all-or-nothing contract is unit-tested without touching the
/// process environment. `exe_dir` sources the worker-binary fallback (`None`
/// when the running binary's own directory can't be determined — a
/// `KASTELLAN_EMAIL_WORKER_BIN` override still works in that case).
///
/// Both the gate (`KASTELLAN_EMAIL_ENDPOINT`) and every required field below
/// are trimmed before being judged blank — a value of `"   "` is exactly as
/// unusable as an empty string and must not silently pass through as a
/// literal " localhost" or similar.
pub(crate) fn parse_email_config(
    get: impl Fn(&str) -> Option<String>,
    exe_dir: Option<&Path>,
) -> anyhow::Result<Option<EmailConfig>> {
    // A blank (present-but-empty) endpoint is treated the same as unset —
    // `Ok(None)`, not `Err` — matching the codebase's existing convention for
    // an optional value gate (e.g. `MatrixSpawnConfig::password`'s
    // `.filter(|v| !v.is_empty())`): a stray `KASTELLAN_EMAIL_ENDPOINT=` in an
    // env file reads the same as the var never having been written at all.
    let Some(endpoint) = non_blank(get("KASTELLAN_EMAIL_ENDPOINT")) else {
        return Ok(None);
    };

    // Collect EVERY missing required var before failing, rather than `?`-ing
    // out at the first one. The error is now an operator-facing log line
    // rather than a startup abort (see `EmailConfig::from_env`'s docs), so
    // naming all of them means one restart to fix a three-typo env file
    // instead of three.
    let mut missing: Vec<&'static str> = Vec::new();
    let mut require = |key: &'static str| -> String {
        match non_blank(get(key)) {
            Some(v) => v,
            None => {
                missing.push(key);
                String::new()
            }
        }
    };
    let subscription = require("KASTELLAN_EMAIL_SUBSCRIPTION");
    let address = require("KASTELLAN_EMAIL_ADDRESS");
    let authserv_id = require("KASTELLAN_EMAIL_AUTHSERV_ID");
    let token_file = require("KASTELLAN_EMAIL_TOKEN_FILE");
    if !missing.is_empty() {
        anyhow::bail!(
            "missing or blank: {} ({} required once KASTELLAN_EMAIL_ENDPOINT is set)",
            missing.join(", "),
            if missing.len() == 1 { "is" } else { "are all" },
        );
    }

    let worker_bin = non_blank(get("KASTELLAN_EMAIL_WORKER_BIN"))
        .map(PathBuf::from)
        .or_else(|| exe_dir.map(|d| d.join(DEFAULT_WORKER_BIN_NAME)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "KASTELLAN_EMAIL_WORKER_BIN is not set and the daemon binary's own \
                 directory could not be determined; set it explicitly"
            )
        })?;

    Ok(Some(EmailConfig {
        endpoint,
        subscription,
        address,
        authserv_id,
        token_file: PathBuf::from(token_file),
        worker_bin,
    }))
}

/// Trim `v` and treat an empty result the same as `None`.
fn non_blank(v: Option<String>) -> Option<String> {
    v.and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    const ALL_REQUIRED: &[(&str, &str)] = &[
        ("KASTELLAN_EMAIL_ENDPOINT", "https://127.0.0.1:8443"),
        ("KASTELLAN_EMAIL_SUBSCRIPTION", "agent-inbox"),
        ("KASTELLAN_EMAIL_ADDRESS", "agent@example.org"),
        ("KASTELLAN_EMAIL_AUTHSERV_ID", "mx.example.net"),
        ("KASTELLAN_EMAIL_TOKEN_FILE", "/etc/kastellan/email.token"),
    ];

    #[test]
    fn none_when_endpoint_unset() {
        assert!(parse_email_config(env(&[]), None).unwrap().is_none());
    }

    #[test]
    fn none_when_endpoint_blank() {
        let g = env(&[("KASTELLAN_EMAIL_ENDPOINT", "   ")]);
        assert!(parse_email_config(g, None).unwrap().is_none());
    }

    #[test]
    fn some_with_every_field_when_fully_configured() {
        let g = env(ALL_REQUIRED);
        let cfg = parse_email_config(g, Some(Path::new("/exe"))).unwrap().expect("configured");
        assert_eq!(cfg.endpoint, "https://127.0.0.1:8443");
        assert_eq!(cfg.subscription, "agent-inbox");
        assert_eq!(cfg.address, "agent@example.org");
        assert_eq!(cfg.authserv_id, "mx.example.net");
        assert_eq!(cfg.token_file, PathBuf::from("/etc/kastellan/email.token"));
        assert_eq!(cfg.worker_bin, PathBuf::from("/exe/kastellan-worker-email-in"));
    }

    #[test]
    fn err_when_endpoint_set_but_subscription_missing() {
        let pairs: Vec<(&str, &str)> =
            ALL_REQUIRED.iter().filter(|(k, _)| *k != "KASTELLAN_EMAIL_SUBSCRIPTION").cloned().collect();
        let g = env(&pairs);
        let err = parse_email_config(g, Some(Path::new("/exe"))).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_EMAIL_SUBSCRIPTION"), "{err}");
    }

    #[test]
    fn err_when_endpoint_set_but_address_blank() {
        let pairs: Vec<(&str, &str)> = ALL_REQUIRED
            .iter()
            .map(|(k, v)| if *k == "KASTELLAN_EMAIL_ADDRESS" { (*k, "   ") } else { (*k, *v) })
            .collect();
        let g = env(&pairs);
        let err = parse_email_config(g, Some(Path::new("/exe"))).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_EMAIL_ADDRESS"), "{err}");
    }

    #[test]
    fn err_when_endpoint_set_but_authserv_id_missing() {
        let pairs: Vec<(&str, &str)> =
            ALL_REQUIRED.iter().filter(|(k, _)| *k != "KASTELLAN_EMAIL_AUTHSERV_ID").cloned().collect();
        let g = env(&pairs);
        let err = parse_email_config(g, Some(Path::new("/exe"))).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_EMAIL_AUTHSERV_ID"), "{err}");
    }

    #[test]
    fn err_when_endpoint_set_but_token_file_missing() {
        let pairs: Vec<(&str, &str)> =
            ALL_REQUIRED.iter().filter(|(k, _)| *k != "KASTELLAN_EMAIL_TOKEN_FILE").cloned().collect();
        let g = env(&pairs);
        let err = parse_email_config(g, Some(Path::new("/exe"))).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_EMAIL_TOKEN_FILE"), "{err}");
    }

    /// The error is an operator-facing log line (the daemon no longer aborts
    /// on it — see the module docs), so it must name EVERY missing variable,
    /// not just whichever one happened to be checked first.
    #[test]
    fn every_missing_required_var_is_named_in_one_error() {
        let g = env(&[("KASTELLAN_EMAIL_ENDPOINT", "https://127.0.0.1:8443")]);
        let err = parse_email_config(g, Some(Path::new("/exe"))).unwrap_err().to_string();
        for key in [
            "KASTELLAN_EMAIL_SUBSCRIPTION",
            "KASTELLAN_EMAIL_ADDRESS",
            "KASTELLAN_EMAIL_AUTHSERV_ID",
            "KASTELLAN_EMAIL_TOKEN_FILE",
        ] {
            assert!(err.contains(key), "{key} missing from the error: {err}");
        }
    }

    #[test]
    fn worker_bin_env_override_wins_over_exe_dir_default() {
        let mut pairs = ALL_REQUIRED.to_vec();
        pairs.push(("KASTELLAN_EMAIL_WORKER_BIN", "/opt/w/kastellan-worker-email-in"));
        let g = env(&pairs);
        let cfg = parse_email_config(g, Some(Path::new("/exe"))).unwrap().expect("configured");
        assert_eq!(cfg.worker_bin, PathBuf::from("/opt/w/kastellan-worker-email-in"));
    }

    #[test]
    fn worker_bin_unresolvable_is_an_error_not_a_panic_or_empty_path() {
        let g = env(ALL_REQUIRED);
        let err = parse_email_config(g, None).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_EMAIL_WORKER_BIN"), "{err}");
    }
}
