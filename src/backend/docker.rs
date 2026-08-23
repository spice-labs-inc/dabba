//! `DockerBackend` — dabba's bare-OS docker-compose path. It drives the portable
//! gitops reconciler under `backends/docker/`: a per-box loop (launchd on macOS,
//! systemd --user on Linux) that every minute pulls a gitops repo and converges the
//! host's `docker compose` stacks to it — the Flux equivalent for compose hosts.
//!
//! This backend is a thin Rust wrapper that shells out to that reconciler, the same
//! way the KubernetesBackend shells to tofu/kubectl:
//!
//! - `up` — ensure the gitops repo is cloned locally, then run `install.sh` (which
//!   installs + kickstarts the reconcile loop for this platform).
//! - `status` — report whether the loop is installed and run `docker compose ps`
//!   across the managed `gitops-*` projects.
//! - `down` — run `uninstall.sh` (stop the loop). It honors the reconciler's
//!   never-destroy contract: running stacks, volumes and data are left in place;
//!   decommissioning a stack stays a deliberate manual act.
//! - `diagram`/`secret ls`/`secret get` — not yet on this backend; each returns a
//!   clear message pointing at the seam (see below).
//!
//! Config knobs (all optional, read from the env's free-form `substrateConfig`, the
//! same mechanism the EKS substrate uses for region/k8sVersion):
//!
//! - `boxName` — this box's directory name in the gitops repo. Default: unset, so the
//!   reconciler falls back to `hostname -s`.
//! - `appsDir` — the apps subtree under the gitops repo. Default `apps`.
//! - `backendDir` — a working copy of `backends/docker/` to drive instead of the
//!   reconciler embedded in this binary. For developing the reconciler itself, where
//!   editing a script and re-running is the whole loop; unset in normal use.
//!
//! The reconciler is compiled into the binary and written into the environment's
//! working directory on demand — see [`crate::backend::reconciler_assets`] for why
//! that is not merely a convenience.
//!
//! SEAM for a later increment (deliberately not built here): `diagram` for compose
//! topology. `dabba status` covers the same ground in text.

use crate::backend::common::{env_workdir, expand_tilde, log, on_path};
use crate::backend::reconciler_assets::{self, REQUIRED_SCRIPTS};
use crate::backend::{Backend, DownOptions, Options};
use crate::config::{DabbaConfig, ResolvedEnv};
use crate::run;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The scheduler identity for an environment's reconcile loop. These must match
/// the derivation in `backends/docker/install.sh` and `uninstall.sh` exactly — the
/// scripts install under these names and this is how `status` finds them.
///
/// They are per-environment because dabba supports multiple environments and the
/// loop used to install under a single fixed name: a second docker-host
/// environment silently replaced the first one's agent, and tearing down either
/// stopped both.
fn launchd_label(env_name: &str) -> String {
    format!("io.spicelabs.dabba.reconcile.{env_name}")
}

fn systemd_timer(env_name: &str) -> String {
    format!("gitops-reconcile-{env_name}.timer")
}

/// The bare-OS docker-compose backend. Selected for the `docker-host` substrate.
pub struct DockerBackend;

impl Backend for DockerBackend {
    fn up(&self, opts: &Options) -> Result<()> {
        up(opts)
    }
    fn down(&self, opts: &DownOptions) -> Result<()> {
        down(opts)
    }
    fn status(&self, config: &Path, env_name: Option<&str>) -> Result<()> {
        status(config, env_name)
    }
    fn diagram(&self, _config: &Path, _env_name: Option<&str>, _mermaid: bool) -> Result<()> {
        bail!(
            "diagram is not yet implemented for the docker backend — rendering the \
             compose topology is a later increment; use `dabba status` for now"
        )
    }
    fn secret_ls(
        &self,
        _config: &Path,
        _env_name: Option<&str>,
        _path: Option<&str>,
    ) -> Result<()> {
        bail!(secret_not_yet())
    }
    fn secret_get(&self, _config: &Path, _env_name: Option<&str>, _name: &str) -> Result<()> {
        bail!(secret_not_yet())
    }
}

fn secret_not_yet() -> &'static str {
    "secrets are not yet wired on the docker backend. The compose-host equivalent of \
     External Secrets (OpenBao shipped as a managed gitops-openbao container) is a \
     later increment; today the reconciler reads a box-local .env only."
}

