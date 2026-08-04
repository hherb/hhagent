//! Installer-coverage guard: every binary the workspace builds is either
//! **installed** by `kastellan-cli install` or **explicitly opted out**,
//! with a reason recorded in code (issue #504).
//!
//! # Why this module exists
//!
//! `core::install::plan::{required_binaries, optional_binaries}` are two
//! hand-maintained lists of binary names. `run_install` copies exactly those
//! names out of the build dir into `~/.local/lib/kastellan/`, and the daemon
//! discovers workers as `current_exe()`-relative siblings — so a worker
//! missing from both lists is **undeployable**, no matter how it is
//! configured. The daemon says so only at `ERROR` level, once, at startup:
//!
//! ```text
//! "worker misconfigured; skipping","tool":"web-research",
//! "detail":"could not resolve worker binary: … no sibling
//!           kastellan-worker-web-research found"
//! ```
//!
//! Nothing else fails. The install prints a cheerful `installed N binaries`,
//! every unit is `active`, and the tool is simply absent from the registry.
//!
//! That is exactly what happened: the list was written when there were six
//! workers and then silently lagged the workspace through **five** additions
//! — `mail` (#483), `email-in` (#496), `web-research`, and both brokers. The
//! mail worker's absence was worked around on the live host with a hand
//! `cp`, which is worse than the outage it papered over: the deployed binary
//! then stopped tracking `main` and no install refreshed it.
//!
//! # What this guard asserts
//!
//! One structural property, in the same spirit as [`crate::provisioning`]:
//! **every `[[bin]]` any workspace member declares is accounted for** —
//! present in `required_binaries()`, in `optional_binaries()`, or in
//! [`tests::NOT_INSTALLED`] with a written reason.
//!
//! Deliberately the whole workspace, not just `workers/*`. The invariant
//! "a binary operators need must actually be installed" is not
//! worker-specific: a new `core`-level binary would go missing in exactly
//! the way `kastellan-worker-mail` did, and scoping the guard to the
//! directory where the bug happened to be found is how you get to watch it
//! happen again somewhere else.
//!
//! It deliberately does *not* assert which of the three a given binary lands
//! in. Deciding that a new worker ships to operators is a judgement call; the
//! guard only insists the call is made **consciously**, in code, rather than
//! by forgetting a list exists. Adding a worker crate now fails this test
//! until its author writes down one of three answers.
//!
//! # Why here, and not in `core`'s own unit tests
//!
//! `linux-check.yml` is compile-only apart from `cargo test -p
//! kastellan-tests-common`, so a guard placed in `core` would run only when
//! an operator drives the DGX suite by hand — and this defect's whole
//! character is that it survives every check that is not specifically
//! looking for it. Here it runs on **every PR**, which is the only place
//! that catches a worker crate on the day it is added.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use kastellan_core::install::plan::{optional_binaries, required_binaries};

    /// Binaries the installer deliberately does **not** copy, each with the
    /// reason it is exempt. A name here is a decision, not an oversight —
    /// which is the entire point of the list.
    ///
    /// Keep this in sync with reality in both directions: an entry naming a
    /// binary the workspace no longer builds is stale, and
    /// [`opt_out_entries_all_exist`] fails on it.
    const NOT_INSTALLED: &[(&str, &str)] = &[
        (
            "kastellan-worker-kv-demo",
            "test fixture for the micro-VM persistent-store arc (slice 5b); never a production tool",
        ),
        (
            "kastellan-worker-net-demo",
            "test fixture for the net-worker-in-a-VM arc (slice 5c); never a production tool",
        ),
        (
            "kastellan-lockdown-probe",
            "test fixture: integration tests spawn it to check that Landlock + seccomp really deny",
        ),
        (
            "kastellan-microvm-init",
            "guest PID1 — baked INTO the rootfs image by build-*-rootfs.sh, never run on the host",
        ),
        (
            "kastellan-microvm-run",
            "resolved from $PATH (sandbox::linux_firecracker::MICROVM_RUN_BIN), not as an \
             exe-relative sibling, so copying it into bin_dir would not make it findable; \
             deploying the micro-VM launcher needs its own mechanism — issue #519",
        ),
        // The `sandbox` crate's five probes. All declare `path =
        // "tests/fixtures/…"`: they exist to be spawned BY the sandbox
        // integration tests, to check from inside a jail that the containment
        // really denies what it claims. Never part of a deployment.
        ("net_probe", "sandbox integration-test fixture: probes network reachability from inside a jail"),
        ("mem_burner", "sandbox integration-test fixture: allocates until the memory cap OOM-kills it"),
        ("sid_probe", "sandbox integration-test fixture: reports its session id to check --new-session"),
        ("mach_probe", "sandbox integration-test fixture: probes macOS mach-lookup denial"),
        ("uds_probe", "sandbox integration-test fixture: probes AF_UNIX reachability from inside a jail"),
    ];

    /// The repository root, derived from this crate's manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests-common has a workspace parent")
            .to_path_buf()
    }

    /// Workspace member paths, in declaration order.
    ///
    /// A deliberately small hand-rolled scan rather than a `toml` dependency:
    /// it reads one array of string literals out of the root manifest, and
    /// pulling a parser into the dev-dep graph to do that would cost more
    /// than it explains. Comments and trailing commas are tolerated; anything
    /// else about the manifest is ignored.
    fn workspace_members(root_manifest: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in root_manifest.lines() {
            let t = line.trim();
            if !inside {
                // `members = [` — the array may open on the same line.
                if t.starts_with("members") && t.contains('[') {
                    inside = true;
                }
                continue;
            }
            if t.starts_with(']') {
                break;
            }
            if let Some(name) = quoted_value(t) {
                out.push(name);
            }
        }
        out
    }

    /// The first double-quoted run in `line`, if any. Used for both member
    /// paths and `name = "…"` values.
    fn quoted_value(line: &str) -> Option<String> {
        let rest = line.split_once('"')?.1;
        let (val, _) = rest.split_once('"')?;
        Some(val.to_string())
    }

    /// Binary names a crate produces, from its manifest text plus whether it
    /// has a `src/main.rs`.
    ///
    /// Two shapes, because both occur in this workspace and a guard that
    /// understood only one would go quietly blind on the other:
    ///
    ///   * explicit `[[bin]]` sections — one binary per section, named by its
    ///     `name = "…"` key (a crate may declare several, as `prelude` does);
    ///   * no `[[bin]]` at all but a `src/main.rs` — Cargo's default binary,
    ///     named after the package.
    ///
    /// Pure: the caller does the filesystem work and passes the answers in,
    /// so the parsing is unit-testable against literals.
    fn declared_bin_names(manifest: &str, package_name: &str, has_src_main: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut in_bin_section = false;
        for line in manifest.lines() {
            let t = line.trim();
            if t.starts_with("[[bin]]") {
                in_bin_section = true;
                continue;
            }
            // Any other section header closes the one we were in. `[[bin]]`
            // itself is caught above, so this cannot swallow a sibling.
            if t.starts_with('[') {
                in_bin_section = false;
                continue;
            }
            if in_bin_section && t.starts_with("name") {
                if let Some(n) = quoted_value(t) {
                    out.push(n);
                    // Only the first `name` in a section names the binary;
                    // stay in the section so a stray key cannot re-trigger.
                    in_bin_section = false;
                }
            }
        }
        if out.is_empty() && has_src_main {
            out.push(package_name.to_string());
        }
        out
    }

    /// The `package.name` of a manifest.
    ///
    /// Scoped to the `[package]` section rather than "the first `name` key",
    /// because a dependency named `name-something` sitting above `[[bin]]`
    /// would otherwise be returned as the package name — a guard that reads
    /// the wrong name reports a binary that does not exist.
    fn package_name(manifest: &str) -> Option<String> {
        let mut in_package = false;
        for line in manifest.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                in_package = t.starts_with("[package]");
                continue;
            }
            if in_package && t.starts_with("name") {
                return quoted_value(t);
            }
        }
        None
    }

    /// Every binary every workspace member builds.
    fn workspace_binaries() -> BTreeSet<String> {
        let root = repo_root();
        let root_manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("workspace root Cargo.toml is readable");
        let members = workspace_members(&root_manifest);
        assert!(
            members.iter().any(|m| m.starts_with("workers/")),
            "parsed no workers/* members out of the root manifest — the scan has gone blind, \
             which would make every assertion below vacuously true. members: {members:?}"
        );

        let mut bins = BTreeSet::new();
        for member in &members {
            let dir = root.join(member);
            let manifest_path = dir.join("Cargo.toml");
            let manifest = std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
            let pkg = package_name(&manifest)
                .unwrap_or_else(|| panic!("no package name in {}", manifest_path.display()));
            let has_main = Path::new(&dir.join("src").join("main.rs")).exists();
            bins.extend(declared_bin_names(&manifest, &pkg, has_main));
        }
        bins
    }

    /// True when the installer copies `name` (either list).
    fn is_installed(name: &str) -> bool {
        required_binaries().contains(&name) || optional_binaries().contains(&name)
    }

    /// **The guard.** Every worker binary is installed or explicitly exempt.
    #[test]
    fn every_binary_is_installed_or_explicitly_opted_out() {
        let opted_out: BTreeSet<&str> = NOT_INSTALLED.iter().map(|(n, _)| *n).collect();
        let unaccounted: Vec<String> = workspace_binaries()
            .into_iter()
            .filter(|b| !is_installed(b) && !opted_out.contains(b.as_str()))
            .collect();

        assert!(
            unaccounted.is_empty(),
            "these worker binaries are built but never installed, so the daemon cannot \
             discover them as exe-relative siblings and the tools are silently absent \
             (issue #504): {unaccounted:?}\n\
             Fix by either adding each to `core::install::plan::optional_binaries()` or \
             recording why it is exempt in `NOT_INSTALLED` here."
        );
    }

    /// The opt-out list must not outlive what it exempts: a name here that
    /// the workspace no longer builds is stale, and a stale exemption is how
    /// a *re*-introduced binary would slip past the guard above.
    #[test]
    fn opt_out_entries_all_exist() {
        let built = workspace_binaries();
        let stale: Vec<&str> = NOT_INSTALLED
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !built.contains(*n))
            .collect();
        assert!(
            stale.is_empty(),
            "NOT_INSTALLED names binaries the workspace no longer builds: {stale:?}"
        );
    }

    /// Every exemption carries a reason. An empty string would technically
    /// satisfy the tuple while defeating the point of it.
    #[test]
    fn opt_out_entries_all_carry_a_reason() {
        for (name, reason) in NOT_INSTALLED {
            assert!(
                reason.len() > 20,
                "{name} is exempt without a usable reason: {reason:?}"
            );
        }
    }

    /// The five workers whose absence issue #504 was filed about. Named
    /// explicitly, so a future edit that drops one fails with *that* name
    /// rather than an anonymous set difference.
    #[test]
    fn the_workers_504_found_missing_are_installed() {
        for name in [
            "kastellan-worker-mail",
            "kastellan-worker-email-in",
            "kastellan-worker-web-research",
            "kastellan-worker-embed-broker",
            "kastellan-worker-search-broker",
        ] {
            assert!(is_installed(name), "{name} is not installed by the installer");
        }
    }

    /// The scan reaches beyond `workers/`. Without this, narrowing it back to
    /// worker crates — the exact scoping mistake this guard exists to
    /// outlast — would still leave every assertion above passing, because
    /// what it stopped looking at would simply cease to be a finding.
    #[test]
    fn the_scan_covers_non_worker_members_too() {
        let built = workspace_binaries();
        // `core` (the daemon + operator CLI) and `db` (the schema initialiser).
        for name in ["kastellan", "kastellan-cli", "kastellan-db-init"] {
            assert!(built.contains(name), "{name} is built but the scan did not see it");
        }
    }

    // ---- parser unit tests ------------------------------------------------
    //
    // The scans above are the guard's eyes: if they silently parse nothing,
    // every assertion passes vacuously. These pin them against literals.

    #[test]
    fn declared_bin_names_reads_explicit_bin_sections() {
        let manifest = r#"
[package]
name = "kastellan-worker-prelude"

[[bin]]
name = "kastellan-lockdown-probe"
path = "src/bin/probe.rs"

[[bin]]
name = "kastellan-worker-lockdown-exec"
path = "src/bin/exec.rs"

[dependencies]
name-like-key = "1"
"#;
        assert_eq!(
            declared_bin_names(manifest, "kastellan-worker-prelude", false),
            vec!["kastellan-lockdown-probe", "kastellan-worker-lockdown-exec"]
        );
    }

    #[test]
    fn declared_bin_names_falls_back_to_the_package_default_binary() {
        let manifest = "[package]\nname = \"kastellan-worker-new\"\n";
        assert_eq!(
            declared_bin_names(manifest, "kastellan-worker-new", true),
            vec!["kastellan-worker-new"]
        );
        // No `src/main.rs` and no `[[bin]]` ⇒ a library crate, no binary.
        assert!(declared_bin_names(manifest, "kastellan-worker-new", false).is_empty());
    }

    #[test]
    fn workspace_members_parses_the_array() {
        let root = "[workspace]\nmembers = [\n    \"core\",\n    # a comment\n    \"workers/mail\",\n]\nresolver = \"2\"\n";
        assert_eq!(workspace_members(root), vec!["core", "workers/mail"]);
    }

    #[test]
    fn package_name_stops_before_bin_sections() {
        let manifest = "[package]\nname = \"pkg\"\n\n[[bin]]\nname = \"other\"\n";
        assert_eq!(package_name(manifest).as_deref(), Some("pkg"));
    }
}
