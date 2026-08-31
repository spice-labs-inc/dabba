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

use crate::backend::common::{env_workdir, expand_tilde, log, on_path, read_stash, write_stash};
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

/// The compose project OpenBao runs as, matching the reconciler's convention and
/// the example stack under `backends/docker/examples/openbao/`.
const OPENBAO_PROJECT: &str = "gitops-openbao";
/// The root token, stashed in the environment's working directory rather than in
/// OpenBao — for the same reason the Kubernetes path stashes it there: the key to
/// the vault cannot live inside the vault it opens.
const OPENBAO_ROOT_TOKEN: &str = "openbao-root";
const OPENBAO_UNSEAL_KEY: &str = "openbao-unseal";

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
    fn secret_ls(&self, config: &Path, env_name: Option<&str>, path: Option<&str>) -> Result<()> {
        secret_ls(config, env_name, path)
    }
    fn secret_get(&self, config: &Path, env_name: Option<&str>, name: &str) -> Result<()> {
        secret_get(config, env_name, name)
    }
}

/// Secrets dabba keeps OUTSIDE OpenBao, listed under `local/` exactly as the
/// Kubernetes backend lists its own.
const STASH_SECRETS: &[&str] = &[OPENBAO_ROOT_TOKEN, OPENBAO_UNSEAL_KEY];

fn secret_ls(config: &Path, env_name: Option<&str>, path: Option<&str>) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let workdir = env_workdir(config, &env.name)?;

    if path.is_none() || path == Some("local") {
        let present: Vec<&str> = STASH_SECRETS
            .iter()
            .copied()
            .filter(|name| !read_stash(&workdir, name).is_empty())
            .collect();
        if !present.is_empty() {
            println!("local/");
            for name in present {
                println!("    {name}");
            }
        }
        if path == Some("local") {
            return Ok(());
        }
    }

    let token = openbao_token(&workdir, &env.name)?;
    let path = path.unwrap_or("secret");
    if !valid_key_value_path(path) {
        bail!("invalid secret path {path:?}");
    }
    openbao(&token, &format!("bao kv list -format=table {path}"))
}

fn secret_get(config: &Path, env_name: Option<&str>, name: &str) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let workdir = env_workdir(config, &env.name)?;

    if let Some(stash_name) = name.strip_prefix("local/") {
        if !STASH_SECRETS.contains(&stash_name) {
            bail!(
                "unknown local secret {stash_name:?}; known: {}",
                STASH_SECRETS.join(", ")
            );
        }
        let value = read_stash(&workdir, stash_name);
        if value.is_empty() {
            bail!(
                "no stashed {stash_name} for env {:?} (not deployed?)",
                env.name
            );
        }
        println!("{value}");
        return Ok(());
    }

    let token = openbao_token(&workdir, &env.name)?;
    let path = if name.starts_with("secret/") {
        name.to_string()
    } else {
        format!("secret/{name}")
    };
    if !valid_key_value_path(&path) {
        bail!("invalid secret path {name:?}");
    }
    openbao(&token, &format!("bao kv get -format=table {path}"))
}

/// Path segments are restricted so a crafted name cannot break out of the shell
/// command it is interpolated into.
fn valid_key_value_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

fn openbao_token(workdir: &Path, env_name: &str) -> Result<String> {
    let token = read_stash(workdir, OPENBAO_ROOT_TOKEN);
    if token.is_empty() {
        bail!(
            "no OpenBao token for env {env_name:?}. Bring the environment up with the \
             openbao stack in its gitops content (see backends/docker/examples/openbao/)."
        );
    }
    Ok(token)
}

/// The running OpenBao container on this box, or an error naming what is missing.
fn openbao_container() -> Result<String> {
    let output = run::capture(
        "docker",
        &[
            "ps",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={OPENBAO_PROJECT}"),
            "--filter",
            "status=running",
        ],
    )
    .unwrap_or_default();
    let id = output.lines().next().unwrap_or_default().trim().to_string();
    if id.is_empty() {
        bail!(
            "the {OPENBAO_PROJECT} stack is not running on this box. Add the openbao \
             stack to this box's gitops content (see backends/docker/examples/openbao/) \
             and let the reconcile loop converge it."
        );
    }
    Ok(id)
}

/// How long to wait for the reconcile loop to converge the OpenBao stack before
/// giving up. The first tick has to pull the image, so this is deliberately
/// generous: 60 attempts at 5s is five minutes.
const OPENBAO_WAIT_ATTEMPTS: usize = 60;