fn up(opts: &Options) -> Result<()> {
    let cfg = DabbaConfig::load(&opts.config)?;
    let env = cfg.resolve(opts.env.as_deref())?;
    let workdir = env_workdir(&opts.config, &env.name)?;
    let backend_dir = docker_backend_dir(&env, &workdir)?;

    // Preflight: the reconciler needs docker (running), git and bash.
    for tool in ["docker", "git", "bash"] {
        if !on_path(tool) {
            bail!("{tool} not found on PATH (the docker backend needs docker, git and bash)");
        }
    }
    if !run::probe("docker", &["info"]) {
        bail!("docker is not running — start Docker and retry");
    }

    // GITOPS_DIR is the clone the reconcile loop converges from; STACKS_DIR is where
    // it applies stacks (reconciler-owned). Both live under the per-env workdir.
    let gitops_dir = workdir.join("gitops");
    let stacks_dir = workdir.join("stacks");
    std::fs::create_dir_all(&stacks_dir)
        .with_context(|| format!("creating {}", stacks_dir.display()))?;

    let source = gitops_source(&cfg, opts)?;
    ensure_gitops_clone(&env.name, &source, &gitops_dir)?;

    log(&format!(
        "[{}] installing the docker reconcile loop ({} / {})",
        env.name,
        std::env::consts::OS,
        loop_kind()
    ));
    let install = backend_dir.join("install.sh");
    let install_str = install.to_string_lossy();
    run::run_with_environment(
        "bash",
        &[install_str.as_ref()],
        &reconciler_environment(&env, &gitops_dir, &stacks_dir),
    )?;

    print_up_summary(&env.name, &gitops_dir, &stacks_dir, &backend_dir);
    Ok(())
}

fn down(opts: &DownOptions) -> Result<()> {
    let cfg = DabbaConfig::load(&opts.config)?;
    let env = cfg.resolve(opts.env.as_deref())?;
    let workdir = env_workdir(&opts.config, &env.name)?;
    let backend_dir = docker_backend_dir(&env, &workdir)?;

    log(&format!(
        "[{}] stopping the docker reconcile loop (leaving running stacks in place)",
        env.name
    ));
    let uninstall = backend_dir.join("uninstall.sh");
    let uninstall_str = uninstall.to_string_lossy();
    // uninstall.sh derives which environment's loop to remove from this.
    run::run_with_environment(
        "bash",
        &[uninstall_str.as_ref()],
        &[("DABBA_ENVIRONMENT".to_string(), Some(env.name.clone()))],
    )?;

    println!(
        "✓ {} reconcile loop stopped.\n  \
         Per the reconciler's never-destroy contract, running stacks, volumes and data \
         were left untouched.\n  \
         To fully decommission a stack, tear it down by hand (e.g. `docker compose -p \
         gitops-<app> down`).",
        env.name
    );
    Ok(())
}

fn status(config: &Path, env_name: Option<&str>) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let workdir = env_workdir(config, &env.name)?;
    let stacks_dir = workdir.join("stacks");

    println!(
        "environment: {}  (substrate: {:?}, domain: {})",
        env.name, env.substrate, env.domain
    );

    let installed = loop_installed(&env.name);
    println!(
        "  reconcile loop: {} ({})",
        if installed {
            "✓ installed"
        } else {
            "✗ not installed (run `up`)"
        },
        loop_kind()
    );

    if !run::probe("docker", &["info"]) {
        println!("  docker:         ✗ daemon not reachable");
        return Ok(());
    }

    // The apps the reconciler has applied are exactly the subdirectories of
    // STACKS_DIR that hold a docker-compose.yml; each is compose project gitops-<app>.
    let apps = applied_apps(&stacks_dir);
    if apps.is_empty() {
        println!("  managed stacks: (none applied yet)");
        return Ok(());
    }
    println!("  managed stacks:");
    for app in apps {
        let project = format!("gitops-{app}");
        println!("    {project}:");
        let ps = compose_ps(&project);
        let body = ps.trim();
        if body.is_empty() {
            println!("      (no containers running)");
        } else {
            for line in body.lines() {
                println!("      {line}");
            }
        }
    }
    Ok(())
}

