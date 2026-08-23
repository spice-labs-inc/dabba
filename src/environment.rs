//! The pinned environment: one place that decides which versions a laptop, a CI
//! runner and a cloud box all resolve to.
//!
//! # Why this is its own thing
//!
//! It is not an `Application` field. A toolchain pin is consumed by things that
//! are not containers at all — a developer's shell, a GitHub runner — so folding
//! it into the workload schema would repeat the union-schema mistake the
//! intersection rule exists to prevent.
//!
//! It is not a substrate setting either, because it is the same everywhere by
//! definition. A pin that varies per environment is not a pin.
//!
//! # Two kinds of version, and why they are separated
//!
//! **`toolchain`** is what a human or a runner must have installed: the Rust
//! compiler, its extra targets, and the command-line tools a build uses. Nothing
//! deploys these; they are checked.
//!
//! **`services`** are versions of things dabba actually runs — Postgres, OpenBao,
//! the object store. Those already have a home: the `tag` of an
//! [`crate::application::Application`]. Restating them here would create exactly
//! the two-lists-drifting problem that has bitten this codebase four times.
//!
//! So they are not restated. The pin is the authority, and
//! [`check_application_versions`] reports any Application whose tag disagrees with
//! it. Drift is caught rather than prevented, which is the only option that does
//! not require a templating language in the schema.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The pinned environment, from `spec.environment`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Environment {
    /// What must be installed to build. Checked, never deployed.
    #[serde(default)]
    pub toolchain: Toolchain,
    /// Versions of services dabba runs. The authority that Application tags are
    /// checked against.
    #[serde(default)]
    pub services: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Toolchain {
    /// Rust toolchain version, e.g. "1.83.0". Empty means unpinned.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rust: String,
    /// Extra compilation targets the build needs, e.g. `wasm32-wasip2`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rust_targets: Vec<String>,
    /// Command-line tools and their pinned versions, e.g. `just: "1.36.0"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
}

impl Environment {
    pub fn is_empty(&self) -> bool {
        self.toolchain == Toolchain::default() && self.services.is_empty()
    }
}

/// What a pin looks like on this machine right now.
#[derive(Debug, PartialEq)]
pub enum State {
    /// Installed and matching.
    Matches(String),
    /// Installed, but a different version.
    Drifted { pinned: String, installed: String },
    /// Not installed at all.
    Missing,
}

impl State {
    fn marker(&self) -> &'static str {
        match self {
            State::Matches(_) => "✓",
            State::Drifted { .. } => "≠",
            State::Missing => "✗",
        }
    }

    fn is_problem(&self) -> bool {
        !matches!(self, State::Matches(_))
    }
}

/// Extract a version number from a `--version` line.
///
/// Every tool spells this differently — "rustc 1.83.0 (90b35a623 2024-11-26)",
/// "just 1.36.0", "sccache 0.8.2" — so rather than a parser per tool, take the
/// first dotted-numeric token. That covers every tool encountered so far and
/// degrades to "no version found" rather than to a wrong one.
pub fn extract_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| {
            let core = token.trim_start_matches('v');
            core.contains('.')
                && core.chars().next().is_some_and(|c| c.is_ascii_digit())
                && core
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        })
        .map(|token| token.trim_start_matches('v').to_string())
}

/// Compare a pinned version against what a command reports.
///
/// A pin of "16" against an installed "16.4.1" MATCHES: pinning a major version
/// and accepting its patches is a normal thing to want, and treating it as drift
/// would make the check cry wolf until people stopped reading it.
fn compare(pinned: &str, installed: Option<String>) -> State {
    match installed {
        None => State::Missing,
        Some(installed) => {
            let matches = installed == pinned
                || installed
                    .strip_prefix(pinned)
                    .is_some_and(|rest| rest.starts_with('.'));
            if matches {
                State::Matches(installed)
            } else {
                State::Drifted {
                    pinned: pinned.to_string(),
                    installed,
                }
            }
        }
    }
}

fn installed_version(binary: &str, args: &[&str]) -> Option<String> {
    crate::run::capture_including_failures(binary, args).and_then(|out| extract_version(&out))
}

