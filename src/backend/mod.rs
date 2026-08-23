//! The backend abstraction. dabba's per-environment verbs —
//! `up`/`down`/`status`/`diagram`/`secret ls`/`secret get` — dispatch through the
//! [`Backend`] trait so the CLI does not have to know how an environment is run.
//!
//! One implementation exists today: [`kubernetes::KubernetesBackend`], the original
//! day-0 flow (tofu provisions a cluster, then Forgejo + the Flux Operator reconcile
//! the platform). The trait exists so a second, non-Kubernetes backend can be added
//! without the CLI learning about it.
//!
//! Backend-neutral helpers (the per-env workdir, the local secret-zero stash, the
//! env-listing/show commands) live in [`common`] so backends share them rather than
//! duplicating; process helpers stay in [`crate::run`].

pub mod common;
pub mod kubernetes;

use crate::config::DabbaConfig;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Options for `up`. Shared by every backend; some fields apply to only one
/// (`quickstart_dir`/`modules_source` are Kubernetes-only).
pub struct Options {
    pub config: PathBuf,
    /// Which environment to act on; None → the config's default.
    pub env: Option<String>,
    /// (Kubernetes) Directory holding the quickstart tofu (01-cluster, 02-bootstrap).
    pub quickstart_dir: PathBuf,
    /// (Kubernetes) Override module sources with a local path (dev).
    pub modules_source: Option<String>,
    /// Local gitops content to seed from. Kubernetes seeds it into Forgejo.
    /// None → the config's `git.upstream`.
    pub gitops_seed: Option<PathBuf>,
}

/// Options for `down`.
pub struct DownOptions {
    pub config: PathBuf,
    pub env: Option<String>,
}

/// The verbs the CLI dispatches per environment. Signatures match the original
/// free functions in `up.rs` so the extraction is behavior-preserving.
pub trait Backend {
    fn up(&self, opts: &Options) -> Result<()>;
    fn down(&self, opts: &DownOptions) -> Result<()>;
    fn status(&self, config: &Path, env_name: Option<&str>) -> Result<()>;
    fn diagram(&self, config: &Path, env_name: Option<&str>, mermaid: bool) -> Result<()>;
    fn secret_ls(&self, config: &Path, env_name: Option<&str>, path: Option<&str>) -> Result<()>;
    fn secret_get(&self, config: &Path, env_name: Option<&str>, name: &str) -> Result<()>;
}

/// Pick the backend implementation for an environment from its resolved substrate.
/// Every substrate is Kubernetes today; the match is the seam a second backend
/// plugs into.
pub fn select(config: &Path, env_name: Option<&str>) -> Result<Box<dyn Backend>> {
    let cfg = DabbaConfig::load(config)?;
    let _env = cfg.resolve(env_name)?;
    Ok(Box::new(kubernetes::KubernetesBackend))
}
