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
    let output = run::capture(
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
    .context("running `bao operator init`")?;

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