/// Every pin, and its state on this machine.
pub fn inspect(environment: &Environment) -> Vec<(String, State)> {
    let mut results = Vec::new();

    if !environment.toolchain.rust.is_empty() {
        results.push((
            "rust".to_string(),
            compare(
                &environment.toolchain.rust,
                installed_version("rustc", &["--version"]),
            ),
        ));
    }

    for target in &environment.toolchain.rust_targets {
        // A target is installed or not; there is no version to compare.
        let installed =
            crate::run::capture_including_failures("rustup", &["target", "list", "--installed"])
                .unwrap_or_default();
        let state = if installed.lines().any(|line| line.trim() == target) {
            State::Matches("installed".to_string())
        } else {
            State::Missing
        };
        results.push((format!("rust target {target}"), state));
    }

    for (tool, pinned) in &environment.toolchain.tools {
        results.push((
            tool.clone(),
            compare(pinned, installed_version(tool, &["--version"])),
        ));
    }

    results
}

/// `dabba environment show` — the pins and how this machine compares.
pub fn show(environment: &Environment) -> Result<()> {
    if environment.is_empty() {
        println!(
            "No pinned environment. Add `spec.environment` to pin the toolchain and \
             service versions that a laptop, CI and the cloud should all resolve to."
        );
        return Ok(());
    }

    println!("toolchain");
    let states = inspect(environment);
    if states.is_empty() {
        println!("  (nothing pinned)");
    }
    for (name, state) in &states {
        match state {
            State::Matches(installed) => println!("  {} {name:<22} {installed}", state.marker()),
            State::Drifted { pinned, installed } => println!(
                "  {} {name:<22} pinned {pinned}, installed {installed}",
                state.marker()
            ),
            State::Missing => println!("  {} {name:<22} not installed", state.marker()),
        }
    }

    if !environment.services.is_empty() {
        println!("\nservices");
        for (service, version) in &environment.services {
            println!("    {service:<22} {version}");
        }
    }
    Ok(())
}

/// `dabba environment check` — non-zero when this machine does not match the pin.
///
/// A pin nobody checks is a comment. This is what makes it a fact.
pub fn check(environment: &Environment) -> Result<()> {
    if environment.is_empty() {
        bail!(
            "no pinned environment to check; add `spec.environment` to the config \
             first"
        );
    }
    let states = inspect(environment);
    let problems: Vec<&(String, State)> = states.iter().filter(|(_, s)| s.is_problem()).collect();

    for (name, state) in &states {
        match state {
            State::Matches(installed) => println!("  ✓ {name:<22} {installed}"),
            State::Drifted { pinned, installed } => {
                println!("  ≠ {name:<22} pinned {pinned}, installed {installed}")
            }
            State::Missing => println!("  ✗ {name:<22} not installed"),
        }
    }

    if problems.is_empty() {
        println!("\n✓ this machine matches the pinned environment");
        return Ok(());
    }
    bail!(
        "{} of {} pins do not match: {}",
        problems.len(),
        states.len(),
        problems
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `dabba environment export` — shell exports, for a workflow or a shell to eval.
pub fn export(environment: &Environment) -> Result<()> {
    if !environment.toolchain.rust.is_empty() {
        println!("DABBA_RUST_VERSION={}", environment.toolchain.rust);
    }
    if !environment.toolchain.rust_targets.is_empty() {
        println!(
            "DABBA_RUST_TARGETS={}",
            environment.toolchain.rust_targets.join(",")
        );
    }
    for (tool, version) in &environment.toolchain.tools {
        println!("DABBA_TOOL_{}={version}", shell_name(tool));
    }
    for (service, version) in &environment.services {
        println!("DABBA_SERVICE_{}={version}", shell_name(service));
    }
    Ok(())
}

/// A tool name as a shell-safe uppercase identifier: `cargo-deny` -> `CARGO_DENY`.
fn shell_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Report Application definitions whose image tag disagrees with the service pin.
///
/// The pin is the authority and the Application carries the tag; this is what
/// keeps them honest without restating the version in two files or introducing a
/// templating language. Returns one line per disagreement.
pub fn check_application_versions(
    environment: &Environment,
    applications: &[(String, crate::application::Application)],
) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, application) in applications {
        // Match on the last path segment of the image: `minio/minio` -> `minio`,
        // `postgres` -> `postgres`, `openbao/openbao` -> `openbao`.
        let image = application
            .spec
            .image
            .rsplit('/')
            .next()
            .unwrap_or(&application.spec.image);
        let Some(pinned) = environment.services.get(image) else {
            continue;
        };
        let tag = &application.spec.tag;
        let agrees = tag == pinned || tag.strip_prefix(pinned).is_some_and(|r| r.starts_with('.'));
        if !agrees {
            problems.push(format!(
                "{name}: image {image} is pinned to {pinned} in spec.environment.services \
                 but the definition uses {tag}"
            ));
        }
    }
    problems
}

