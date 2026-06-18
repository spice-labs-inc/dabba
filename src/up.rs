//! `dabba up`/`down`/`status`/`kubeconfig` — the per-environment lifecycle. `up` is
//! the day-0 bootstrap: provision the substrate, install Forgejo + the Flux Operator,
//! seed the gitops content into Forgejo, seed the demo secret into OpenBao, and wait
//! for the platform to settle. Each env runs in its own `.dabba/<env>/` working dir.

use crate::config::{DabbaConfig, Issuer, ResolvedEnv, Substrate};
use crate::run;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Options {
    pub config: PathBuf,
    /// Which environment to act on; None → the config's default.
    pub env: Option<String>,
    pub quickstart_dir: PathBuf,
    /// Override module sources: a local path (dev) → `<path>/modules/<substrate>`;
    /// None → the public git ref.
    pub modules_source: Option<String>,
    /// Local gitops content to seed Forgejo from (air-gapped/dev). None → clone the
    /// config's git.upstream.
    pub gitops_seed: Option<PathBuf>,
}

const FORGEJO_USER: &str = "dabba";
const DEMO_MESSAGE: &str = "Hello from OpenBao — delivered through dabba";
const WAIT_ATTEMPTS: usize = 120;

pub fn run(opts: &Options) -> Result<()> {
    let cfg = DabbaConfig::load(&opts.config)?;
    let env = cfg.resolve(opts.env.as_deref())?;
    // Per-env working copy: state + kubeconfig live here, isolated from other envs.
    let workdir = env_workdir(&opts.config, &env.name)?;
    preflight(&env, &workdir)?;
    isolate_helm_registry(&workdir)?;
    // Secret-zero: per-env random credentials, generated once and stashed locally.
    // The OpenBao root token is the irreducible local secret (it's the key to OpenBao
    // itself, so it can't live inside it); the Forgejo password is needed before
    // OpenBao is up, so it's stashed too and mirrored into OpenBao once it's ready.
    let openbao_root = env_secret(&workdir, "openbao-root", false)?;
    let forgejo_pw = env_secret(&workdir, "forgejo-password", false)?;
    let openobserve_pw = env_secret(&workdir, "openobserve-password", true)?;

    let template = opts
        .quickstart_dir
        .canonicalize()
        .with_context(|| format!("quickstart dir {}", opts.quickstart_dir.display()))?;
    let c01 = workdir.join("01-cluster");
    let c02 = workdir.join("02-bootstrap");
    copy_stage(&template.join("02-bootstrap"), &c02)?;

    // Determine the kubeconfig: provision a cluster, or use a bring-your-own one.
    let kubeconfig = if env.substrate == Substrate::Existing {
        let kc = expand_tilde(env.kubeconfig.as_deref().unwrap_or_default());
        let kc = kc
            .canonicalize()
            .with_context(|| format!("kubeconfig {}", kc.display()))?;
        log(&format!("[{}] using the provided kubeconfig", env.name));
        kc
    } else {
        copy_stage(&template.join("01-cluster"), &c01)?;
        let substrate = substrate_dir(env.substrate)?;
        // Substrate selection (module source isn't a TF variable, so rewrite it).
        let source = match &opts.modules_source {
            Some(p) => format!("{p}/modules/{substrate}"),
            None => format!(
                "git::https://github.com/spice-labs-inc/dabba-modules.git//modules/{substrate}?ref=main"
            ),
        };
        rewrite_module_source(&c01.join("main.tf"), &source)?;
        log(&format!("[{}] creating the {substrate} cluster", env.name));
        let c01arg = format!("-chdir={}", c01.display());
        run::run_quiet("tofu", &[&c01arg, "init", "-input=false"])?;
        run::run(
            "tofu",
            &[
                &c01arg,
                "apply",
                "-auto-approve",
                "-input=false",
                &format!("-var=cluster_name={}", env.name),
            ],
        )?;
        workdir.join("kubeconfig") // 01-cluster writes ../kubeconfig = workdir/kubeconfig
    };

    // export KUBECONFIG so every later kubectl/tofu inherits it.
    std::env::set_var("KUBECONFIG", &kubeconfig);

    // 02-bootstrap runs the same regardless of substrate. Localize its module
    // sources for local dev.
    if let Some(local) = &opts.modules_source {
        localize_module_sources(&c02.join("main.tf"), local)?;
    }
    log(&format!(
        "[{}] installing Forgejo and the Flux Operator",
        env.name
    ));
    let c02arg = format!("-chdir={}", c02.display());
    run::run_quiet("tofu", &[&c02arg, "init", "-input=false"])?;
    let vars = bootstrap_vars(&env, &kubeconfig, &openbao_root, &forgejo_pw);
    let mut args = vec![c02arg.as_str(), "apply", "-auto-approve", "-input=false"];
    args.extend(vars.iter().map(String::as_str));
    run::run("tofu", &args)?;

    seed_forgejo(&cfg, opts, &forgejo_pw)?;
    seed_openbao(&openbao_root, &forgejo_pw, &openobserve_pw)?;
    wait_for_reconciled(WAIT_ATTEMPTS)?;
    print_summary(&env, &kubeconfig);
    Ok(())
}

