//! dabba — bring up a full Kubernetes platform from a single config.
//!
//! Command surface (modeled on spice-labs-cli: `dabba <command> [opts]`):
//!   bare verbs act on the config's default environment;
//!   `dabba env <name> <verb>` targets a specific one.
//!   day-0:  init | up | down | doctor
//!   env:    ls | use <name> | env <name> [add|up|down|status|kubeconfig|diagram|rm]
//!   read:   status | kubeconfig | diagram | secret
//!   config: config validate | show
//!   shell:  completions <shell>

mod application;
mod backend;
mod config;
mod edit;
mod environment;
mod render;
mod run;

use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use config::DabbaConfig;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "dabba",
    version,
    about = "Bring up a full Kubernetes platform from a single config"
)]
struct Cli {
    /// Path to the DabbaConfig file
    #[arg(short, long, global = true, default_value = "dabba.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// Flags shared by `up` (top-level and per-env).
#[derive(Args)]
struct UpArgs {
    /// Directory holding the quickstart tofu (01-cluster, 02-bootstrap)
    #[arg(long, default_value = "quickstart")]
    quickstart_dir: PathBuf,
    /// Override module sources with a local path (dev): <path>/modules/<substrate>
    #[arg(long)]
    modules_source: Option<String>,
    /// Local gitops content to seed Forgejo from (else clone spec.git.upstream)
    #[arg(long)]
    gitops_seed: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config (the local kind / k3d / minikube environments)
    Init,
    /// Bring the default environment up (day-0 bootstrap)
    Up(UpArgs),
    /// Tear the default environment down
    Down,
    /// Show what is running for the default environment
    Status,
    /// Print the default environment's kubeconfig path
    Kubeconfig {
        /// Print an `export KUBECONFIG=…` line instead of the bare path
        #[arg(long)]
        export: bool,
    },
    /// Draw the default environment's live topology (ASCII; --mermaid to embed)
    Diagram {
        /// Emit Mermaid graph text (for GitHub/markdown/mmdc) instead of ASCII
        #[arg(long)]
        mermaid: bool,
    },
    /// Read OpenBao secrets for the default environment
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// List configured environments (and which is the default)
    Ls,
    /// Set the default environment
    Use { name: String },
    /// Operate on a named environment: dabba env <name> <verb>
    Env {
        /// Environment name
        name: String,
        #[command(subcommand)]
        action: Option<EnvAction>,
    },
    /// Preflight checks (docker / tools / cluster reachable)
    Doctor,
    /// The pinned toolchain and service versions, and whether this machine matches
    Environment {
        #[command(subcommand)]
        action: EnvironmentAction,
    },
    /// Work with portable application definitions
    Application {
        #[command(subcommand)]
        action: ApplicationAction,
    },
    /// Manage the dabba config
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print a shell completion script (bash, zsh, fish, …) to stdout
    Completions {
        /// Target shell
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum EnvAction {
    /// Add this environment to the config
    Add {
        #[arg(long)]
        substrate: String,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Bring this environment up
    Up(UpArgs),
    /// Tear this environment down
    Down,
    /// Show what is running for this environment
    Status,
    /// Print this environment's kubeconfig path
    Kubeconfig {
        #[arg(long)]
        export: bool,
    },
    /// Draw this environment's live topology (ASCII; --mermaid to embed)
    Diagram {
        #[arg(long)]
        mermaid: bool,
    },
    /// Remove this environment from the config
    Rm,
}

#[derive(Subcommand)]
enum SecretAction {
    /// List secrets (OpenBao + the local/ stash; default lists both)
    Ls { path: Option<String> },
    /// Show a secret's value, e.g. `dabba/forgejo` or `local/openbao-root`
    Get { name: String },
}

#[derive(Subcommand)]
enum EnvironmentAction {
    /// Print the pins and how this machine compares
    Show,
    /// Exit non-zero if this machine does not match the pins
    Check,
    /// Print the pins as shell exports, for a workflow or shell to eval
    Export,
    /// Report Application definitions whose tag disagrees with a service pin
    Verify {
        /// Directory of Application definitions
        #[arg(default_value = "examples/applications")]
        directory: PathBuf,
    },
}

#[derive(Subcommand)]
enum ApplicationAction {
    /// Validate an application definition against the portable schema
    Validate {
        /// Path to the Application YAML
        file: PathBuf,
    },
    /// Print a starter definition exercising every portable field
    Example,
    /// Render a definition for a substrate (compose file, or Kubernetes objects)
    Render {
        /// Path to the Application YAML
        file: PathBuf,
        /// Substrate to render for (any Kubernetes substrate, or docker-host)
        #[arg(long, default_value = "docker-host")]
        substrate: String,
    },
    /// Report which parts of a definition a given substrate will NOT honour
    Portability {
        /// Path to the Application YAML
        file: PathBuf,
        /// Substrate to check against (any Kubernetes substrate, or docker-host)
        #[arg(long, default_value = "docker-host")]
        substrate: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate a config against the schema
    Validate {
        #[arg(default_value = "dabba.yaml")]
        file: PathBuf,
    },
    /// Show the parsed, validated config
    Show {
        #[arg(default_value = "dabba.yaml")]
        file: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = cli.config;
    match cli.command {
        Command::Config { action } => match action {
            ConfigAction::Validate { file } => {
                DabbaConfig::load(&file)?;
                println!("✓ {} is valid", file.display());
                Ok(())
            }
            ConfigAction::Show { file } => {
                let parsed = DabbaConfig::load(&file)?;
                print!(
                    "{}",
                    serde_yaml::to_string(&parsed).context("re-serializing config")?
                );
                Ok(())
            }
        },
        Command::Doctor => doctor(&cfg),
        Command::Environment { action } => {
            let parsed = DabbaConfig::load(&cfg)?;
            match action {
                EnvironmentAction::Show => environment::show(&parsed.spec.environment),
                EnvironmentAction::Check => environment::check(&parsed.spec.environment),
                EnvironmentAction::Export => environment::export(&parsed.spec.environment),
                EnvironmentAction::Verify { directory } => {
                    let applications = environment::load_applications(&directory)?;
                    let mut problems = environment::check_application_versions(
                        &parsed.spec.environment,
                        &applications,
                    );
                    // Hand-written compose stacks carry pinned images too; the
                    // reconciler still accepts them, so a pin that only reached
                    // rendered artifacts would miss the stack it was written for.
                    let compose =
                        environment::load_compose_files(Path::new("backends")).unwrap_or_default();
                    problems.extend(environment::check_compose_versions(
                        &parsed.spec.environment,
                        &compose,
                    ));

                    if problems.is_empty() {
                        println!(
                            "✓ {} definition(s) and {} compose stack(s) agree with the \
                             pinned service versions",
                            applications.len(),
                            compose.len()
                        );
                        return Ok(());
                    }
                    for problem in &problems {
                        println!("  ≠ {problem}");
                    }
                    anyhow::bail!("{} disagreement(s) with the pin", problems.len())
                }
            }
        }
        Command::Application { action } => match action {
            ApplicationAction::Validate { file } => {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let app = application::Application::parse(&text)?;
                println!(
                    "✓ {} is a valid portable application definition",
                    app.metadata.name
                );
                Ok(())
            }
            ApplicationAction::Example => {
                // The fixture the conformance matrix renders through both
                // backends. Printing that exact value means the example users
                // start from cannot drift from the one that is proven to work.
                println!("{}", application::EXHAUSTIVE_EXAMPLE.trim_start());
                Ok(())
            }
            ApplicationAction::Render { file, substrate } => {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let app = application::Application::parse(&text)?;
                let rendered = if substrate == "docker-host" {
                    render::compose::render(&app)?
                } else {
                    render::kubernetes::render(&app)?
                };
                print!("{rendered}");
                Ok(())
            }
            ApplicationAction::Portability { file, substrate } => {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let app = application::Application::parse(&text)?;
                let target_is_kubernetes = substrate != "docker-host";
                let ignored = app.non_portable_fields(target_is_kubernetes);
                if ignored.is_empty() {
                    println!(
                        "✓ {} is fully portable to {substrate} (no substrate-specific blocks)",
                        app.metadata.name
                    );
                } else {
                    println!(
                        "! {} carries configuration {substrate} will ignore:",
                        app.metadata.name
                    );
                    for field in &ignored {
                        println!("    spec.{field}");
                    }
                    println!(
                        "  These are escape hatches, so this is expected — but the \n  \
                         behaviour they provide will not exist on {substrate}."
                    );
                }
                Ok(())
            }
        },
        Command::Up(a) => backend::select(&cfg, None)?.up(&up_options(&cfg, None, a)),
        Command::Down => backend::select(&cfg, None)?.down(&backend::DownOptions {
            config: cfg,
            env: None,
        }),
        Command::Status => backend::select(&cfg, None)?.status(&cfg, None),
        // `kubeconfig` is a Kubernetes concept, not a Backend verb — dispatch it
        // straight to the k8s backend (a docker-host env reports "not deployed").
        Command::Kubeconfig { export } => backend::kubernetes::kubeconfig(&cfg, None, export),
        Command::Diagram { mermaid } => backend::select(&cfg, None)?.diagram(&cfg, None, mermaid),
        Command::Secret { action } => {
            let b = backend::select(&cfg, None)?;
            match action {
                SecretAction::Ls { path } => b.secret_ls(&cfg, None, path.as_deref()),
                SecretAction::Get { name } => b.secret_get(&cfg, None, &name),
            }
        }
        Command::Ls => backend::common::ls(&cfg),
        Command::Use { name } => edit::use_env(&cfg, &name),
        Command::Init => edit::init(&cfg),
        Command::Env { name, action } => dispatch_env(&cfg, &name, action),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
    }
}

fn dispatch_env(config: &Path, name: &str, action: Option<EnvAction>) -> Result<()> {
    let env = Some(name.to_string());
    match action {
        None => backend::common::show(config, name),
        Some(EnvAction::Up(a)) => {
            backend::select(config, Some(name))?.up(&up_options(config, env, a))
        }
        Some(EnvAction::Down) => backend::select(config, Some(name))?.down(&backend::DownOptions {
            config: config.to_path_buf(),
            env,
        }),
        Some(EnvAction::Status) => backend::select(config, Some(name))?.status(config, Some(name)),
        Some(EnvAction::Kubeconfig { export }) => {
            backend::kubernetes::kubeconfig(config, Some(name), export)
        }
        Some(EnvAction::Diagram { mermaid }) => {
            backend::select(config, Some(name))?.diagram(config, Some(name), mermaid)
        }
        Some(EnvAction::Add { substrate, domain }) => {
            edit::add_env(config, name, &substrate, domain.as_deref())
        }
        Some(EnvAction::Rm) => edit::rm_env(config, name),
    }
}

fn up_options(config: &Path, env: Option<String>, a: UpArgs) -> backend::Options {
    backend::Options {
        config: config.to_path_buf(),
        env,
        quickstart_dir: a.quickstart_dir,
        modules_source: a.modules_source,
        gitops_seed: a.gitops_seed,
    }
}

/// Minimum OpenTofu/Terraform version (the modules declare `required_version >= 1.6.0`).
const MIN_TOFU: &str = "1.6.0";

/// Check the day-0 prerequisites: tools present, the right versions, and (for
/// docker) actually running.
/// `dabba doctor` — check the tools the CONFIGURED substrates actually need.
///
/// This used to demand kubectl and tofu unconditionally, so it failed for anyone
/// whose only environment was docker-host — on a substrate whose entire premise is
/// not needing Kubernetes. A missing config is treated as "might be anything", so
/// running `dabba doctor` before `dabba init` still checks everything.
fn doctor(config: &Path) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    let substrates: Vec<config::Substrate> = config::DabbaConfig::load(config)
        .map(|cfg| cfg.spec.environments.iter().map(|e| e.substrate).collect())
        .unwrap_or_default();
    let needs_kubernetes = substrates.is_empty()
        || substrates
            .iter()
            .any(|s| !matches!(s, config::Substrate::DockerHost));
    let needs_docker_host = substrates.is_empty()
        || substrates
            .iter()
            .any(|s| matches!(s, config::Substrate::DockerHost));

    // docker: on PATH AND the daemon is reachable (a stopped daemon is the classic trap).
    if !on_path("docker") {
        println!("  ✗ docker (not found)");
        problems.push("docker not on PATH".into());
    } else if !run::probe("docker", &["info"]) {
        println!("  ✗ docker (installed, but the daemon isn't reachable — is it running?)");
        problems.push("docker daemon not running".into());
    } else {
        println!("  ✓ docker");
    }

    // The reconciler on a compose host drives git and bash directly.
    if needs_docker_host {
        for tool in ["git", "bash"] {
            if on_path(tool) {
                println!("  ✓ {tool}");
            } else {
                println!("  ✗ {tool} (not found; needed by the docker-host substrate)");
                problems.push(format!("{tool} not on PATH"));
            }
        }
    }

    if needs_kubernetes {
        // kubectl: presence is enough (it's tolerant of version skew).
        if on_path("kubectl") {
            println!("  ✓ kubectl");
        } else {
            println!("  ✗ kubectl (not found)");
            problems.push("kubectl not on PATH".into());
        }

        // tofu: on PATH AND >= MIN_TOFU (an older tofu fails confusingly mid-`up`).
        match tofu_version() {
            None => {
                println!("  ✗ tofu (not found)");
                problems.push("tofu not on PATH".into());
            }
            Some(v) if version_lt(&v, MIN_TOFU) => {
                println!("  ✗ tofu {v} (need >= {MIN_TOFU})");
                problems.push(format!("tofu {v} is older than {MIN_TOFU}"));
            }
            Some(v) => println!("  ✓ tofu ({v})"),
        }
    } else {
        println!("  · kubectl/tofu not checked (no Kubernetes substrate configured)");
    }

    if problems.is_empty() {
        println!("✓ preflight ok");
        Ok(())
    } else {
        anyhow::bail!("preflight failed: {}", problems.join("; "))
    }
}

/// The installed OpenTofu/Terraform version, e.g. "1.12.2" (first line of
/// `tofu version` → "OpenTofu v1.12.2").
fn tofu_version() -> Option<String> {
    let out = run::capture("tofu", &["version"])?;
    out.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(|s| s.trim_start_matches('v').to_string())
}

/// Parse a dotted version into a comparable tuple, ignoring any trailing suffix
/// on each part (e.g. "31+" → 31).
fn parse_semver(v: &str) -> (u32, u32, u32) {
    let mut p = v.split('.').map(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
    )
}

fn version_lt(a: &str, b: &str) -> bool {
    parse_semver(a) < parse_semver(b)
}

/// True if `bin` is an executable file somewhere on PATH.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the CLI offers has to appear in the docs.
    ///
    /// This branch added two top-level nouns, `application` and `environment`, and
    /// listed neither — they were reachable only by running `--help` or by finding
    /// the page that happened to mention them. Derived from the enum rather than a
    /// hand-kept list, for the same reason every other list here is.
    #[test]
    fn every_command_is_listed_in_the_docs() {
        let source = include_str!("main.rs");
        let docs = include_str!("../docs/src/content/docs/index.mdx");

        let body = source
            .split_once("enum Command {")
            .expect("the Command enum is declared in main.rs")
            .1
            .split_once("\n}\n")
            .expect("the enum is closed")
            .0;

        let mut commands: Vec<String> = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            // A variant is `Name,` or `Name {`, at the top level of the enum.
            let name = line.trim_end_matches(&[',', ' ', '{'][..]);
            if name.is_empty()
                || !name.starts_with(|c: char| c.is_ascii_uppercase())
                || !name.chars().all(|c| c.is_ascii_alphanumeric())
            {
                continue;
            }
            // clap derives the command name by lowercasing the variant.
            commands.push(name.to_lowercase());
        }
        commands.sort();
        commands.dedup();

        assert!(
            commands.len() >= 10,
            "parsed only {commands:?} out of the Command enum; the scan broke and this \
             test would pass vacuously"
        );

        for command in &commands {
            assert!(
                docs.contains(&format!("`dabba {command}")),
                "`dabba {command}` exists but is listed nowhere in the docs index, so \
                 the only way to find it is --help"
            );
        }
    }

    #[test]
    fn version_compare() {
        assert!(version_lt("1.5.7", "1.6.0"));
        assert!(version_lt("1.6.0", "1.12.2"));
        assert!(!version_lt("1.6.0", "1.6.0"));
        assert!(!version_lt("1.12.2", "1.6.0"));
        // tolerant of trailing suffixes (e.g. a k8s minor like "31+")
        assert_eq!(parse_semver("1.31+"), (1, 31, 0));
        assert_eq!(parse_semver("v1.32.2"), (0, 32, 2)); // leading 'v' is stripped by callers
    }
}
