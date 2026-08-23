//! The backend abstraction. dabba's per-environment verbs —
//! `up`/`down`/`status`/`diagram`/`secret ls`/`secret get` — dispatch through the
//! [`Backend`] trait so the CLI does not have to know how an environment is run.
//!
//! Two implementations exist today:
//!   * [`kubernetes::KubernetesBackend`] — the original day-0 flow (tofu provisions
//!     a cluster, then Forgejo + the Flux Operator reconcile the platform).
//!   * [`docker::DockerBackend`] — the bare-OS docker-compose path, which drives the
//!     gitops reconciler under `backends/docker/` (a per-box loop that converges the
//!     host's compose stacks to a git repo, the Flux equivalent for compose hosts).
//!
//! The environment's `substrate` selects the implementation (see [`select`]): a
//! `docker-host` substrate picks the `DockerBackend`, everything else the
//! `KubernetesBackend`.
//!
//! Backend-neutral helpers (the per-env workdir, the local secret-zero stash, the
//! env-listing/show commands) live in [`common`] so both backends share them rather
//! than duplicating; process helpers stay in [`crate::run`].

pub mod common;
pub mod docker;
pub mod kubernetes;
pub mod reconciler_assets;

use crate::config::{DabbaConfig, Substrate};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Options for `up`. Shared by both backends; some fields apply to only one
/// (`quickstart_dir`/`modules_source` are Kubernetes-only, `gitops_seed` is used by
/// both — as the local gitops content to seed).
pub struct Options {
    pub config: PathBuf,
    /// Which environment to act on; None → the config's default.
    pub env: Option<String>,
    /// (Kubernetes) Directory holding the quickstart tofu (01-cluster, 02-bootstrap).
    pub quickstart_dir: PathBuf,
    /// (Kubernetes) Override module sources with a local path (dev).
    pub modules_source: Option<String>,
    /// Local gitops content to seed from. Kubernetes seeds it into Forgejo; the
    /// DockerBackend clones/uses it as the reconciler's gitops repo. None → the
    /// config's `git.upstream`.
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
/// A `docker-host` substrate selects the [`docker::DockerBackend`]; every other
/// substrate keeps the original [`kubernetes::KubernetesBackend`].
pub fn select(config: &Path, env_name: Option<&str>) -> Result<Box<dyn Backend>> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;
    Ok(match env.substrate {
        Substrate::DockerHost => Box::new(docker::DockerBackend),
        _ => Box::new(kubernetes::KubernetesBackend),
    })
}
