//! The DabbaConfig schema — the single contract the CLI (day-0), Flux, and the
//! day-2 controller all read. The config file IS a `DabbaConfig` custom resource.
//!
//! A config describes one or more ENVIRONMENTS (`dabba env <name> …`). Each env is
//! a managed boundary that today holds a single cluster (multi-cluster envs come
//! later). Shared platform settings live at `spec`; each env overrides what differs
//! (substrate, and optionally domain/kubeconfig). Secrets never live here — they go
//! OpenBao -> External Secrets.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DabbaConfig {
    pub api_version: String,
    pub kind: String,
    #[serde(default)]
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    #[serde(default = "default_name")]
    pub name: String,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            name: default_name(),
        }
    }
}

fn default_name() -> String {
    "dabba".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Spec {
    pub environments: Vec<Environment>, // [B]
    /// Which env bare `dabba up`/`status`/… act on. Optional when there is exactly one.
    #[serde(default)]
    pub default_environment: Option<String>,
    pub domain: String, // [R] shared default -> cluster-vars ${domain}
    #[serde(default)]
    pub tls: Tls, // [R]
    #[serde(default)]
    pub gateway: Gateway, // [B]
    #[serde(default)]
    pub git: Git, // [B]/[R]
    #[serde(default)]
    pub secrets: Secrets, // [R]
    #[serde(default)]
    pub observability: Observability, // [R]
    #[serde(default)]
    pub alerting: Alerting, // [R]
    #[serde(default)]
    pub use_cases: Vec<String>, // [R]
}

/// One environment: a named managed boundary. The `name` is its identity — it is
/// stamped into cluster-vars as `environment` and (for now, one cluster per env)
/// names the cluster too.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Environment {
    pub name: String,
    pub substrate: Substrate,
    /// Override the shared `spec.domain` for this env.
    #[serde(default)]
    pub domain: Option<String>,
    /// Path to a kubeconfig for an existing cluster (required when substrate=existing).
    #[serde(default)]
    pub kubeconfig: Option<String>,
    /// Substrate-specific knobs (k8sVersion, nodeCount, region…); shape varies per substrate.
    #[serde(default)]
    pub substrate_config: serde_yaml::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Substrate {
    Kind,
    K3d,
    Minikube,
    ScalewayKapsule,
    Eks,
    /// Bring-your-own: skip provisioning and configure the cluster `kubeconfig` points at.
    Existing,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tls {
    #[serde(default)]
    pub issuer: Issuer,
    #[serde(default)]
    pub acme: serde_yaml::Value,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Issuer {
    #[default]
    Selfsigned,
    Acme,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Gateway {
    #[serde(default)]
    pub exposure: Exposure,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exposure {
    #[default]
    Nodeport,
    Loadbalancer,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Git {
    #[serde(default)]
    pub mode: GitMode,
    /// Where `dabba up` clones the gitops content from when no `--gitops-seed` is
    /// given. Defaults to the canonical public dabba-gitops; override for a fork.
    #[serde(default = "default_upstream")]
    pub upstream: String,
    #[serde(default)]
    pub push_mirror: PushMirror,
}

fn default_upstream() -> String {
    "https://github.com/spice-labs-inc/dabba-gitops.git".into()
}

impl Default for Git {
    fn default() -> Self {
        Git {
            mode: GitMode::default(),
            upstream: default_upstream(),
            push_mirror: PushMirror::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitMode {
    #[default]
    InCluster,
    External,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushMirror {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub target: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub backend: SecretBackend,
    #[serde(default)]
    pub openbao: serde_yaml::Value,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretBackend {
    #[default]
    Openbao,
    ScalewaySm,
    AwsSm,
    Onepassword,
    None,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observability {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pipeline: Pipeline,
    #[serde(default)]
    pub backend: ObsBackend,
    #[serde(default)]
    pub settings: serde_yaml::Value,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pipeline {
    #[default]
    Vector,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObsBackend {
    #[default]
    Openobserve,
    Signoz,
    Honeycomb,
    Datadog,
    None,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Alerting {
    #[serde(default = "default_true")]
    pub flux_notifications: bool,
    #[serde(default)]
    pub slack: SlackAlert,
}

// flux notifications default ON even when the whole `alerting` block is absent.
impl Default for Alerting {
    fn default() -> Self {
        Self {
            flux_notifications: true,
            slack: SlackAlert::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SlackAlert {
    #[serde(default)]
    pub enabled: bool,
}

/// The flattened view of one environment that `up`/`down`/`status` act on.
pub struct ResolvedEnv {
    pub name: String,
    pub substrate: Substrate,
    pub kubeconfig: Option<String>,
    pub domain: String,
    pub issuer: Issuer,
    pub exposure: Exposure,
    /// ACME account email (from spec.tls.acme.email); empty when not using ACME.
    pub acme_email: String,
    pub substrate_config: serde_yaml::Value,
}

impl ResolvedEnv {
    /// Read a string field out of `substrate_config` (the free-form per-substrate
    /// knobs), falling back to `default` when it is absent or not a string.
    pub fn substrate_str(&self, key: &str, default: &str) -> String {
        self.substrate_config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    /// Read a string-list field out of `substrate_config` (empty when absent).
    pub fn substrate_list(&self, key: &str) -> Vec<String> {
        self.substrate_config
            .get(key)
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl DabbaConfig {
    /// Parse and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: DabbaConfig =
            serde_yaml::from_str(&text).context("parsing config (invalid field or value)")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Cross-field checks beyond what serde's typed parse already enforces.
    pub fn validate(&self) -> Result<()> {
        if self.kind != "DabbaConfig" {
            bail!("kind must be \"DabbaConfig\", got {:?}", self.kind);
        }
        if !self.api_version.starts_with("dabba.spicelabs.io/") {
            bail!(
                "apiVersion must be under dabba.spicelabs.io/, got {:?}",
                self.api_version
            );
        }
        if self.spec.environments.is_empty() {
            bail!("spec.environments must list at least one environment");
        }
        let mut seen = HashSet::new();
        for e in &self.spec.environments {
            if e.name.trim().is_empty() {
                bail!("every environment needs a name");
            }
            if !seen.insert(e.name.as_str()) {
                bail!("duplicate environment name {:?}", e.name);
            }
            if e.substrate == Substrate::Existing && e.kubeconfig.is_none() {
                bail!(
                    "environment {:?}: substrate=existing requires kubeconfig (path to its kubeconfig)",
                    e.name
                );
            }
            let domain = e.domain.as_deref().unwrap_or(&self.spec.domain);
            if domain.trim().is_empty() {
                bail!(
                    "environment {:?}: domain is required (set spec.domain or the env's domain)",
                    e.name
                );
            }
        }
        if let Some(d) = &self.spec.default_environment {
            if !self.spec.environments.iter().any(|e| &e.name == d) {
                bail!(
                    "defaultEnvironment {:?} is not one of: {}",
                    d,
                    self.env_names().join(", ")
                );
            }
        }
        if self.spec.tls.issuer == Issuer::Acme && self.spec.gateway.exposure == Exposure::Nodeport
        {
            bail!("tls.issuer=acme needs gateway.exposure=loadbalancer (ACME http/dns-01 needs real ingress)");
        }
        Ok(())
    }

    /// The env bare commands act on: `defaultEnvironment`, or the only env if there is one.
    pub fn default_env_name(&self) -> Result<&str> {
        if let Some(d) = &self.spec.default_environment {
            return Ok(d.as_str());
        }
        match self.spec.environments.as_slice() {
            [one] => Ok(one.name.as_str()),
            [] => bail!("no environments defined"),
            _ => bail!("multiple environments — set spec.defaultEnvironment or pass an env name"),
        }
    }

    pub fn env_names(&self) -> Vec<&str> {
        self.spec
            .environments
            .iter()
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Resolve a named env (or the default when `name` is None) into a flat view.
    pub fn resolve(&self, name: Option<&str>) -> Result<ResolvedEnv> {
        let name = match name {
            Some(n) => n.to_string(),
            None => self.default_env_name()?.to_string(),
        };
        let env = self
            .spec
            .environments
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| {
                anyhow!(
                    "no environment named {:?} (have: {})",
                    name,
                    self.env_names().join(", ")
                )
            })?;
        Ok(ResolvedEnv {
            name: env.name.clone(),
            substrate: env.substrate,
            kubeconfig: env.kubeconfig.clone(),
            domain: env
                .domain
                .clone()
                .unwrap_or_else(|| self.spec.domain.clone()),
            issuer: self.spec.tls.issuer,
            exposure: self.spec.gateway.exposure,
            acme_email: self
                .spec
                .tls
                .acme
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            substrate_config: env.substrate_config.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = r#"
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
metadata:
  name: dabba
spec:
  domain: localtest.me
  defaultEnvironment: kind
  environments:
    - { name: kind, substrate: kind }
    - { name: k3d, substrate: k3d }
    - { name: minikube, substrate: minikube }
  useCases:
    - demo
"#;

    #[test]
    fn parses_and_validates_local() {
        let cfg: DabbaConfig = serde_yaml::from_str(LOCAL).unwrap();
        cfg.validate().unwrap();
        let env = cfg.resolve(None).unwrap(); // default
        assert_eq!(env.name, "kind");
        assert_eq!(env.substrate, Substrate::Kind);
        assert_eq!(env.issuer, Issuer::Selfsigned); // shared default
        assert_eq!(env.domain, "localtest.me"); // shared default
        assert!(cfg.spec.alerting.flux_notifications); // default true
    }

    #[test]
    fn resolve_picks_named_env() {
        let cfg: DabbaConfig = serde_yaml::from_str(LOCAL).unwrap();
        assert_eq!(cfg.resolve(Some("k3d")).unwrap().substrate, Substrate::K3d);
        assert!(cfg.resolve(Some("nope")).is_err());
    }

    #[test]
    fn rejects_unknown_substrate() {
        let bad = LOCAL.replace("substrate: kind", "substrate: openstack");
        assert!(serde_yaml::from_str::<DabbaConfig>(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = LOCAL.replace(
            "domain: localtest.me",
            "domain: localtest.me\n  notAField: x",
        );
        assert!(serde_yaml::from_str::<DabbaConfig>(&bad).is_err());
    }

    #[test]
    fn rejects_duplicate_env_names() {
        let bad = LOCAL.replace(
            "{ name: k3d, substrate: k3d }",
            "{ name: kind, substrate: k3d }",
        );
        let cfg: DabbaConfig = serde_yaml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_default_environment() {
        let bad = LOCAL.replace("defaultEnvironment: kind", "defaultEnvironment: staging");
        let cfg: DabbaConfig = serde_yaml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn existing_requires_kubeconfig() {
        let bad = LOCAL.replace(
            "{ name: minikube, substrate: minikube }",
            "{ name: byo, substrate: existing }",
        );
        let cfg: DabbaConfig = serde_yaml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());

        let ok = LOCAL.replace(
            "{ name: minikube, substrate: minikube }",
            "{ name: byo, substrate: existing, kubeconfig: ~/.kube/config }",
        );
        let cfg: DabbaConfig = serde_yaml::from_str(&ok).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.resolve(Some("byo")).unwrap().substrate,
            Substrate::Existing
        );
    }

    #[test]
    fn rejects_acme_with_nodeport() {
        let bad = LOCAL.replace(
            "domain: localtest.me",
            "domain: x.example.com\n  tls:\n    issuer: acme",
        );
        let cfg: DabbaConfig = serde_yaml::from_str(&bad).unwrap();
        assert!(cfg.validate().is_err());
    }
}