/// Report hand-written compose files whose image tag disagrees with a service pin.
///
/// Applications are not the only place a pinned version appears: the reconciler
/// deliberately still accepts hand-written compose stacks, and the OpenBao example
/// is one. A pin that only reached rendered artifacts would silently miss exactly
/// the stack it was written for.
///
/// This reads `image: name:tag` lines textually rather than parsing compose,
/// because it needs to work on a file that may use compose features this codebase
/// does not model, and a pin check should never be the thing that rejects a valid
/// stack.
pub fn check_compose_versions(
    environment: &Environment,
    files: &[(String, String)],
) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, contents) in files {
        for line in contents.lines() {
            let Some(reference) = line.trim().strip_prefix("image:") else {
                continue;
            };
            let reference = reference.trim().trim_matches(|c| c == '"' || c == '\'');
            // Split off the tag, being careful that a registry port (host:5000/x)
            // is not mistaken for one.
            let Some((image, tag)) = reference.rsplit_once(':') else {
                continue;
            };
            if tag.contains('/') {
                continue; // that colon was a registry port, not a tag
            }
            let software = image.rsplit('/').next().unwrap_or(image);
            let Some(pinned) = environment.services.get(software) else {
                continue;
            };
            let agrees =
                tag == pinned || tag.strip_prefix(pinned).is_some_and(|r| r.starts_with('.'));
            if !agrees {
                problems.push(format!(
                    "{name}: image {software} is pinned to {pinned} in \
                     spec.environment.services but the stack uses {tag}"
                ));
            }
        }
    }
    problems
}

/// Every `docker-compose.yml` under a directory tree, for the version check.
pub fn load_compose_files(directory: &std::path::Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("docker-compose.yml") {
                let text = std::fs::read_to_string(&path)?;
                files.push((
                    path.strip_prefix(directory)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                    text,
                ));
            }
        }
    }
    Ok(files)
}