/// `docker compose -p <project> ps` for a managed project, falling back to a plain
/// `docker ps` label filter if compose reports nothing (both are honest views of the
/// project's containers).
fn compose_ps(project: &str) -> String {
    if let Some(out) = run::capture(
        "docker",
        &[
            "compose",
            "-p",
            project,
            "ps",
            "--format",
            "table {{.Name}}\t{{.Service}}\t{{.Status}}\t{{.Ports}}",
        ],
    ) {
        // compose prints just a header row when a project has no containers; treat
        // that as empty so the caller can fall back.
        if out.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
            return out;
        }
    }
    run::capture(
        "docker",
        &[
            "ps",
            "-a",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
            "--format",
            "table {{.Names}}\t{{.Status}}\t{{.Ports}}",
        ],
    )
    .unwrap_or_default()
}

/// Subdirectories of STACKS_DIR that hold a `docker-compose.yml` — the apps the
/// reconciler has applied (each named `<app>`, project `gitops-<app>`).
fn applied_apps(stacks_dir: &Path) -> Vec<String> {
    let mut apps = Vec::new();
    let Ok(entries) = std::fs::read_dir(stacks_dir) else {
        return apps;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("docker-compose.yml").is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                apps.push(name.to_string());
            }
        }
    }
    apps.sort();
    apps
}

/// Is the reconcile loop installed for this host? (launchd LaunchAgent on macOS,
/// systemd --user timer on Linux.)
fn loop_installed(env_name: &str) -> bool {
    if cfg!(target_os = "macos") {
        let Some(uid) = run::capture("id", &["-u"]).map(|s| s.trim().to_string()) else {
            return false;
        };
        run::probe(
            "launchctl",
            &["print", &format!("gui/{uid}/{}", launchd_label(env_name))],
        )
    } else {
        run::probe(
            "systemctl",
            &["--user", "is-enabled", &systemd_timer(env_name)],
        )
    }
}

/// Is this environment deployed? For a compose host that means its reconcile loop
/// is installed — the analogue of the Kubernetes backend's "the cluster exists".
pub fn is_deployed(env_name: &str) -> bool {
    loop_installed(env_name)
}

fn loop_kind() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd LaunchAgent"
    } else {
        "systemd --user timer"
    }
}

/// The gitops content to converge from: `--gitops-seed <path>` (a local path or URL),
/// else the config's `git.upstream`.
fn gitops_source(cfg: &DabbaConfig, opts: &Options) -> Result<String> {
    if let Some(seed) = &opts.gitops_seed {
        // A local path passed on the CLI; expand ~ and make it absolute so the clone
        // is cwd-independent (git clone of a local path needs a resolvable source).
        let raw = seed.to_string_lossy();
        let expanded = expand_tilde(&raw);
        let abs = expanded.canonicalize().unwrap_or(expanded);
        return Ok(abs.to_string_lossy().into_owned());
    }
    let upstream = cfg.spec.git.upstream.trim();
    if upstream.is_empty() {
        bail!("no gitops source: pass --gitops-seed <path-or-url> or set spec.git.upstream");
    }
    Ok(upstream.to_string())
}

/// Ensure `gitops_dir` is a git clone of `source`. If it already is one, leave it —
/// the reconcile loop self-updates (`git fetch` + `git reset --hard origin/main`) on
/// its next tick. `source` may be a local path or a remote URL.
fn ensure_gitops_clone(env_name: &str, source: &str, gitops_dir: &Path) -> Result<()> {
    if gitops_dir.join(".git").is_dir() {
        log(&format!(
            "[{env_name}] reusing existing gitops clone at {} (the loop self-updates)",
            gitops_dir.display()
        ));
        return Ok(());
    }
    if gitops_dir.exists() {
        bail!(
            "{} exists but is not a git clone; remove it and retry",
            gitops_dir.display()
        );
    }
    log(&format!(
        "[{env_name}] cloning gitops content from {source}"
    ));
    let gitops_str = gitops_dir.to_string_lossy();
    run::run("git", &["clone", source, gitops_str.as_ref()])
        .with_context(|| format!("cloning gitops source {source}"))?;
    Ok(())
}

/// The env's box name (its directory in the gitops repo), or None to let the
/// reconciler default to `hostname -s`.
fn box_name(env: &ResolvedEnv) -> Option<String> {
    let name = env.substrate_str("boxName", "");
    (!name.trim().is_empty()).then_some(name)
}

