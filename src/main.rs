//! dabba — bring up a full Kubernetes platform from a single config.
//!
//! Command surface (modeled on spice-labs-cli: `dabba <command> [opts]`):
//!   bare verbs act on the config's default environment;
//!   `dabba env <name> <verb>` targets a specific one.
//!   day-0:  init | up | down | doctor
//!   env:    ls | use <name> | env <name> [add|up|down|status|kubeconfig|diagram|rm]
//!   read:   status | kubeconfig | diagram | secret
//!   config: config validate | show

mod config;
mod edit;
mod run;
mod up;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
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
    /// Manage the dabba config
    Config {
        #[command(subcommand)]
        action: ConfigAction,
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
        Command::Doctor => doctor(),
        Command::Up(a) => up::run(&up_options(&cfg, None, a)),
        Command::Down => up::down(&up::DownOptions {
            config: cfg,
            env: None,
        }),
        Command::Status => up::status(&cfg, None),
        Command::Kubeconfig { export } => up::kubeconfig(&cfg, None, export),
        Command::Diagram { mermaid } => up::diagram(&cfg, None, mermaid),
        Command::Secret { action } => match action {
            SecretAction::Ls { path } => up::secret_ls(&cfg, None, path.as_deref()),
            SecretAction::Get { name } => up::secret_get(&cfg, None, &name),
        },
        Command::Ls => up::ls(&cfg),
        Command::Use { name } => edit::use_env(&cfg, &name),
        Command::Init => edit::init(&cfg),
        Command::Env { name, action } => dispatch_env(&cfg, &name, action),
    }
}

fn dispatch_env(config: &Path, name: &str, action: Option<EnvAction>) -> Result<()> {
    let env = Some(name.to_string());
    match action {
        None => up::show(config, name),
        Some(EnvAction::Up(a)) => up::run(&up_options(config, env, a)),
        Some(EnvAction::Down) => up::down(&up::DownOptions {
            config: config.to_path_buf(),
            env,
        }),
        Some(EnvAction::Status) => up::status(config, Some(name)),
        Some(EnvAction::Kubeconfig { export }) => up::kubeconfig(config, Some(name), export),
        Some(EnvAction::Diagram { mermaid }) => up::diagram(config, Some(name), mermaid),
        Some(EnvAction::Add { substrate, domain }) => {
            edit::add_env(config, name, &substrate, domain.as_deref())
        }
        Some(EnvAction::Rm) => edit::rm_env(config, name),
    }
}

fn up_options(config: &Path, env: Option<String>, a: UpArgs) -> up::Options {
    up::Options {
        config: config.to_path_buf(),
        env,
        quickstart_dir: a.quickstart_dir,
        modules_source: a.modules_source,
        gitops_seed: a.gitops_seed,
    }
}

/// Check the day-0 prerequisites are present.
fn doctor() -> Result<()> {
    let tools = ["docker", "kubectl", "tofu"];
    let mut missing = Vec::new();
    for t in tools {
        let ok = on_path(t);
        println!("  {} {}", if ok { "✓" } else { "✗" }, t);
        if !ok {
            missing.push(t);
        }
    }
    if missing.is_empty() {
        println!("✓ preflight ok");
        Ok(())
    } else {
        anyhow::bail!("missing prerequisites: {}", missing.join(", "))
    }
}

/// True if `bin` is an executable file somewhere on PATH.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}