/// `<config dir>/.dabba/<env>` — the per-env working dir (tofu state + kubeconfig).
fn env_workdir(config: &Path, env_name: &str) -> Result<PathBuf> {
    let base = config
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join(".dabba").join(env_name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Copy a quickstart stage template into the per-env workdir, overwriting the .tf
/// but leaving any existing state/.terraform in place.
fn copy_stage(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    run::run_quiet(
        "cp",
        &[
            "-r",
            &format!("{}/.", src.display()),
            &dest.display().to_string(),
        ],
    )
}

pub struct DownOptions {
    pub config: PathBuf,
    pub env: Option<String>,
}

/// `dabba down` — the inverse of `up`. For a provisioned substrate, destroy the
/// cluster (which takes everything in it). For `existing` (BYO), leave the cluster
/// alone and only uninstall what we installed.
pub fn down(opts: &DownOptions) -> Result<()> {
    let cfg = DabbaConfig::load(&opts.config)?;
    let env = cfg.resolve(opts.env.as_deref())?;
    let workdir = env_workdir(&opts.config, &env.name)?;
    let c01 = workdir.join("01-cluster");
    let c02 = workdir.join("02-bootstrap");

    if env.substrate == Substrate::Existing {
        // BYO cluster: don't destroy it — only tear down the platform we installed.
        let kc = expand_tilde(env.kubeconfig.as_deref().unwrap_or_default());
        let kc = kc.canonicalize().unwrap_or(kc);
        std::env::set_var("KUBECONFIG", &kc);
        if c02.join(".terraform").is_dir() {
            log(&format!(
                "[{}] uninstalling the platform (leaving the existing cluster in place)",
                env.name
            ));
            let c02arg = format!("-chdir={}", c02.display());
            let openbao_root = read_stash(&workdir, "openbao-root");
            let forgejo_pw = read_stash(&workdir, "forgejo-password");
            let vars = bootstrap_vars(&env, &kc, &openbao_root, &forgejo_pw);
            let mut args = vec![c02arg.as_str(), "destroy", "-auto-approve", "-input=false"];
            args.extend(vars.iter().map(String::as_str));
            run::try_run("tofu", &args);
        }
    } else if c01.join(".terraform").is_dir() {
        // Destroying the cluster removes everything in it.
        log(&format!("[{}] destroying the cluster", env.name));
        let c01arg = format!("-chdir={}", c01.display());
        run::try_run(
            "tofu",
            &[
                &c01arg,
                "destroy",
                "-auto-approve",
                "-input=false",
                &format!("-var=cluster_name={}", env.name),
            ],
        );
    }

    // Drop the whole per-env workdir (state, working copies, kubeconfig).
    let _ = std::fs::remove_dir_all(&workdir);
    println!("✓ {} torn down", env.name);
    Ok(())
}

/// The -var args 02-bootstrap needs — shared by `up` (apply) and `down` (destroy).
/// For one-cluster-per-env, `cluster` and `environment` are both the env name.
fn bootstrap_vars(
    env: &ResolvedEnv,
    kubeconfig: &Path,
    openbao_root: &str,
    forgejo_pw: &str,
) -> Vec<String> {
    vec![
        format!("-var=kubeconfig_path={}", kubeconfig.display()),
        format!("-var=domain={}", env.domain),
        format!("-var=cluster_issuer={}", issuer_name(env.issuer)),
        format!("-var=cluster_name={}", env.name),
        format!("-var=environment={}", env.name),
        format!("-var=openbao_root_token={openbao_root}"),
        format!("-var=forgejo_admin_password={forgejo_pw}"),
    ]
}

/// Read-or-generate a per-env secret stashed in the workdir (0600). Reused across
/// re-ups so the value is stable for the life of the env. `complex` adds a fixed
/// upper/digit/special suffix to satisfy app password policies (e.g. OpenObserve).
fn env_secret(workdir: &Path, name: &str, complex: bool) -> Result<String> {
    let path = workdir.join(name);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    // The entropy is in the hex; the suffix only satisfies complexity policies.
    let val = if complex {
        format!("{}Aa1!", random_token(24)?)
    } else {
        random_token(24)?
    };
    std::fs::write(&path, &val).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(val)
}

/// Read a stashed per-env secret (empty string if absent) — for `down`, which must
/// not generate.
fn read_stash(workdir: &Path, name: &str) -> String {
    std::fs::read_to_string(workdir.join(name))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `nbytes` of OS randomness as a lowercase hex string. Bails if `/dev/urandom`
/// can't be read — a silent all-zeros token would be a catastrophic secret.
fn random_token(nbytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; nbytes];
    let mut f = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    f.read_exact(&mut buf).context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// `dabba status` — for the given env: is it deployed, and how is Flux reconciling?
pub fn status(config: &Path, env_name: Option<&str>) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    println!(
        "environment: {}  (substrate: {:?}, domain: {})",
        env.name, env.substrate, env.domain
    );

    let Some(kc) = env_kubeconfig(config, &env)? else {
        println!(
            "  status:      not deployed (run `dabba env {} up`)",
            env.name
        );
        return Ok(());
    };
    std::env::set_var("KUBECONFIG", &kc);
    if !run::probe("kubectl", &["get", "--raw", "/readyz"]) {
        println!("  status:      ✗ cluster unreachable ({})", kc.display());
        return Ok(());
    }

    // Live Flux state.
    let ksts = flux_ready("kustomizations");
    let hrs = helm_releases();
    let k_ready = ksts.iter().filter(|(_, r)| *r).count();
    let h_ready = hrs.iter().filter(|(_, r, _)| *r).count();
    let not_ready: Vec<String> = ksts
        .iter()
        .filter(|(_, r)| !r)
        .map(|(n, _)| n.clone())
        .chain(hrs.iter().filter(|(_, r, _)| !r).map(|(n, _, _)| n.clone()))
        .collect();
    let verdict = if ksts.is_empty() {
        "settling (no Kustomizations yet)".to_string()
    } else if not_ready.is_empty() {
        "✓ reconciled".to_string()
    } else {
        format!("✗ degraded — {} not ready", not_ready.len())
    };
    println!("  status:      {verdict}");
    println!(
        "  flux:        {k_ready}/{} kustomizations, {h_ready}/{} helmreleases ready",
        ksts.len(),
        hrs.len()
    );
    if !not_ready.is_empty() {
        println!("  not ready:   {}", not_ready.join(", "));
    }

    // Declared (from the config) — the config-aware half.
    println!("\n  declared (dabba.yaml):");
    let uc = if cfg.spec.use_cases.is_empty() {
        "(none)".to_string()
    } else {
        cfg.spec.use_cases.join(", ")
    };
    println!("    use-cases:      {uc}");
    println!(
        "    observability:  {}",
        if cfg.spec.observability.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("    tls issuer:     {:?}", cfg.spec.tls.issuer);
    println!("    secrets:        {:?}", cfg.spec.secrets.backend);

    // Live components + versions.
    if !ksts.is_empty() {
        println!("\n  components:");
        for (n, r) in &ksts {
            println!("    {} {n}", mark(*r));
        }
    }
    if !hrs.is_empty() {
        println!("\n  releases:");
        for (n, r, v) in &hrs {
            println!("    {} {n:<24} {v}", mark(*r));
        }
    }

    println!("\n  endpoints:");
    println!("    demo app:    https://podinfo.{}:31443", env.domain);
    if cfg.spec.observability.enabled {
        println!("    openobserve: https://o2.{}:31443", env.domain);
    }
    Ok(())
}

/// (name, ready, chart-version) for every Flux HelmRelease.
fn helm_releases() -> Vec<(String, bool, String)> {
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        metadata: Meta,
        #[serde(default)]
        status: HrStatus,
        #[serde(default)]
        spec: Spec,
    }
    #[derive(serde::Deserialize)]
    struct Meta {
        name: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct HrStatus {
        #[serde(default)]
        conditions: Vec<Cond>,
        #[serde(default)]
        history: Vec<Hist>,
    }
    #[derive(serde::Deserialize)]
    struct Cond {
        #[serde(rename = "type")]
        typ: String,
        status: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct Hist {
        #[serde(default, rename = "chartVersion")]
        chart_version: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct Spec {
        #[serde(default)]
        chart: Chart,
    }
    #[derive(serde::Deserialize, Default)]
    struct Chart {
        #[serde(default)]
        spec: ChartSpec,
    }
    #[derive(serde::Deserialize, Default)]
    struct ChartSpec {
        #[serde(default)]
        version: String,
    }

    let Some(yaml) = run::capture("kubectl", &["get", "helmreleases", "-A", "-o", "yaml"]) else {
        return vec![];
    };
    let list: List = serde_yaml::from_str(&yaml).unwrap_or(List { items: vec![] });
    list.items
        .into_iter()
        .map(|i| {
            let ready = i
                .status
                .conditions
                .iter()
                .any(|c| c.typ == "Ready" && c.status == "True");
            let ver = i
                .status
                .history
                .first()
                .map(|h| h.chart_version.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or(i.spec.chart.spec.version);
            (i.metadata.name, ready, ver)
        })
        .collect()
}

/// `dabba kubeconfig` — print the env's kubeconfig path (or an `export` line).
pub fn kubeconfig(config: &Path, env_name: Option<&str>, export: bool) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let kc = env_kubeconfig(config, &env)?
        .ok_or_else(|| anyhow::anyhow!("env {:?} is not deployed (no kubeconfig)", env.name))?;
    if export {
        println!("export KUBECONFIG={}", kc.display());
    } else {
        println!("{}", kc.display());
    }
    Ok(())
}

/// Secrets dabba stashes locally per env, listed and fetched under the `local/`
/// namespace. The OpenBao root token can't live in the vault it unlocks; the Forgejo
/// password is mirrored into OpenBao but also kept here for bootstrap.
const STASH_SECRETS: &[&str] = &["openbao-root", "forgejo-password"];

/// `dabba secret ls [path]` — list secrets. With no path (or `local`) it also lists the
/// local per-env stash under `local/`; an OpenBao path lists just that subtree.
pub fn secret_ls(config: &Path, env_name: Option<&str>, path: Option<&str>) -> Result<()> {
    if path.is_none() || path == Some("local") {
        let cfg = DabbaConfig::load(config)?;
        let env = cfg.resolve(env_name)?;
        if let Ok(workdir) = env_workdir(config, &env.name) {
            let present: Vec<&str> = STASH_SECRETS
                .iter()
                .copied()
                .filter(|n| !read_stash(&workdir, n).is_empty())
                .collect();
            if !present.is_empty() {
                println!("local/");
                for n in present {
                    println!("    {n}");
                }
            }
        }
        if path == Some("local") {
            return Ok(());
        }
    }
    let token = secret_ctx(config, env_name)?;
    let p = path.unwrap_or("secret");
    if !valid_kv_path(p) {
        bail!("invalid secret path {p:?}");
    }
    bao(&token, &format!("bao kv list -format=table {p}"))
}

/// `dabba secret get <path>` — show a secret's value.
/// `local/<name>` reads dabba's local per-env stash (e.g. `local/openbao-root`, the key
/// to OpenBao, which can't live inside it). Any other path is an OpenBao kv path under
/// `secret/`: `dabba/forgejo`, `demo/podinfo`.
pub fn secret_get(config: &Path, env_name: Option<&str>, name: &str) -> Result<()> {
    if let Some(stash_name) = name.strip_prefix("local/") {
        if !STASH_SECRETS.contains(&stash_name) {
            bail!(
                "unknown local secret {stash_name:?}; known: {}",
                STASH_SECRETS.join(", ")
            );
        }
        let cfg = DabbaConfig::load(config)?;
        let env = cfg.resolve(env_name)?;
        let workdir = env_workdir(config, &env.name)?;
        let val = read_stash(&workdir, stash_name);
        if val.is_empty() {
            bail!(
                "no stashed {stash_name} for env {:?} (not deployed?)",
                env.name
            );
        }
        println!("{val}");
        return Ok(());
    }
    let token = secret_ctx(config, env_name)?;
    let p = if name.starts_with("secret/") {
        name.to_string()
    } else {
        format!("secret/{name}")
    };
    if !valid_kv_path(&p) {
        bail!("invalid secret path {name:?}");
    }
    bao(&token, &format!("bao kv get -format=table {p}"))
}

/// Paths dabba hands to `bao` land in an `sh -c` string; restrict them to the simple
/// shape kv paths actually take ([A-Za-z0-9._-] segments joined by `/`) so a crafted
/// path can't inject shell.
fn valid_kv_path(p: &str) -> bool {
    !p.is_empty()
        && p.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// Resolve the env, point KUBECONFIG at it, and return the OpenBao token: the per-env
/// stashed root token (secret-zero), falling back to the dev default only for an env
/// brought up before secret-zero.
fn secret_ctx(config: &Path, env_name: Option<&str>) -> Result<String> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let kc = env_kubeconfig(config, &env)?
        .ok_or_else(|| anyhow::anyhow!("env {:?} is not deployed", env.name))?;
    std::env::set_var("KUBECONFIG", &kc);
    let token = env_workdir(config, &env.name)
        .ok()
        .and_then(|w| std::fs::read_to_string(w.join("openbao-root")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    Ok(token)
}

fn bao(token: &str, cmd: &str) -> Result<()> {
    run::run(
        "kubectl",
        &[
            "-n",
            "openbao",
            "exec",
            "openbao-0",
            "--",
            "sh",
            "-c",
            &format!("BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN={token} {cmd}"),
        ],
    )
}

/// `dabba ls` — list the configured environments and which one is the default.
pub fn ls(config: &Path) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let default = cfg.default_env_name().ok();
    for env in &cfg.spec.environments {
        let marker = if Some(env.name.as_str()) == default {
            "*"
        } else {
            " "
        };
        let deployed = env_workdir(config, &env.name)
            .ok()
            .map(|w| w.join("01-cluster").join(".terraform").is_dir() || env.kubeconfig.is_some())
            .unwrap_or(false);
        println!(
            "{marker} {:<16} {:?}{}",
            env.name,
            env.substrate,
            if deployed { "  (deployed)" } else { "" }
        );
    }
    Ok(())
}

/// `dabba diagram` — the visual half of `status`: render the env's live topology
/// (Flux Kustomizations + HelmReleases with health) as Mermaid (default) or ASCII.
pub fn diagram(config: &Path, env_name: Option<&str>, mermaid: bool) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    let kc = env_kubeconfig(config, &env)?
        .ok_or_else(|| anyhow::anyhow!("env {:?} is not deployed (no kubeconfig)", env.name))?;
    std::env::set_var("KUBECONFIG", &kc);
    let ksts = flux_ready("kustomizations");
    let hrs = helm_releases();
    if mermaid {
        render_mermaid(&env, &ksts, &hrs);
    } else {
        render_ascii(&env, &ksts, &hrs);
    }
    Ok(())
}

/// (name, ready) for every Flux resource of `kind`, from its Ready condition.
fn flux_ready(kind: &str) -> Vec<(String, bool)> {
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        metadata: Meta,
        #[serde(default)]
        status: Status,
    }
    #[derive(serde::Deserialize)]
    struct Meta {
        name: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct Status {
        #[serde(default)]
        conditions: Vec<Cond>,
    }
    #[derive(serde::Deserialize)]
    struct Cond {
        #[serde(rename = "type")]
        typ: String,
        status: String,
    }

    let Some(yaml) = run::capture("kubectl", &["get", kind, "-A", "-o", "yaml"]) else {
        return vec![];
    };
    let list: List = serde_yaml::from_str(&yaml).unwrap_or(List { items: vec![] });
    list.items
        .into_iter()
        .map(|i| {
            let ready = i
                .status
                .conditions
                .iter()
                .any(|c| c.typ == "Ready" && c.status == "True");
            (i.metadata.name, ready)
        })
        .collect()
}

fn mark(ready: bool) -> char {
    if ready {
        '✓'
    } else {
        '✗'
    }
}

fn render_ascii(env: &ResolvedEnv, ksts: &[(String, bool)], hrs: &[(String, bool, String)]) {
    let healthy =
        !ksts.is_empty() && ksts.iter().all(|(_, r)| *r) && hrs.iter().all(|(_, r, _)| *r);
    let verdict = if healthy {
        "reconciled"
    } else {
        "degraded / still settling"
    };
    println!(
        "env: {}  (substrate: {:?})        [{verdict}]\n",
        env.name, env.substrate
    );
    println!("  Forgejo (git) ──► Flux");
    println!("    Kustomizations:");
    for (n, r) in ksts {
        println!("      {} {n}", mark(*r));
    }
    if ksts.is_empty() {
        println!("      (none yet)");
    }
    println!("    HelmReleases:");
    for (n, r, v) in hrs {
        println!("      {} {n}  {v}", mark(*r));
    }
    if hrs.is_empty() {
        println!("      (none yet)");
    }
    println!("\n  secret path:  OpenBao ──► External Secrets ──► app");
    println!("  gateway:      https://podinfo.{}:31443", env.domain);
}

fn render_mermaid(env: &ResolvedEnv, ksts: &[(String, bool)], hrs: &[(String, bool, String)]) {
    let mut ok: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    println!(
        "%% dabba env {} — `dabba env {} diagram`",
        env.name, env.name
    );
    println!("graph TD");
    println!("  forgejo([\"Forgejo (git)\"]) --> flux([Flux])");
    for (i, (n, r)) in ksts.iter().enumerate() {
        let id = format!("k{i}");
        println!("  flux --> {id}[\"{} {n}\"]", mark(*r));
        if *r {
            ok.push(id)
        } else {
            bad.push(id)
        }
    }
    for (i, (n, r, v)) in hrs.iter().enumerate() {
        let id = format!("hr{i}");
        let label = if v.is_empty() {
            n.clone()
        } else {
            format!("{n} {v}")
        };
        println!("  flux --> {id}([\"{} {label}\"])", mark(*r));
        if *r {
            ok.push(id)
        } else {
            bad.push(id)
        }
    }
    println!(
        "  openbao([OpenBao]) --> eso([External Secrets]) --> app([\"podinfo.{}\"])",
        env.domain
    );
    println!("  classDef ok fill:#d4f7d4,stroke:#2e7d32;");
    println!("  classDef bad fill:#f7d4d4,stroke:#c62828;");
    if !ok.is_empty() {
        println!("  class {} ok;", ok.join(","));
    }
    if !bad.is_empty() {
        println!("  class {} bad;", bad.join(","));
    }
}

/// `dabba env <name>` (no verb) — show the env's resolved config.
pub fn show(config: &Path, env_name: &str) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(Some(env_name))?;
    println!("name:       {}", env.name);
    println!("substrate:  {:?}", env.substrate);
    println!("domain:     {}", env.domain);
    println!("issuer:     {:?}", env.issuer);
    if let Some(kc) = &env.kubeconfig {
        println!("kubeconfig: {kc}");
    }
    Ok(())
}

/// The kubeconfig for an env: the BYO path for `existing`, else the per-env
/// workdir kubeconfig if it exists.
fn env_kubeconfig(config: &Path, env: &ResolvedEnv) -> Result<Option<PathBuf>> {
    if env.substrate == Substrate::Existing {
        let kc = expand_tilde(env.kubeconfig.as_deref().unwrap_or_default());
        return Ok(kc.canonicalize().ok());
    }
    let kc = env_workdir(config, &env.name)?.join("kubeconfig");
    Ok(kc.is_file().then_some(kc))
}

fn preflight(env: &ResolvedEnv, workdir: &Path) -> Result<()> {
    for t in ["tofu", "kubectl", "git", "curl"] {
        if !on_path(t) {
            bail!("{t} not found on PATH");
        }
    }
    if !run::probe("docker", &["info"]) {
        bail!("docker is not running — start Docker and retry");
    }
    heal_ghcr_token();
    if env.substrate != Substrate::Existing {
        // These only apply to a FRESH provision. On a resume our own cluster
        // legitimately holds the name and the gateway ports, so skip them.
        let c01 = workdir.join("01-cluster");
        let fresh = !c01.join("terraform.tfstate").exists() && !c01.join(".terraform").is_dir();
        if fresh {
            check_gateway_ports()?;
            check_no_existing_cluster(env)?;
        }
        warn_low_inotify();
    }
    Ok(())
}

/// Low inotify limits make kind's k8s controllers crash mid-run with an opaque
/// "too many open files". Warn (don't block) so the user can raise them first.
fn warn_low_inotify() {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let instances = read("/proc/sys/fs/inotify/max_user_instances").unwrap_or(u64::MAX);
    let watches = read("/proc/sys/fs/inotify/max_user_watches").unwrap_or(u64::MAX);
    if instances < 256 || watches < 524_288 {
        eprintln!(
            "⚠ inotify limits are low (instances={instances}, watches={watches}); kind pods may \
             crash with \"too many open files\". raise them with:\n    \
             sudo sysctl fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=1048576"
        );
    }
}

/// A cluster with the env's name already existing (often a leftover from a failed run
/// whose state was wiped) makes provisioning fail opaquely. Catch it with guidance.
fn check_no_existing_cluster(env: &ResolvedEnv) -> Result<()> {
    let exists = match env.substrate {
        Substrate::Kind => run::capture("kind", &["get", "clusters"])
            .is_some_and(|o| o.lines().any(|l| l.trim() == env.name)),
        Substrate::K3d => {
            run::capture("k3d", &["cluster", "list", "--no-headers"]).is_some_and(|o| {
                o.lines()
                    .any(|l| l.split_whitespace().next() == Some(&env.name))
            })
        }
        Substrate::Minikube => run::capture("minikube", &["profile", "list", "-o", "json"])
            .is_some_and(|o| o.contains(&format!("\"Name\":\"{}\"", env.name))),
        _ => false,
    };
    if exists {
        let del = match env.substrate {
            Substrate::Kind => format!("kind delete cluster --name {}", env.name),
            Substrate::K3d => format!("k3d cluster delete {}", env.name),
            Substrate::Minikube => format!("minikube delete -p {}", env.name),
            _ => unreachable!(),
        };
        bail!(
            "a cluster named {:?} already exists.\n  \
             if it is a dabba leftover: `dabba env {} down` (or, if its workdir is gone, `{}`), \
             then retry.",
            env.name,
            env.name,
            del
        );
    }
    Ok(())
}

/// Point Helm at an empty OCI registry config for this run. dabba's bootstrap charts
/// are all PUBLIC, so a stale/wrong-scoped ghcr login in the user's ~/.config/helm or
/// ~/.docker must not be presented — ghcr answers a presented-but-bad cred with `403:
/// denied` instead of serving the chart anonymously.
fn isolate_helm_registry(workdir: &Path) -> Result<()> {
    let cfg = workdir.join("helm-registry.json");
    std::fs::write(&cfg, "{\"auths\":{}}").with_context(|| format!("writing {}", cfg.display()))?;
    std::env::set_var("HELM_REGISTRY_CONFIG", &cfg);
    Ok(())
}

/// A set-but-invalid GH_TOKEN/GITHUB_TOKEN makes Helm present it to ghcr.io and get a
/// 403 on otherwise-public charts (an opaque failure ~8 min into apply). When ghcr
/// rejects it, strip it from our process env so the child tofu/helm pulls anonymously.
/// Safe: a rejected token is useless anyway, and tier-0 charts are all public.
fn heal_ghcr_token() {
    let url = "https://ghcr.io/token?scope=repository:controlplaneio-fluxcd/charts/flux-operator:pull&service=ghcr.io";
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        let Some(tok) = std::env::var(var).ok().filter(|t| !t.is_empty()) else {
            continue;
        };
        // Best-effort: only act when ghcr clearly rejects the token (offline → skip).
        let code = run::capture(
            "curl",
            &[
                "-s",
                "-m",
                "8",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-u",
                &format!("x:{tok}"),
                url,
            ],
        );
        if matches!(code.as_deref(), Some("401") | Some("403")) {
            eprintln!(
                "⚠ {var} is set but ghcr.io rejects it (HTTP {}) — removing it for this run so \
                 dabba's public charts pull anonymously.\n  \
                 (fix or `unset {var}` in your shell profile to silence this; the same bad token \
                 also breaks `gh` and `docker login ghcr.io`.)",
                code.unwrap_or_default()
            );
            std::env::remove_var(var);
        }
    }
}

/// Provisioned substrates publish the gateway on host ports; if something already
/// holds them, `docker run` dies with an opaque `exit status 125`. Check first.
fn check_gateway_ports() -> Result<()> {
    for port in [31080u16, 31443] {
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
            bail!(
                "host port {port} is already in use — the cluster needs it for the gateway, and \
                 docker would otherwise fail with an opaque `exit status 125`.\n  \
                 free it (stop whatever is bound there), or pick another via \
                 substrateConfig.httpPort/httpsPort."
            );
        }
    }
    Ok(())
}

fn seed_forgejo(cfg: &DabbaConfig, opts: &Options, forgejo_pw: &str) -> Result<()> {
    log("Loading the gitops content into Forgejo");
    run::wait_for("forgejo service", WAIT_ATTEMPTS, || {
        run::probe(
            "kubectl",
            &["-n", "git-server", "get", "svc", "forgejo-http"],
        )
    })?;

    let mut pf = run::spawn(
        "kubectl",
        &[
            "-n",
            "git-server",
            "port-forward",
            "svc/forgejo-http",
            "3000:3000",
        ],
    )?;
    // Kill the port-forward whether seeding succeeds or fails.
    let result = seed_forgejo_inner(cfg, opts, forgejo_pw);
    let _ = pf.kill();
    result?;

    // Pull now rather than waiting out the sync interval.
    run::try_run(
        "kubectl",
        &[
            "-n",
            "flux-system",
            "annotate",
            "gitrepository/flux-system",
            &format!("reconcile.fluxcd.io/requestedAt={}", now_secs()),
            "--overwrite",
        ],
    );
    Ok(())
}

fn seed_forgejo_inner(cfg: &DabbaConfig, opts: &Options, forgejo_pw: &str) -> Result<()> {
    run::wait_for("forgejo api", WAIT_ATTEMPTS, || {
        run::probe(
            "curl",
            &[
                "-sf",
                "-o",
                "/dev/null",
                "http://localhost:3000/api/v1/version",
            ],
        )
    })?;

    let creds = format!("{FORGEJO_USER}:{forgejo_pw}");
    run::try_run(
        "curl",
        &[
            "-sf",
            "-u",
            &creds,
            "-X",
            "POST",
            "http://localhost:3000/api/v1/user/repos",
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"name":"dabba-gitops","private":false,"auto_init":false}"#,
        ],
    );

    let src = std::env::temp_dir().join(format!("dabba-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(&src).context("creating seed temp dir")?;
    let src_str = src.to_string_lossy().to_string();

    match &opts.gitops_seed {
        Some(dir) => {
            run::run("cp", &["-r", &format!("{}/.", dir.display()), &src_str])?;
        }
        None => {
            let upstream = &cfg.spec.git.upstream;
            if upstream.trim().is_empty() {
                bail!("no gitops content: set spec.git.upstream or pass --gitops-seed <dir>");
            }
            run::run("git", &["clone", "-q", "--depth", "1", upstream, &src_str])?;
        }
    }
    let _ = std::fs::remove_dir_all(src.join(".git"));
    render_cluster_selection(&src, cfg)?;

    let cdir = src_str.as_str();
    run::run_quiet("git", &["-C", cdir, "init", "-q"])?;
    run::run_quiet("git", &["-C", cdir, "add", "-A"])?;
    run::run_quiet(
        "git",
        &[
            "-C",
            cdir,
            "-c",
            "user.email=seed@dabba.local",
            "-c",
            "user.name=dabba",
            "commit",
            "-qm",
            "seed gitops content",
        ],
    )?;
    let push_url = format!(
        "http://{FORGEJO_USER}:{forgejo_pw}@localhost:3000/{FORGEJO_USER}/dabba-gitops.git"
    );
    // Force: dabba's seed is the authoritative source, and each seed is a fresh
    // `git init` (unrelated history), so re-seeds must overwrite the existing branch.
    run::run_quiet(
        "git",
        &["-C", cdir, "push", "-q", "--force", &push_url, "HEAD:main"],
    )?;
    let _ = std::fs::remove_dir_all(&src);
    Ok(())
}

/// Render the dabba-MANAGED selection of active Flux Kustomizations from the config,
/// into the seeded gitops (generated; overwrites clusters/local/kustomization.yml).
/// Component manifests stay hand-authored; only this selection is generated.
fn render_cluster_selection(seed: &Path, cfg: &DabbaConfig) -> Result<()> {
    let ksum = seed.join("clusters/local/kustomization.yml");
    if !ksum.exists() {
        return Ok(()); // not the tier-0 layout — nothing to render
    }
    let mut resources = vec!["crds.yaml", "platform.yaml", "use-case.yaml"];
    if cfg.spec.observability.enabled {
        resources.push("observability.yaml");
    }
    let list = resources
        .iter()
        .map(|r| format!("  - {r}"))
        .collect::<Vec<_>>()
        .join("\n");
    let content = format!(
        "# GENERATED by dabba from dabba.yaml — do not edit; overwritten on `dabba up`/`apply`.\n\
         # The dabba-MANAGED selection of which per-cluster Flux Kustomizations are active.\n\
         # Component manifests are hand-authored; only this selection is generated.\n\
         # To change what's enabled, edit dabba.yaml.\n\
         apiVersion: kustomize.config.k8s.io/v1beta1\n\
         kind: Kustomization\n\n\
         resources:\n{list}\n"
    );
    std::fs::write(&ksum, content).with_context(|| format!("writing {}", ksum.display()))?;
    Ok(())
}

fn seed_openbao(openbao_root: &str, forgejo_pw: &str, openobserve_pw: &str) -> Result<()> {
    log("Waiting for OpenBao (flux is installing the platform — give it a few minutes)");
    run::wait_for("openbao pod", WAIT_ATTEMPTS, || {
        run::probe("kubectl", &["-n", "openbao", "get", "pod", "openbao-0"])
    })?;
    run::run(
        "kubectl",
        &[
            "-n",
            "openbao",
            "wait",
            "--for=condition=ready",
            "pod/openbao-0",
            "--timeout=300s",
        ],
    )?;

    log("Seeding secrets into OpenBao");
    // Demo secret + mirror the per-env app creds in so `dabba secret get` can read them.
    let script = format!(
        "export BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN={openbao_root}; \
         bao kv put secret/demo/podinfo message='{DEMO_MESSAGE}'; \
         bao kv put secret/dabba/forgejo username={FORGEJO_USER} password='{forgejo_pw}'; \
         bao kv put secret/dabba/openobserve email=admin@dabba.local password='{openobserve_pw}'"
    );
    run::run(
        "kubectl",
        &[
            "-n",
            "openbao",
            "exec",
            "openbao-0",
            "--",
            "sh",
            "-c",
            &script,
        ],
    )?;

    // Nudge the secret sync + app install rather than waiting out the intervals.
    run::wait_for("podinfo ExternalSecret", WAIT_ATTEMPTS, || {
        run::probe(
            "kubectl",
            &["-n", "podinfo", "get", "externalsecret", "podinfo-message"],
        )
    })?;
    run::try_run(
        "kubectl",
        &[
            "-n",
            "podinfo",
            "annotate",
            "externalsecret",
            "podinfo-message",
            &format!("force-sync={}", now_secs()),
            "--overwrite",
        ],
    );
    run::wait_for("podinfo HelmRelease", WAIT_ATTEMPTS, || {
        run::probe(
            "kubectl",
            &["-n", "podinfo", "get", "helmrelease", "podinfo"],
        )
    })?;
    run::try_run(
        "kubectl",
        &[
            "-n",
            "podinfo",
            "annotate",
            "helmrelease",
            "podinfo",
            &format!("reconcile.fluxcd.io/requestedAt={}", now_secs()),
            "--overwrite",
        ],
    );
    Ok(())
}

/// Verify the platform actually reached the desired state across layers, instead of
/// declaring success after our own steps and letting an async Flux/ESO/workload
/// failure pass silently. Polls until every Flux Kustomization + HelmRelease is Ready
/// and no pod is crashlooping; on timeout it surfaces the REAL error from each failing
/// layer (the Flux condition message, the pod's crash reason + error log line).
pub fn wait_for_reconciled(attempts: usize) -> Result<()> {
    log("Verifying the platform reconciled (Flux + workloads)");
    for _ in 0..attempts {
        let ksts = flux_ready("kustomizations");
        if !ksts.is_empty() && reconcile_failures().is_empty() {
            log("✓ all Flux Kustomizations + HelmReleases ready, no crashing pods");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    let fails = reconcile_failures();
    bail!(
        "platform did not fully reconcile in time — the failing layers:\n  {}",
        if fails.is_empty() {
            "(no specific failure found — check `dabba status` / `flux get all -A`)".to_string()
        } else {
            fails.join("\n  ")
        }
    );
}

/// Not-ready Flux resources + crashing pods, each with its actual error message.
fn reconcile_failures() -> Vec<String> {
    let mut out = Vec::new();
    for (kind, label) in [
        ("kustomizations", "kustomization"),
        ("helmreleases", "helmrelease"),
    ] {
        for (name, ready, msg) in flux_conditions(kind) {
            if !ready {
                let m = if msg.is_empty() {
                    "not ready".into()
                } else {
                    msg
                };
                out.push(format!("{label}/{name}: {m}"));
            }
        }
    }
    out.extend(unhealthy_pods());
    out
}

/// (name, ready, Ready-condition-message) for a Flux resource kind.
fn flux_conditions(kind: &str) -> Vec<(String, bool, String)> {
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        metadata: Meta,
        #[serde(default)]
        status: St,
    }
    #[derive(serde::Deserialize)]
    struct Meta {
        name: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct St {
        #[serde(default)]
        conditions: Vec<Cond>,
    }
    #[derive(serde::Deserialize)]
    struct Cond {
        #[serde(rename = "type")]
        typ: String,
        status: String,
        #[serde(default)]
        message: String,
    }
    let Some(yaml) = run::capture("kubectl", &["get", kind, "-A", "-o", "yaml"]) else {
        return vec![];
    };
    let list: List = serde_yaml::from_str(&yaml).unwrap_or(List { items: vec![] });
    list.items
        .into_iter()
        .map(|i| {
            let ready = i.status.conditions.iter().find(|c| c.typ == "Ready");
            (
                i.metadata.name,
                ready.map(|c| c.status == "True").unwrap_or(false),
                ready.map(|c| c.message.clone()).unwrap_or_default(),
            )
        })
        .collect()
}

/// Crashlooping / image-pull-failing pods, each with its crash reason and (best-effort)
/// the error/panic line from its log — so the real cause surfaces, not just "not ready".
fn unhealthy_pods() -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Pod>,
    }
    #[derive(serde::Deserialize)]
    struct Pod {
        metadata: Meta,
        #[serde(default)]
        status: PStatus,
    }
    #[derive(serde::Deserialize)]
    struct Meta {
        name: String,
        namespace: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct PStatus {
        #[serde(default)]
        phase: String,
        #[serde(default, rename = "containerStatuses")]
        cs: Vec<Cs>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Cs {
        #[serde(default)]
        state: CState,
    }
    #[derive(serde::Deserialize, Default)]
    struct CState {
        #[serde(default)]
        waiting: Option<Waiting>,
    }
    #[derive(serde::Deserialize)]
    struct Waiting {
        #[serde(default)]
        reason: String,
    }
    let bad = ["CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"];
    let Some(yaml) = run::capture("kubectl", &["get", "pods", "-A", "-o", "yaml"]) else {
        return vec![];
    };
    let list: List = serde_yaml::from_str(&yaml).unwrap_or(List { items: vec![] });
    let mut out = Vec::new();
    for p in list.items {
        if p.status.phase == "Succeeded" {
            continue;
        }
        for c in &p.status.cs {
            let Some(w) = &c.state.waiting else { continue };
            if bad.contains(&w.reason.as_str()) {
                let ns = &p.metadata.namespace;
                let log_line = run::capture(
                    "kubectl",
                    &[
                        "-n",
                        ns,
                        "logs",
                        &p.metadata.name,
                        "--tail=20",
                        "--all-containers",
                    ],
                )
                .and_then(|l| {
                    l.lines()
                        .rev()
                        .find(|ln| {
                            let lc = ln.to_lowercase();
                            lc.contains("error") || lc.contains("panic") || lc.contains("fatal")
                        })
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
                let detail = if log_line.is_empty() {
                    w.reason.clone()
                } else {
                    format!("{} — {log_line}", w.reason)
                };
                out.push(format!("pod {ns}/{}: {detail}", p.metadata.name));
                break;
            }
        }
    }
    out
}

fn print_summary(env: &ResolvedEnv, kubeconfig: &Path) {
    println!(
        "\n✓ {name} ready\n\n  \
         demo app:    https://podinfo.{domain}:31443   (the banner is served from OpenBao)\n  \
         openbao ui:  https://bao.{domain}:31443        token: dabba secret get local/openbao-root\n  \
         git server:  kubectl -n git-server port-forward svc/forgejo-http 3000:3000\n               \
         then http://localhost:3000  (user {user}; pw: dabba secret get dabba/forgejo)\n  \
         flux:        KUBECONFIG={kube} flux get kustomizations\n\n  \
         credentials are per-env random (no shipped defaults) — `dabba secret ls` / `dabba secret get`\n  \
         TLS uses a self-signed CA, so your browser will warn — that is expected.\n  \
         teardown:    dabba env {name} down",
        name = env.name,
        domain = env.domain,
        user = FORGEJO_USER,
        kube = kubeconfig.display(),
    );
}

fn substrate_dir(s: Substrate) -> Result<&'static str> {
    Ok(match s {
        Substrate::Kind => "kind",
        Substrate::K3d => "k3d",
        Substrate::Minikube => "minikube",
        Substrate::ScalewayKapsule | Substrate::Eks => {
            bail!("cloud substrates (Tier 1) are not built yet")
        }
        // Existing is handled before this is called (no provisioning module).
        Substrate::Existing => bail!("existing substrate has no provisioning module"),
    })
}

/// Expand a leading `~/` to $HOME; otherwise pass through unchanged.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn issuer_name(i: Issuer) -> &'static str {
    match i {
        Issuer::Selfsigned => "dabba-ca",
        Issuer::Acme => "letsencrypt",
    }
}

/// Rewrite the `module "cluster"` source line in 01-cluster/main.tf to `source`.
/// Matches whatever is there now (idempotent across re-runs / substrate swaps).
fn rewrite_module_source(main_tf: &Path, source: &str) -> Result<()> {
    let text = std::fs::read_to_string(main_tf)
        .with_context(|| format!("reading {}", main_tf.display()))?;
    let mut replaced = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && line.contains("source") && line.contains("/modules/") {
                replaced = true;
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}source = \"{source}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        bail!(
            "could not find a module source line in {}",
            main_tf.display()
        );
    }
    std::fs::write(main_tf, out.join("\n") + "\n")
        .with_context(|| format!("writing {}", main_tf.display()))?;
    Ok(())
}

/// Rewrite every dabba-modules git source in a file to a local path, preserving
/// the module name. Used for 02-bootstrap (git-server, flux-operator) in dev.
fn localize_module_sources(main_tf: &Path, local: &str) -> Result<()> {
    let text = std::fs::read_to_string(main_tf)
        .with_context(|| format!("reading {}", main_tf.display()))?;
    let out: Vec<String> = text
        .lines()
        .map(|line| match module_name_in(line) {
            Some(name) => {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}source = \"{local}/modules/{name}\"")
            }
            None => line.to_string(),
        })
        .collect();
    std::fs::write(main_tf, out.join("\n") + "\n")
        .with_context(|| format!("writing {}", main_tf.display()))?;
    Ok(())
}

/// If `line` is a dabba-modules git source, return the module name (e.g. "git-server").
fn module_name_in(line: &str) -> Option<String> {
    if !line.contains("source") || !line.contains("dabba-modules.git//modules/") {
        return None;
    }
    let after = line.split("//modules/").nth(1)?;
    let name = after.split('?').next()?;
    Some(name.to_string())
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn log(msg: &str) {
    eprintln!("▸ {msg}");
}