/// The apps subtree under the gitops repo (default `apps`).
fn apps_dir(env: &ResolvedEnv) -> String {
    env.substrate_str("appsDir", "apps")
}

/// The environment `install.sh` and `reconcile.sh` read their configuration from.
///
/// A `None` value means the variable is *removed* from the child's environment
/// rather than left to be inherited: `BOX_NAME` unset is what tells the reconciler
/// to fall back to `hostname -s`, so inheriting a stray `BOX_NAME` from whatever
/// shell invoked dabba would silently point the box at another box's stacks.
///
/// This is a plain function of the resolved environment so it can be asserted on
/// directly; the caller hands the result to [`run::run_with_environment`] rather
/// than mutating this process's own environment.
fn reconciler_environment(
    env: &ResolvedEnv,
    gitops_dir: &Path,
    stacks_dir: &Path,
) -> Vec<(String, Option<String>)> {
    vec![
        (
            "GITOPS_DIR".to_string(),
            Some(gitops_dir.to_string_lossy().into_owned()),
        ),
        (
            "STACKS_DIR".to_string(),
            Some(stacks_dir.to_string_lossy().into_owned()),
        ),
        ("GITOPS_APPS_DIR".to_string(), Some(apps_dir(env))),
        ("BOX_NAME".to_string(), box_name(env)),
        ("DABBA_ENVIRONMENT".to_string(), Some(env.name.clone())),
    ]
}

/// Resolve the reconciler directory (absolute), verifying the scripts we call are
/// present.
///
/// Normally this writes the reconciler embedded in this binary into the
/// environment's working directory, because an installed dabba has no
/// `backends/docker/` on disk at all. `substrateConfig.backendDir` overrides that
/// with a working copy, for developing the reconciler itself.
fn docker_backend_dir(env: &ResolvedEnv, workdir: &Path) -> Result<PathBuf> {
    let configured = env.substrate_str("backendDir", "");
    if configured.trim().is_empty() {
        return reconciler_assets::materialize(workdir);
    }

    let expanded = expand_tilde(&configured);
    let dir = expanded.canonicalize().with_context(|| {
        format!(
            "substrateConfig.backendDir points at {configured:?}, which does not exist. \
             Leave it unset to use the reconciler built into this dabba binary."
        )
    })?;
    for script in REQUIRED_SCRIPTS {
        if !dir.join(script).is_file() {
            bail!(
                "{} does not look like backends/docker/ (missing {script})",
                dir.display()
            );
        }
    }
    Ok(dir)
}