/// Load every Application under a directory, for the version check.
pub fn load_applications(
    directory: &std::path::Path,
) -> Result<Vec<(String, crate::application::Application)>> {
    let mut applications = Vec::new();
    let entries =
        std::fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let Ok(application) = crate::application::Application::parse(&text) else {
            continue; // not an Application; the config lives here too
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        applications.push((name, application));
    }
    Ok(applications)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_version_from_every_spelling_seen_so_far() {
        assert_eq!(
            extract_version("rustc 1.83.0 (90b35a623 2024-11-26)").as_deref(),
            Some("1.83.0")
        );
        assert_eq!(extract_version("just 1.36.0").as_deref(), Some("1.36.0"));
        assert_eq!(extract_version("sccache 0.8.2").as_deref(), Some("0.8.2"));
        assert_eq!(extract_version("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(
            extract_version("psql (PostgreSQL) 16.4").as_deref(),
            Some("16.4")
        );
        // No version rather than a wrong one.
        assert_eq!(extract_version("no numbers here"), None);
        assert_eq!(extract_version(""), None);
    }

    /// Pinning a major and accepting its patches has to match, or the check cries
    /// wolf until people stop reading it.
    #[test]
    fn a_major_pin_accepts_its_patch_releases() {
        assert_eq!(
            compare("16", Some("16.4.1".into())),
            State::Matches("16.4.1".into())
        );
        assert_eq!(
            compare("1.83", Some("1.83.0".into())),
            State::Matches("1.83.0".into())
        );
    }

    /// But a different version is drift, and a prefix that is not a version
    /// boundary is NOT a match — 16 must not accept 161.
    #[test]
    fn a_different_version_is_drift() {
        assert!(matches!(
            compare("1.83.0", Some("1.82.0".into())),
            State::Drifted { .. }
        ));
        assert!(
            matches!(compare("16", Some("161.0".into())), State::Drifted { .. }),
            "16 must not match 161 — a prefix is not a version boundary"
        );
        assert_eq!(compare("1.83.0", None), State::Missing);
    }

    fn application(image: &str, tag: &str) -> crate::application::Application {
        let text = format!(
            "apiVersion: dabba.spicelabs.io/v1alpha1\nkind: Application\n\
             metadata:\n  name: thing\nspec:\n  image: {image}\n  tag: \"{tag}\"\n"
        );
        crate::application::Application::parse(&text).unwrap()
    }

    #[test]
    fn an_application_disagreeing_with_the_pin_is_reported() {
        let mut environment = Environment::default();
        environment.services.insert("postgres".into(), "16".into());

        let agreeing = vec![("ok.yaml".to_string(), application("postgres", "16.4"))];
        assert!(check_application_versions(&environment, &agreeing).is_empty());

        let disagreeing = vec![("bad.yaml".to_string(), application("postgres", "15"))];
        let problems = check_application_versions(&environment, &disagreeing);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("pinned to 16"));
        assert!(problems[0].contains("uses 15"));
    }

    /// The image may be namespaced; the pin names the software, not the registry
    /// path, so `minio/minio` has to match a `minio` pin.
    #[test]
    fn a_namespaced_image_matches_the_pin_for_its_software() {
        let mut environment = Environment::default();
        environment
            .services
            .insert("minio".into(), "RELEASE.2024".into());
        let apps = vec![(
            "cache.yaml".to_string(),
            application("minio/minio", "RELEASE.2023"),
        )];
        assert_eq!(check_application_versions(&environment, &apps).len(), 1);
    }

    /// An unpinned service is not a problem: pinning is opt-in per service, and
    /// reporting every unpinned image would make the check useless noise.
    #[test]
    fn an_unpinned_service_is_left_alone() {
        let environment = Environment::default();
        let apps = vec![("x.yaml".to_string(), application("nginx", "alpine"))];
        assert!(check_application_versions(&environment, &apps).is_empty());
    }

    #[test]
    fn a_hand_written_compose_stack_is_checked_against_the_pin() {
        let mut environment = Environment::default();
        environment
            .services
            .insert("openbao".into(), "2.1.0".into());

        let agreeing = vec![(
            "openbao/docker-compose.yml".to_string(),
            "services:\n  openbao:\n    image: openbao/openbao:2.1.0\n".to_string(),
        )];
        assert!(check_compose_versions(&environment, &agreeing).is_empty());

        let disagreeing = vec![(
            "openbao/docker-compose.yml".to_string(),
            "services:\n  openbao:\n    image: openbao/openbao:1.14.0\n".to_string(),
        )];
        let problems = check_compose_versions(&environment, &disagreeing);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("uses 1.14.0"));
    }

    /// A registry port is not a tag. `registry:5000/openbao` must not be read as
    /// image `registry` at version `5000/openbao`.
    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        let mut environment = Environment::default();
        environment.services.insert("registry".into(), "2".into());
        let files = vec![(
            "x.yml".to_string(),
            "    image: registry:5000/openbao/openbao\n".to_string(),
        )];
        assert!(check_compose_versions(&environment, &files).is_empty());
    }

    #[test]
    fn an_untagged_image_is_left_alone() {
        let mut environment = Environment::default();
        environment.services.insert("nginx".into(), "1.27".into());
        let files = vec![("x.yml".to_string(), "    image: nginx\n".to_string())];
        assert!(check_compose_versions(&environment, &files).is_empty());
    }

    #[test]
    fn shell_names_are_safe_identifiers() {
        assert_eq!(shell_name("just"), "JUST");
        assert_eq!(shell_name("cargo-deny"), "CARGO_DENY");
        assert_eq!(shell_name("wasm-tools"), "WASM_TOOLS");
    }

    #[test]
    fn an_empty_environment_is_recognised_as_unpinned() {
        assert!(Environment::default().is_empty());
        let mut pinned = Environment::default();
        pinned.toolchain.rust = "1.83.0".into();
        assert!(!pinned.is_empty());
    }
}
