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
//! **every binary any workspace member builds is accounted for** — present
//! in `required_binaries()`, in `optional_binaries()`, or in
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
//! # "Every binary" means all three of Cargo's discovery modes
//!
//! A crate can produce a binary three ways, and a guard that understood only
//! some of them would go quietly blind on the rest — reproducing #504
//! through a door it was not watching:
//!
//!   * an explicit `[[bin]]` section, named by its `name` key (a crate may
//!     declare several, as `prelude` does);
//!   * `src/main.rs` — the package-named default binary;
//!   * **auto-discovery** (`autobins`, on by default): any `src/bin/*.rs` or
//!     `src/bin/*/main.rs`, named after the file stem or directory.
//!
//! The third is why [`tests::declared_bin_names`] takes candidates gathered
//! from the filesystem rather than a single `has_src_main` flag. Explicit
//! sections and auto-discovery overlap — every binary in this workspace is
//! *currently* declared explicitly, and `prelude` declares names that differ
//! from its file stems — so candidates are suppressed by claimed **path**
//! first and name second, the way Cargo merges them. Get that wrong in the
//! other direction and the guard invents binaries that do not exist.
//!
//! # Why a real TOML parser
//!
//! An earlier draft hand-rolled a line scanner to avoid a dev-dep. That was
//! the wrong trade twice over: `toml` is already a workspace pin `core`
//! depends on, so there was no new dependency to avoid; and this guard's
//! failure mode is a silent **false pass**, which is precisely what a line
//! scanner delivers — a commented-out `# "workers/retired",` parses as a
//! live member, exactly the shape of the #479 bug where a `contains()` check
//! was satisfied by a `# shellcheck source=` comment instead of the real
//! line. The parser is the guard's eyes; it does not get to be approximate.
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

    /// Parse a manifest, failing loudly. `who` names the crate in the panic:
    /// a guard that cannot read a manifest must say which one.
    fn parse(manifest: &str, who: &str) -> toml::Table {
        manifest
            .parse::<toml::Table>()
            .unwrap_or_else(|e| panic!("parse {who} manifest: {e}"))
    }

    /// Workspace member paths, in declaration order.
    fn workspace_members(root_manifest: &str) -> Vec<String> {
        let doc = parse(root_manifest, "workspace root");
        let members = doc
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
            .expect("root manifest has [workspace] members = [...]");
        members
            .iter()
            .map(|m| {
                m.as_str()
                    .expect("workspace member entries are strings")
                    .to_string()
            })
            .collect()
    }

    /// The `package.name` of a manifest.
    fn package_name(manifest: &str, who: &str) -> Option<String> {
        parse(manifest, who)
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    }

    /// Manifest-relative target paths, compared the way Cargo resolves them:
    /// separator-normalised and without a leading `./`.
    fn normalize_path(p: &str) -> String {
        p.replace('\\', "/")
            .trim_start_matches("./")
            .to_string()
    }

    /// `(binary name, manifest-relative path)` for every target Cargo would
    /// **auto-discover** in `dir`, sorted for determinism.
    ///
    /// The filesystem half of the three discovery modes: `src/main.rs` plus
    /// `src/bin/*.rs` and `src/bin/*/main.rs`. Whether any of these survive
    /// is [`declared_bin_names`]'s call — an explicit `[[bin]]` claiming the
    /// same path absorbs the candidate rather than adding a second target.
    fn auto_bin_candidates(dir: &Path, package_name: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if dir.join("src").join("main.rs").is_file() {
            out.push((package_name.to_string(), "src/main.rs".to_string()));
        }
        let bin_dir = dir.join("src").join("bin");
        match std::fs::read_dir(&bin_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry =
                        entry.unwrap_or_else(|e| panic!("read {}: {e}", bin_dir.display()));
                    let path = entry.path();
                    let file_name = entry.file_name();
                    let file_name = file_name.to_string_lossy();
                    if path.is_dir() {
                        // `src/bin/<dir>/main.rs` ⇒ a binary named `<dir>`.
                        if path.join("main.rs").is_file() {
                            out.push((
                                file_name.to_string(),
                                format!("src/bin/{file_name}/main.rs"),
                            ));
                        }
                    } else if let Some(stem) = file_name.strip_suffix(".rs") {
                        out.push((stem.to_string(), format!("src/bin/{stem}.rs")));
                    }
                }
            }
            // No `src/bin` at all is the common case, not a problem. Any
            // other error is: a guard that cannot see the directory must not
            // conclude it is empty.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("read {}: {e}", bin_dir.display()),
        }
        out.sort();
        out
    }

    /// Binary names a crate produces: its explicit `[[bin]]` sections plus
    /// whichever of `auto_candidates` Cargo's auto-discovery would still add.
    ///
    /// Pure: the caller does the filesystem work and passes the answers in,
    /// so the resolution is unit-testable against literals.
    fn declared_bin_names(
        manifest: &str,
        package_name: &str,
        auto_candidates: &[(String, String)],
    ) -> Vec<String> {
        let doc = parse(manifest, package_name);

        let mut names: Vec<String> = Vec::new();
        let mut claimed_paths: BTreeSet<String> = BTreeSet::new();
        if let Some(bins) = doc.get("bin").and_then(|b| b.as_array()) {
            for (i, bin) in bins.iter().enumerate() {
                let name = bin.get("name").and_then(|n| n.as_str()).unwrap_or_else(|| {
                    panic!("{package_name}: [[bin]] #{i} has no `name` key — the guard \
                            cannot account for a binary it cannot name")
                });
                names.push(name.to_string());
                if let Some(p) = bin.get("path").and_then(|p| p.as_str()) {
                    claimed_paths.insert(normalize_path(p));
                }
            }
        }

        // `autobins = false` switches auto-discovery off wholesale.
        let autobins = doc
            .get("package")
            .and_then(|p| p.get("autobins"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !autobins {
            return names;
        }

        let explicit: BTreeSet<String> = names.iter().cloned().collect();
        for (name, path) in auto_candidates {
            // Path first: `prelude` declares `src/bin/lockdown_probe.rs` under
            // the name `kastellan-lockdown-probe`, so a name-only check would
            // invent a phantom `lockdown_probe` target.
            if claimed_paths.contains(&normalize_path(path)) || explicit.contains(name) {
                continue;
            }
            names.push(name.clone());
        }
        names
    }

    /// Every binary one workspace member builds, resolved against the real
    /// manifest and the real directory layout.
    fn member_binaries(member: &str) -> Vec<String> {
        let dir = repo_root().join(member);
        let manifest_path = dir.join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
        let pkg = package_name(&manifest, member)
            .unwrap_or_else(|| panic!("no package name in {}", manifest_path.display()));
        let auto = auto_bin_candidates(&dir, &pkg);
        declared_bin_names(&manifest, &pkg, &auto)
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
            bins.extend(member_binaries(member));
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

    // ---- the scan's eyes, against the real tree ---------------------------
    //
    // The `src/bin` half of discovery is only worth having if it actually
    // sees a `src/bin`. These pin it against the two crates in this tree that
    // have one, in both directions: it must find the directory, and it must
    // not invent targets from files an explicit `[[bin]]` already claims.

    #[test]
    fn the_src_bin_scan_is_not_blind() {
        let core = repo_root().join("core");
        let candidates = auto_bin_candidates(&core, "kastellan-core");
        assert!(
            candidates.contains(&(
                "kastellan-cli".to_string(),
                "src/bin/kastellan-cli/main.rs".to_string()
            )),
            "auto-discovery missed core's src/bin/kastellan-cli/ directory: {candidates:?}"
        );
        assert!(
            candidates.contains(&("kastellan-core".to_string(), "src/main.rs".to_string())),
            "auto-discovery missed core's src/main.rs: {candidates:?}"
        );
    }

    /// `core` declares both its binaries explicitly, one of them over
    /// `src/main.rs`. The package-named candidate must be absorbed, not
    /// added: a phantom `kastellan-core` binary would be unaccounted for and
    /// would fail the guard for a reason that does not exist.
    #[test]
    fn explicit_sections_absorb_the_default_binary() {
        assert_eq!(member_binaries("core"), vec!["kastellan", "kastellan-cli"]);
    }

    /// `prelude` is the crate that proves path-before-name dedup is needed:
    /// both its `[[bin]]` names differ from their `src/bin/*.rs` stems.
    #[test]
    fn explicit_sections_absorb_src_bin_files_by_path() {
        assert_eq!(
            member_binaries("workers/prelude"),
            vec!["kastellan-lockdown-probe", "kastellan-worker-lockdown-exec"]
        );
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
            declared_bin_names(manifest, "kastellan-worker-prelude", &[]),
            vec!["kastellan-lockdown-probe", "kastellan-worker-lockdown-exec"]
        );
    }

    #[test]
    fn declared_bin_names_falls_back_to_the_package_default_binary() {
        let manifest = "[package]\nname = \"kastellan-worker-new\"\n";
        let main_only = [("kastellan-worker-new".to_string(), "src/main.rs".to_string())];
        assert_eq!(
            declared_bin_names(manifest, "kastellan-worker-new", &main_only),
            vec!["kastellan-worker-new"]
        );
        // No `src/main.rs`, no `src/bin`, no `[[bin]]` ⇒ a library crate.
        assert!(declared_bin_names(manifest, "kastellan-worker-new", &[]).is_empty());
    }

    /// The #504 shape the earlier draft could not see: a crate that ships a
    /// second binary as a bare `src/bin/*.rs` with no `[[bin]]` section.
    /// Cargo builds it; before this, the guard did not know it existed.
    #[test]
    fn declared_bin_names_covers_bare_src_bin_autodiscovery() {
        let manifest = "[package]\nname = \"kastellan-worker-new\"\n";
        let auto = [
            ("kastellan-worker-new".to_string(), "src/main.rs".to_string()),
            ("helper-tool".to_string(), "src/bin/helper-tool.rs".to_string()),
        ];
        assert_eq!(
            declared_bin_names(manifest, "kastellan-worker-new", &auto),
            vec!["kastellan-worker-new", "helper-tool"]
        );
    }

    /// Dedup is by path first. An explicit section renaming a `src/bin` file
    /// must absorb it, or the guard reports a binary Cargo never builds.
    #[test]
    fn declared_bin_names_dedupes_autodiscovery_by_claimed_path() {
        let manifest = r#"
[package]
name = "kastellan-worker-prelude"

[[bin]]
name = "kastellan-lockdown-probe"
path = "./src/bin/lockdown_probe.rs"
"#;
        let auto = [(
            "lockdown_probe".to_string(),
            "src/bin/lockdown_probe.rs".to_string(),
        )];
        assert_eq!(
            declared_bin_names(manifest, "kastellan-worker-prelude", &auto),
            vec!["kastellan-lockdown-probe"]
        );
    }

    #[test]
    fn declared_bin_names_honours_autobins_false() {
        let manifest = "[package]\nname = \"pkg\"\nautobins = false\n";
        let auto = [("stray".to_string(), "src/bin/stray.rs".to_string())];
        assert!(declared_bin_names(manifest, "pkg", &auto).is_empty());
    }

    #[test]
    fn workspace_members_parses_the_array() {
        let root = r#"
[workspace]
members = [
    "core",
    # a comment
    # "workers/retired",
    "workers/mail",
]
resolver = "2"
"#;
        // The commented-out member is a comment, not a member. A line scanner
        // that took the first quoted run per line saw it as live (#479's shape).
        assert_eq!(workspace_members(root), vec!["core", "workers/mail"]);
    }

    #[test]
    fn package_name_ignores_bin_and_dependency_sections() {
        let manifest = "[package]\nname = \"pkg\"\n\n[[bin]]\nname = \"other\"\n";
        assert_eq!(package_name(manifest, "t").as_deref(), Some("pkg"));
        let dep_first = "[dependencies]\nname-like = \"1\"\n\n[package]\nname = \"pkg\"\n";
        assert_eq!(package_name(dep_first, "t").as_deref(), Some("pkg"));
    }
}