/// Does this box's gitops content declare an OpenBao stack?
///
/// Checked rather than inferred from whether a container turns up, so a box that
/// simply has no secret store is not punished with a five-minute wait, and a box
/// that DOES declare one gets a real error instead of silently skipping
/// initialisation.
fn openbao_declared(gitops_dir: &Path, apps_dir: &str, box_directory: &str) -> bool {
    let stack = gitops_dir
        .join(apps_dir)
        .join(box_directory)
        .join("openbao");
    stack.join("docker-compose.yml").is_file()
        || stack.join("docker-compose.override.yml").is_file()
}

/// The box's directory name in the gitops repo: the configured `boxName`, else
/// `hostname -s`, matching the reconciler's own default.
fn box_directory(env: &ResolvedEnv) -> String {
    box_name(env).unwrap_or_else(|| {
        run::capture("hostname", &["-s"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    })
}

/// Whether OpenBao has been initialised, and whether it is currently sealed.
struct OpenbaoStatus {
    initialized: bool,
    sealed: bool,
}

/// `bao status`, parsed.
///
/// The exit code carries state rather than failure here — 2 means sealed — so the
/// output is captured regardless of it. JSON is a subset of YAML, so the parser
/// already in the dependency tree reads it without pulling in another one.
fn openbao_status() -> Result<OpenbaoStatus> {
    let container = openbao_container()?;
    let output = run::capture_including_failures(
        "docker",
        &[
            "exec",
            &container,
            "sh",
            "-c",
            "BAO_ADDR=http://127.0.0.1:8200 bao status -format=json",
        ],
    )
    .unwrap_or_default();

    let value: serde_yaml::Value = serde_yaml::from_str(&output)
        .with_context(|| format!("parsing `bao status` output: {output:?}"))?;
    let flag = |key: &str| value.get(key).and_then(|v| v.as_bool());
    Ok(OpenbaoStatus {
        initialized: flag("initialized").unwrap_or(false),
        sealed: flag("sealed").unwrap_or(true),
    })
}

/// Make sure OpenBao is initialised and unsealed, if this box declares one.
///
/// File storage — which the example stack uses, because a dev-mode server loses
/// every secret on restart — means a fresh server comes up SEALED and stays that
/// way until something unseals it. That something is this.
///
/// Idempotent by construction: it initialises only a vault that reports itself
/// uninitialised, so re-running `up` against a vault holding real data unseals it
/// and never re-initialises it. Re-initialising would orphan every existing
/// secret behind a key nobody has.
fn ensure_openbao_ready(env: &ResolvedEnv, workdir: &Path, gitops_dir: &Path) -> Result<()> {
    if !openbao_declared(gitops_dir, &apps_dir(env), &box_directory(env)) {
        return Ok(());
    }

    log(&format!(
        "[{}] waiting for the reconcile loop to bring up OpenBao",
        env.name
    ));
    run::wait_for(
        "the openbao stack to be running (the loop converges it within a minute, \
         plus however long the image pull takes)",
        OPENBAO_WAIT_ATTEMPTS,
        || openbao_container().is_ok(),
    )?;

    // The server may be listening before it can answer; a failed status read is
    // not yet a failure.
    let mut status = openbao_status();
    for _ in 0..12 {
        if status.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        status = openbao_status();
    }
    let status = status?;

    if !status.initialized {
        initialize_openbao(env, workdir)?;
    } else if status.sealed {
        unseal_openbao(env, workdir)?;
    } else {
        log(&format!("[{}] OpenBao is already unsealed", env.name));
    }
    Ok(())
}

/// Initialise a fresh vault and stash what comes back.
///
/// One key share, threshold one. Splitting the key into shares is meaningful when
/// the shares go to different people or systems; here every share would land in
/// the same directory on the same box, which buys no security and adds ways for
/// an unattended restart to fail. This is stated so its absence reads as a
/// decision rather than an oversight.
///
/// The unseal key and root token exist exactly once, in this output. They are
/// written before anything else can fail, because they cannot be recovered.
fn initialize_openbao(env: &ResolvedEnv, workdir: &Path) -> Result<()> {
    log(&format!("[{}] initialising OpenBao (first run)", env.name));
    let container = openbao_container()?;
    let output = run::capture_explaining_failure(
        "docker",
        &[
            "exec",
            &container,
            "sh",
            "-c",
            "BAO_ADDR=http://127.0.0.1:8200 bao operator init \
             -key-shares=1 -key-threshold=1 -format=json",
        ],
    )
    .context("initialising OpenBao")?;

    let value: serde_yaml::Value =
        serde_yaml::from_str(&output).context("parsing `bao operator init` output")?;
    let unseal_key = value
        .get("unseal_keys_b64")
        .and_then(|v| v.as_sequence())
        .and_then(|s| s.first())
        .and_then(|v| v.as_str())
        .context("no unseal key in `bao operator init` output")?;
    let root_token = value
        .get("root_token")
        .and_then(|v| v.as_str())
        .context("no root token in `bao operator init` output")?;

    // Stash before unsealing: if unsealing fails we must still hold the key, or
    // the vault is permanently unopenable.
    write_stash(workdir, OPENBAO_UNSEAL_KEY, unseal_key)?;
    write_stash(workdir, OPENBAO_ROOT_TOKEN, root_token)?;

    unseal_openbao(env, workdir)?;

    // The kv v2 mount the secret verbs and the reconciler both address as
    // `secret/...`. Enabling it here means a freshly initialised box can resolve
    // references immediately rather than on whatever tick someone remembers.
    openbao(
        root_token,
        "bao secrets enable -path=secret kv-v2 2>/dev/null || true",
    )?;

    log(&format!(
        "[{}] OpenBao initialised; unseal key and root token are in {} (0600)",
        env.name,
        workdir.display()
    ));
    Ok(())
}

fn unseal_openbao(env: &ResolvedEnv, workdir: &Path) -> Result<()> {
    let key = read_stash(workdir, OPENBAO_UNSEAL_KEY);
    if key.is_empty() {
        bail!(
            "OpenBao for env {:?} is sealed and dabba has no unseal key for it. \
             The key is written to {} on first initialisation and cannot be \
             recovered from the vault it opens; restore it from your backup.",
            env.name,
            workdir.join(OPENBAO_UNSEAL_KEY).display()
        );
    }
    log(&format!("[{}] unsealing OpenBao", env.name));
    let container = openbao_container()?;
    // The key crosses to the container on stdin and is only then placed in the
    // command line, INSIDE the container.
    //
    // `bao operator unseal -` looks like it should read the key from stdin — that
    // is what `vault operator unseal -` does — but OpenBao 2.x does not implement
    // it, and rejects the empty read with "'key' must be a valid hex or base64
    // string", an error about the key that is really about the plumbing. Verified
    // directly: stdin fails, an argument succeeds.
    //
    // So the key must be an argument, and the question is only whose process list
    // it lands in. Read from stdin by the shell inside the container, it never
    // appears in the host's `ps` — which shows only this `docker exec` line. It is
    // briefly visible to a process inside the OpenBao container itself, which is
    // an acceptable residual: anything able to read that container's process list
    // can already read the vault's storage directly.
    run::run_stdin(
        "docker",
        &[
            "exec",
            "-i",
            &container,
            "sh",
            "-c",
            "read -r KEY; BAO_ADDR=http://127.0.0.1:8200 exec bao operator unseal \"$KEY\"",
        ],
        &format!("{key}\n"),
    )
    .context("unsealing OpenBao")
}

/// Run a `bao` command inside the OpenBao container, token on stdin.
///
/// The token never appears in argv, which `ps` and /proc expose to every process on
/// the box — the same reasoning as the Kubernetes backend's equivalent.
fn openbao(token: &str, command: &str) -> Result<()> {
    let container = openbao_container()?;
    let script =
        format!("read -r BAO_TOKEN; export BAO_TOKEN BAO_ADDR=http://127.0.0.1:8200; {command}");
    run::run_stdin(
        "docker",
        &["exec", "-i", &container, "sh", "-c", &script],
        &format!("{token}\n"),
    )
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
        &reconciler_environment(&env, &gitops_dir, &stacks_dir, &workdir),
    )?;

    // The loop is installed; OpenBao, if this box has one, still needs to be
    // initialised and unsealed before any stack's secret references can resolve.
    ensure_openbao_ready(&env, &workdir, &gitops_dir)?;

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
    let backups = backups_dir(&workdir);

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
        match latest_backup(&backups, &app) {
            Some((archive, kept)) => println!(
                "      last backup:  {}  ({kept} kept)",
                backup_stamp(&archive).unwrap_or(archive)
            ),
            None => println!("      last backup:  none"),
        }
    }
    Ok(())
}