fn print_up_summary(env_name: &str, gitops_dir: &Path, stacks_dir: &Path, backend_dir: &Path) {
    println!(
        "\n✓ {env_name} reconcile loop installed\n\n  \
         gitops repo:  {gitops}\n  \
         stacks dir:   {stacks}\n  \
         reconciler:   {backend}\n  \
         loop:         {kind} (runs every minute; also kicked off once now)\n\n  \
         status:       dabba env {env_name} status\n  \
         teardown:     dabba env {env_name} down   (stops the loop; leaves stacks running)",
        gitops = gitops_dir.display(),
        stacks = stacks_dir.display(),
        backend = backend_dir.display(),
        kind = loop_kind(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Exposure, Issuer, Substrate};

    /// A resolved docker-host environment with the given substrateConfig.
    fn environment(name: &str, substrate_config: &str) -> ResolvedEnv {
        ResolvedEnv {
            name: name.to_string(),
            substrate: Substrate::DockerHost,
            kubeconfig: None,
            domain: "localtest.me".to_string(),
            issuer: Issuer::Selfsigned,
            exposure: Exposure::Nodeport,
            acme_email: String::new(),
            substrate_config: serde_yaml::from_str(substrate_config).unwrap(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dabba-docker-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn box_name_is_unset_unless_configured() {
        assert_eq!(box_name(&environment("box1", "{}")), None);
        assert_eq!(
            box_name(&environment("box1", "{ boxName: alpha }")),
            Some("alpha".to_string())
        );
        // Whitespace is not a box name; it must fall back to `hostname -s`.
        assert_eq!(box_name(&environment("box1", "{ boxName: '  ' }")), None);
    }

    #[test]
    fn apps_dir_defaults_to_apps() {
        assert_eq!(apps_dir(&environment("box1", "{}")), "apps");
        assert_eq!(
            apps_dir(&environment("box1", "{ appsDir: deployments }")),
            "deployments"
        );
    }

    /// BOX_NAME must be REMOVED rather than inherited when unset: a stray BOX_NAME
    /// in the invoking shell would otherwise point this box at another box's stacks.
    #[test]
    fn unset_box_name_is_removed_from_the_child_environment() {
        let pairs = reconciler_environment(
            &environment("box1", "{}"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
        );
        let box_name = pairs.iter().find(|(k, _)| k == "BOX_NAME").unwrap();
        assert_eq!(box_name.1, None, "BOX_NAME must be removed, not inherited");
    }

    #[test]
    fn reconciler_environment_carries_every_knob_the_loop_reads() {
        let pairs = reconciler_environment(
            &environment("box1", "{ boxName: alpha, appsDir: deployments }"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
        );
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .unwrap_or_else(|| panic!("{key} is not passed to the reconciler"))
                .1
                .clone()
        };
        assert_eq!(get("GITOPS_DIR"), Some("/tmp/gitops".to_string()));
        assert_eq!(get("STACKS_DIR"), Some("/tmp/stacks".to_string()));
        assert_eq!(get("GITOPS_APPS_DIR"), Some("deployments".to_string()));
        assert_eq!(get("BOX_NAME"), Some("alpha".to_string()));
        // Without this the loop installs under the wrong scheduler identity.
        assert_eq!(get("DABBA_ENVIRONMENT"), Some("box1".to_string()));
    }

    /// Two environments must never share a scheduler identity, or the second `up`
    /// replaces the first one's loop and either `down` stops both.
    #[test]
    fn scheduler_identity_is_per_environment() {
        assert_ne!(launchd_label("alpha"), launchd_label("beta"));
        assert_ne!(systemd_timer("alpha"), systemd_timer("beta"));
        assert!(launchd_label("alpha").ends_with(".alpha"));
        assert!(systemd_timer("alpha").starts_with("gitops-reconcile-"));
    }

    /// These strings must match backends/docker/install.sh, which derives the same
    /// names independently. A drift here means `status` cannot find a running loop.
    #[test]
    fn scheduler_identity_matches_the_install_script() {
        let install = include_str!("../../backends/docker/install.sh");
        assert!(
            install.contains(r#"LABEL="io.spicelabs.dabba.reconcile.$DABBA_ENVIRONMENT""#),
            "install.sh no longer derives the launchd label the way launchd_label does"
        );
        assert!(
            install.contains(r#"UNIT_BASE="gitops-reconcile-$DABBA_ENVIRONMENT""#),
            "install.sh no longer derives the unit name the way systemd_timer does"
        );
    }

    #[test]
    fn applied_apps_finds_only_directories_holding_a_compose_file() {
        let dir = scratch("applied-apps");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/docker-compose.yml"), "services: {}").unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("loose-file"), "not a stack").unwrap();

        assert_eq!(applied_apps(&dir), vec!["real".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn applied_apps_is_empty_when_nothing_has_been_applied() {
        assert!(applied_apps(Path::new("/nonexistent/stacks")).is_empty());
    }

    /// With no backendDir configured the reconciler is materialised from the binary
    /// — the path an installed dabba always takes, since it has no checkout.
    #[test]
    fn backend_dir_materialises_the_embedded_reconciler_by_default() {
        let dir = scratch("materialise-default");
        let resolved = docker_backend_dir(&environment("box1", "{}"), &dir).unwrap();
        assert_eq!(resolved, dir.join("reconciler"));
        for script in REQUIRED_SCRIPTS {
            assert!(resolved.join(script).is_file(), "{script} missing");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backend_dir_rejects_a_configured_path_that_is_not_a_reconciler() {
        let dir = scratch("bad-backend-dir");
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let env = environment("box1", &format!("{{ backendDir: {} }}", empty.display()));
        let error = docker_backend_dir(&env, &dir).unwrap_err().to_string();
        assert!(
            error.contains("does not look like backends/docker/"),
            "got: {error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backend_dir_error_points_at_the_embedded_default() {
        let dir = scratch("missing-backend-dir");
        let env = environment("box1", "{ backendDir: /nonexistent/path }");
        let error = docker_backend_dir(&env, &dir).unwrap_err().to_string();
        assert!(error.contains("Leave it unset"), "got: {error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