/// Where this environment keeps its archives.
///
/// One function, because `status` reads this directory and the reconciler is told
/// about it through `BACKUPS_DIR` — two places deriving the same path independently
/// is how the scheduler identity went wrong before it was fixed.
fn backups_dir(workdir: &Path) -> PathBuf {
    workdir.join("backups")
}

/// The newest archive for an application, and how many are kept.
///
/// backup.sh names them `<app>-<UTC stamp>.tar.gz` with a sortable stamp, so the
/// newest is the last by name. Reported because a backup you cannot see the age of
/// is one you are trusting rather than checking: the alert says when a run FAILED,
/// and this says whether one ever succeeded.
fn latest_backup(backups: &Path, app: &str) -> Option<(String, usize)> {
    let entries = std::fs::read_dir(backups.join(app)).ok()?;
    let mut archives: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.starts_with(&format!("{app}-")) && name.ends_with(".tar.gz")).then_some(name)
        })
        .collect();
    archives.sort();
    let newest = archives.last()?.clone();
    Some((newest, archives.len()))
}

/// The UTC stamp out of an archive name, as it was written — no timezone guessing,
/// because this runs on machines in places this code does not know about.
fn backup_stamp(archive: &str) -> Option<String> {
    let stamp = archive.strip_suffix(".tar.gz")?.rsplit_once('-')?.1;
    // YYYYMMDDTHHMMSSZ
    if stamp.len() != 16 || !stamp.ends_with('Z') || !stamp.contains('T') {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}:{}:{} UTC",
        &stamp[0..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[9..11],
        &stamp[11..13],
        &stamp[13..15]
    ))
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
    workdir: &Path,
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
        // Beside the stacks, and per environment for the same reason the scheduler
        // identity is: two environments on one box with an app of the same name
        // would otherwise write archives over each other.
        (
            "BACKUPS_DIR".to_string(),
            Some(backups_dir(workdir).to_string_lossy().into_owned()),
        ),
        (
            "GITOPS_BRANCH".to_string(),
            Some(env.substrate_str("gitopsBranch", "main")),
        ),
        ("BOX_NAME".to_string(), box_name(env)),
        ("DABBA_ENVIRONMENT".to_string(), Some(env.name.clone())),
        (
            "OPENBAO_PROJECT".to_string(),
            Some(OPENBAO_PROJECT.to_string()),
        ),
        // Where the reconciler reads the token from when resolving x-secrets. The
        // token travels as a path, never as a value in the environment.
        (
            "OPENBAO_TOKEN_FILE".to_string(),
            Some(
                workdir
                    .join(OPENBAO_ROOT_TOKEN)
                    .to_string_lossy()
                    .into_owned(),
            ),
        ),
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
    use crate::backend::common::scratch::ScratchDirectory;
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
            Path::new("/tmp/workdir"),
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
            Path::new("/tmp/workdir"),
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
        // The reconciler resolves x-secrets by reading the token from this path.
        // The token itself must never travel through the environment, where the
        // child's /proc would expose it.
        assert_eq!(
            get("OPENBAO_TOKEN_FILE"),
            Some("/tmp/workdir/openbao-root".to_string())
        );
        assert!(
            !pairs.iter().any(|(_, value)| value
                .as_deref()
                .is_some_and(|v| v.len() > 20 && !v.contains('/'))),
            "a secret-looking value is being passed through the environment"
        );
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

    /// Every variable the reconciler is configured with must survive into the
    /// SCHEDULED ticks, not just the install-time pass.
    ///
    /// This has now gone wrong twice. `appsDir` was honoured by install.sh and
    /// dropped by the unit templates, so the knob appeared to work and did
    /// nothing. Then OPENBAO_TOKEN_FILE was added to the child environment and not
    /// to the templates, so every tick after the first reported that no token was
    /// available. install.sh already fails on an unrendered token, but nothing
    /// caught a variable that was never put in a template at all — which is this.
    #[test]
    fn every_configured_variable_survives_into_the_scheduled_ticks() {
        let launchd = include_str!(
            "../../backends/docker/launchd/io.spicelabs.dabba.reconcile.plist.template"
        );
        let systemd =
            include_str!("../../backends/docker/systemd/gitops-reconcile.service.template");

        let pairs = reconciler_environment(
            &environment("box1", "{}"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
            Path::new("/tmp/workdir"),
        );

        for (key, _) in &pairs {
            // Match the ASSIGNMENT, not the name anywhere in the file. A plain
            // substring check passes on a variable that survives only in a
            // comment, which reaches no child process — exactly the failure this
            // test exists to catch.
            assert!(
                launchd.contains(&format!("<key>{key}</key>")),
                "{key} is passed to install.sh but is not set in the launchd \
                 template EnvironmentVariables, so the scheduled ticks would \
                 never see it"
            );
            assert!(
                systemd.contains(&format!("Environment={key}=")),
                "{key} is passed to install.sh but has no Environment= line in \
                 the systemd template, so the scheduled ticks would never see it"
            );
        }
    }

    /// The inverse of the test above, and the one that was missing.
    ///
    /// That test checks every variable dabba PASSES reaches the templates. It
    /// cannot catch the opposite: a variable the reconciler READS that dabba never
    /// passes. `gitopsBranch` was documented as a config knob in
    /// examples/docker-host.yaml, honoured by reconcile.sh, and never plumbed —
    /// so setting it did nothing, exactly like appsDir before it.
    ///
    /// The list is derived from reconcile.sh's own `VAR="${VAR:-default}"` lines
    /// rather than hand-maintained, because a hand-maintained list is the thing
    /// that keeps going wrong here.
    #[test]
    fn every_variable_the_reconciler_reads_is_passed_to_it() {
        let reconciler = include_str!("../../backends/docker/reconcile.sh");

        // Ambient environment, supplied by the shell or the scheduler rather than
        // by dabba's configuration.
        const AMBIENT: &[&str] = &["HOME", "USER", "TMPDIR", "PATH", "XDG_RUNTIME_DIR"];

        let mut reads: Vec<String> = Vec::new();
        for line in reconciler.lines() {
            let line = line.trim();
            // Matches: NAME="${NAME:-default}"
            let Some((name, rest)) = line.split_once("=\"${") else {
                continue;
            };
            if !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') || name.is_empty() {
                continue;
            }
            // The name must be the WHOLE variable being read, not a prefix of it.
            // `BOX="${BOX_NAME:-...}"` assigns a local from a different variable,
            // and a prefix match reported the local as unpassed configuration.
            let Some(after) = rest.strip_prefix(name) else {
                continue;
            };
            if !after.starts_with(":-") && !after.starts_with('}') {
                continue;
            }
            if AMBIENT.contains(&name) || reads.iter().any(|r| r == name) {
                continue;
            }
            reads.push(name.to_string());
        }

        assert!(
            reads.len() >= 5,
            "parsed only {} configuration variables out of reconcile.sh; the parser \
             broke and this test would pass vacuously",
            reads.len()
        );

        let passed: Vec<String> = reconciler_environment(
            &environment("box1", "{}"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
            Path::new("/tmp/workdir"),
        )
        .into_iter()
        .map(|(key, _)| key)
        .collect();

        for name in reads {
            assert!(
                passed.contains(&name),
                "reconcile.sh reads {name} but dabba never passes it, so any config \
                 knob behind it silently does nothing"
            );
        }
    }

    #[test]
    fn the_tracked_branch_is_configurable() {
        let pairs = reconciler_environment(
            &environment("box1", "{ gitopsBranch: release }"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
            Path::new("/tmp/workdir"),
        );
        let branch = pairs.iter().find(|(k, _)| k == "GITOPS_BRANCH").unwrap();
        assert_eq!(branch.1, Some("release".to_string()));
    }

    /// Both schedulers must substitute the SAME tokens.
    ///
    /// They did not: launchd plists were rendered and systemd units were copied
    /// verbatim, on the reasoning that systemd expands `%h` itself. But `%h` cannot
    /// name the directory dabba materialises its scripts into, so a unit that wanted
    /// to run the shipped `backup.sh` had to hard-code an absolute path containing
    /// the environment name — while the macOS half of the same feature did not.
    /// A capability present on one platform and absent on the other is the shape of
    /// bug this backend keeps producing, so it is asserted rather than remembered.
    #[test]
    fn both_schedulers_substitute_the_same_tokens() {
        let reconciler = include_str!("../../backends/docker/reconcile.sh");

        /// The `__TOKEN__` names a `sed -e "s|__X__|...|g"` line substitutes, within
        /// one function of the script.
        fn tokens_substituted_by(reconciler: &str, function: &str) -> Vec<String> {
            let body = reconciler
                .split_once(&format!("{function}() {{"))
                .unwrap_or_else(|| panic!("{function} is defined in reconcile.sh"))
                .1
                .split_once("\n}\n")
                .expect("the function is closed")
                .0;
            let mut tokens: Vec<String> = body
                .lines()
                .filter_map(|line| {
                    let start = line.find("s|__")? + 2;
                    let rest = &line[start..];
                    let end = rest.find("__|")? + 2;
                    Some(rest[..end].to_string())
                })
                .collect();
            tokens.sort();
            tokens.dedup();
            tokens
        }

        let systemd = tokens_substituted_by(reconciler, "sync_systemd_units");
        let launchd = tokens_substituted_by(reconciler, "sync_launchd_agents");

        assert!(
            systemd.len() >= 4,
            "parsed only {systemd:?} out of sync_systemd_units; the parser broke and \
             this test would pass vacuously"
        );
        assert_eq!(
            systemd, launchd,
            "the two schedulers substitute different tokens, so a job unit that works \
             on one host silently does not on the other"
        );

        // And every token the shipped examples use must be one of them, or the
        // example installs a unit with a literal __TOKEN__ in its ExecStart.
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("backends/docker/examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&directory).expect("the examples directory") {
            let path = entry.expect("an examples entry").path();
            if !path.is_file() {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("reading the example");
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            for line in body.lines() {
                // Skip the prose: the header comments name the tokens to explain them.
                if line.trim_start().starts_with('#') || line.trim_start().starts_with("<!--") {
                    continue;
                }
                let mut rest = line;
                while let Some(start) = rest.find("__") {
                    let after = &rest[start + 2..];
                    let Some(end) = after.find("__") else { break };
                    let token = format!("__{}__", &after[..end]);
                    assert!(
                        systemd.contains(&token),
                        "{name} uses {token}, which the reconciler never substitutes — \
                         it would install a unit with that text left in it"
                    );
                    checked += 1;
                    rest = &after[end + 2..];
                }
            }
        }
        assert!(
            checked > 0,
            "no tokens were found in any shipped example, so this half of the test \
             would pass vacuously"
        );
    }

    /// A shipped job unit must pass every location the script it runs needs.
    ///
    /// `backup.sh` falls back to `$HOME/stacks` and `$HOME/backups`, which is
    /// nowhere near where a dabba-managed environment keeps anything — stacks live
    /// under the per-environment working directory. The shipped units invoked that
    /// script without setting either, so following the example produced a backup
    /// job that could never find the stack it was meant to archive. The test suite
    /// missed it because it calls the script directly with the variables already
    /// set, which is exactly the shape of a test that cannot see this.
    #[test]
    fn a_shipped_job_unit_passes_every_location_its_script_needs() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("backends/docker/examples");
        // Variables the script reads to LOCATE things, as opposed to tuning knobs
        // that have a sensible default (BACKUP_KEEP, BACKUP_QUIESCE).
        const LOCATIONS: &[&str] = &["STACKS_DIR", "BACKUPS_DIR"];

        let mut checked = 0;
        for entry in std::fs::read_dir(&directory).expect("the examples directory") {
            let path = entry.expect("an examples entry").path();
            if !path.is_file() {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("reading the example");
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // Only units that actually invoke one of the shipped scripts.
            if !body.contains("backup.sh") && !body.contains("restore.sh") {
                continue;
            }
            for variable in LOCATIONS {
                assert!(
                    body.contains(variable),
                    "{name} runs a shipped script but never sets {variable}, so the job \
                     would look under $HOME instead of this environment"
                );
            }
            checked += 1;
        }
        assert!(
            checked >= 2,
            "expected a systemd and a launchd example invoking a shipped script, found \
             {checked} — this test would pass vacuously"
        );
    }

    /// The reconciler never destroys data. That is the promise this backend is
    /// built on — `down` leaves stacks running, removing an app from git stops
    /// managing it rather than deleting it — and until now it was written in three
    /// comments and enforced nowhere.
    ///
    /// A loop that runs unattended every minute is the worst possible place to
    /// discover that someone added a teardown to an error path. restore.sh is the
    /// one script allowed to destroy, deliberately and by hand, so it is not
    /// covered here.
    #[test]
    fn the_reconciler_contains_nothing_that_destroys_data() {
        let reconciler = include_str!("../../backends/docker/reconcile.sh");

        // Each entry is a command that removes something a person would miss.
        const DESTRUCTIVE: &[(&str, &str)] = &[
            (
                "compose down",
                "tears a stack down, taking its containers with it",
            ),
            ("volume rm", "deletes a named volume and everything in it"),
            ("volume prune", "deletes every unused volume on the box"),
            (
                "system prune",
                "deletes across the whole daemon, not just this stack",
            ),
            ("image prune", "deletes images other projects may be using"),
            (
                "rm -rf",
                "removes a tree, and the trees here hold application data",
            ),
            (
                "docker rm",
                "removes containers this loop is supposed to converge",
            ),
        ];

        for line in reconciler.lines() {
            let code = line.trim();
            // Prose explaining what it does NOT do is the whole point of the file.
            if code.starts_with('#') {
                continue;
            }
            for (command, why) in DESTRUCTIVE {
                assert!(
                    !code.contains(command),
                    "reconcile.sh contains `{command}`, which {why}. The reconciler never \
                     destroys: removing an app from git stops managing it, and tearing it \
                     down stays a deliberate act. If this is genuinely needed, it belongs \
                     in restore.sh or in a person's hands, not in a loop that runs every \
                     minute unattended.\n  offending line: {code}"
                );
            }
        }

        // The promise is only worth testing if the file is the one we think it is.
        assert!(
            reconciler.contains("docker compose pull") && reconciler.contains("up -d"),
            "reconcile.sh no longer looks like the reconciler; this check is aimed at \
             the wrong file"
        );
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

    /// The unit a job names in `OnFailure=` is rendered by the reconciler; the unit
    /// that actually exists is rendered by install.sh; and uninstall.sh has to
    /// remove that same one. Three files deriving one name independently is the
    /// arrangement that produced every naming bug in this backend so far.
    #[test]
    fn the_job_alert_unit_is_named_the_same_way_everywhere() {
        let reconciler = include_str!("../../backends/docker/reconcile.sh");
        let install = include_str!("../../backends/docker/install.sh");
        let uninstall = include_str!("../../backends/docker/uninstall.sh");

        assert!(
            reconciler.contains(
                r#"JOB_ALERT_UNIT="gitops-reconcile-${DABBA_ENVIRONMENT:-default}-job-alert@""#
            ),
            "reconcile.sh no longer derives the job alert unit the expected way"
        );
        assert!(
            install.contains(r#"JOB_ALERT="$UNIT_BASE-job-alert@""#),
            "install.sh no longer derives the job alert unit the expected way"
        );
        assert!(
            uninstall.contains(r#"JOB_ALERT="$UNIT_BASE-job-alert@""#),
            "uninstall.sh no longer derives the job alert unit the expected way"
        );
        // UNIT_BASE is gitops-reconcile-$DABBA_ENVIRONMENT in both scripts, so the
        // two spellings above agree. Assert that rather than assuming it.
        assert!(
            install.contains(r#"UNIT_BASE="gitops-reconcile-$DABBA_ENVIRONMENT""#)
                && uninstall.contains(r#"UNIT_BASE="gitops-reconcile-$DABBA_ENVIRONMENT""#),
            "UNIT_BASE changed, so the job alert unit names no longer agree"
        );

        // And it must actually be installed and removed, not merely named.
        assert!(
            install.contains(r#"render "$BACKEND_DIR/systemd/dabba-job-alert@.service.template""#),
            "install.sh no longer installs the job alert unit"
        );
        assert!(
            uninstall.contains(r#""$UNIT_DIR/$JOB_ALERT.service""#),
            "uninstall.sh no longer removes the job alert unit, so teardown leaves it behind"
        );
    }

    /// The reconciler boots LaunchAgents in and out by `gui/<uid>/<label>`, and it
    /// derives that label from the FILENAME. A plist whose declared `Label`
    /// disagrees installs once and can then never be updated or removed, so it
    /// outlives its own deletion from git.
    ///
    /// The shipped example is what people copy, so it is the one file that must
    /// not get this wrong. It did: it was named `example-cron.plist` while
    /// declaring `io.spicelabs.dabba.cron.example-app.nightly-backup`, and the
    /// macOS cron test could not catch it because that test generates its fixture
    /// with the filename and the Label from the same variable.
    #[test]
    fn shipped_launchd_examples_declare_a_label_matching_their_filename() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("backends/docker/examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("reading the examples directory") {
            let path = entry.expect("an examples directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("a plist filename")
                .to_string();
            let body = std::fs::read_to_string(&path).expect("reading the plist");
            let declared = label_in_plist(&body)
                .unwrap_or_else(|| panic!("{stem}.plist declares no Label at all"));
            assert_eq!(
                declared, stem,
                "{stem}.plist declares Label {declared:?}; the reconciler manages \
                 agents by filename, so an example that disagrees teaches people to \
                 write an agent it can never remove"
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no example plists were found, so this test would pass vacuously"
        );
    }

    /// The `<string>` following `<key>Label</key>`, mirroring what the reconciler's
    /// sed does at runtime.
    fn label_in_plist(body: &str) -> Option<String> {
        let after = body.split_once("<key>Label</key>")?.1;
        let open = after.find("<string>")? + "<string>".len();
        let close = after[open..].find("</string>")? + open;
        Some(after[open..close].trim().to_string())
    }

    #[test]
    fn applied_apps_finds_only_directories_holding_a_compose_file() {
        let guard = ScratchDirectory::new("docker-applied-apps");
        let dir = guard.path().to_path_buf();
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/docker-compose.yml"), "services: {}").unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("loose-file"), "not a stack").unwrap();

        assert_eq!(applied_apps(&dir), vec!["real".to_string()]);
    }

    #[test]
    fn applied_apps_is_empty_when_nothing_has_been_applied() {
        assert!(applied_apps(Path::new("/nonexistent/stacks")).is_empty());
    }

    /// The alert says when a backup RUN failed. This says whether one ever
    /// succeeded, which is the question you actually have about a backup.
    #[test]
    fn status_reports_the_newest_archive_and_how_many_are_kept() {
        let guard = ScratchDirectory::new("docker-backup-status");
        let backups = guard.path().join("app");
        std::fs::create_dir_all(&backups).unwrap();

        assert_eq!(
            latest_backup(guard.path(), "app"),
            None,
            "an empty directory has no backups"
        );

        // Deliberately created out of order: the stamp decides which is newest, not
        // the filesystem.
        for stamp in ["20260826T033000Z", "20260824T033000Z", "20260825T033000Z"] {
            std::fs::write(backups.join(format!("app-{stamp}.tar.gz")), "x").unwrap();
        }
        // Something that is not an archive must not be counted as one.
        std::fs::write(backups.join("app-20260826T033000Z.tar.gz.partial"), "x").unwrap();
        std::fs::write(backups.join("notes.txt"), "x").unwrap();

        let (newest, kept) = latest_backup(guard.path(), "app").expect("archives were found");
        assert_eq!(newest, "app-20260826T033000Z.tar.gz");
        assert_eq!(kept, 3, "a partial and a stray file are not backups");
    }

    /// Rendered as the stamp was written. No timezone is guessed: this runs on
    /// machines in places this code knows nothing about.
    #[test]
    fn a_backup_stamp_renders_as_written() {
        assert_eq!(
            backup_stamp("app-20260826T033000Z.tar.gz").as_deref(),
            Some("2026-08-26 03:30:00 UTC")
        );
        assert_eq!(backup_stamp("app-not-a-stamp.tar.gz"), None);
        assert_eq!(backup_stamp("app-20260826T0330Z.tar.gz"), None);
    }

    /// `status` reads the archives and the reconciler is told where to write them.
    /// Two derivations of one path is how the scheduler identity went wrong.
    #[test]
    fn the_backups_directory_has_one_derivation() {
        let workdir = Path::new("/tmp/env");
        assert_eq!(backups_dir(workdir), workdir.join("backups"));
        let passed = reconciler_environment(
            &environment("box1", "{}"),
            Path::new("/tmp/gitops"),
            Path::new("/tmp/stacks"),
            workdir,
        );
        let value = passed
            .iter()
            .find(|(key, _)| key == "BACKUPS_DIR")
            .and_then(|(_, value)| value.clone())
            .expect("BACKUPS_DIR is passed to the reconciler");
        assert_eq!(value, backups_dir(workdir).to_string_lossy());
    }

    /// With no backendDir configured the reconciler is materialised from the binary
    /// — the path an installed dabba always takes, since it has no checkout.
    #[test]
    fn backend_dir_materialises_the_embedded_reconciler_by_default() {
        let guard = ScratchDirectory::new("docker-materialise-default");
        let dir = guard.path().to_path_buf();
        let resolved = docker_backend_dir(&environment("box1", "{}"), &dir).unwrap();
        assert_eq!(resolved, dir.join("reconciler"));
        for script in REQUIRED_SCRIPTS {
            assert!(resolved.join(script).is_file(), "{script} missing");
        }
    }

    #[test]
    fn backend_dir_rejects_a_configured_path_that_is_not_a_reconciler() {
        let guard = ScratchDirectory::new("docker-bad-backend-dir");
        let dir = guard.path().to_path_buf();
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let env = environment("box1", &format!("{{ backendDir: {} }}", empty.display()));
        let error = docker_backend_dir(&env, &dir).unwrap_err().to_string();
        assert!(
            error.contains("does not look like backends/docker/"),
            "got: {error}"
        );
    }

    /// The path is interpolated into a shell command inside the container, so a
    /// crafted name must be rejected before it gets there rather than escaped.
    #[test]
    fn secret_paths_reject_anything_that_could_escape_a_shell_command() {
        for good in ["secret", "secret/demo", "secret/demo/podinfo", "a-b_c.d"] {
            assert!(valid_key_value_path(good), "{good} should be valid");
        }
        for bad in [
            "",
            "secret/",
            "/secret",
            "secret//demo",
            "demo; rm -rf /",
            "demo$(whoami)",
            "demo`id`",
            "demo && echo",
            "demo\nkv list",
            "demo'",
        ] {
            assert!(!valid_key_value_path(bad), "{bad:?} should be rejected");
        }
    }

    /// `local/` names the stash that deliberately lives outside OpenBao. Anything
    /// not in it must be rejected rather than read from an arbitrary file.
    #[test]
    fn only_known_names_resolve_under_local() {
        assert!(STASH_SECRETS.contains(&OPENBAO_ROOT_TOKEN));
        assert!(STASH_SECRETS.contains(&OPENBAO_UNSEAL_KEY));
        assert!(!STASH_SECRETS.contains(&"../../etc/passwd"));
    }

    #[test]
    fn backend_dir_error_points_at_the_embedded_default() {
        let guard = ScratchDirectory::new("docker-missing-backend-dir");
        let dir = guard.path().to_path_buf();
        let env = environment("box1", "{ backendDir: /nonexistent/path }");
        let error = docker_backend_dir(&env, &dir).unwrap_err().to_string();
        assert!(error.contains("Leave it unset"), "got: {error}");
    }
}
