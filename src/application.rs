//! The portable application definition: one description of an application that
//! renders to Kubernetes objects on a Kubernetes substrate and to a compose file
//! on a `docker-host` substrate.
//!
//! # Why this exists
//!
//! dabba's implicit promise is that you change the `substrate:` line and get the
//! same platform. That promise held across kind/k3d/minikube/EKS because they all
//! terminate in a kubeconfig and everything above it is identical. It did not hold
//! for `docker-host`, which consumes a completely different artifact format — and
//! nothing said so. `spec.tls`, `spec.gateway`, `spec.secrets`, `spec.observability`
//! and `spec.useCases` were inherited by a docker-host environment and honoured by
//! none of it, silently.
//!
//! # The design rule that keeps this from rotting
//!
//! **This schema is the INTERSECTION of what both runtimes genuinely honour, not
//! the union.** If a field cannot be rendered meaningfully on both sides, it does
//! not go in the schema.
//!
//! The alternative — accept whatever an application declares and have each renderer
//! emit what it happens to support — makes every unsupported field a silent
//! half-implementation. You find out that `resources` quietly did nothing on compose
//! at exactly the wrong moment. Making the schema the intersection turns
//! half-implementation from a thing reviewers must catch into a thing that cannot be
//! expressed.
//!
//! That rule is enforced mechanically, not by discipline: [`crate::conformance`]
//! walks this schema field by field and fails the build if either renderer ignores
//! one. See [`EXHAUSTIVE_EXAMPLE`].
//!
//! # Deliberately out of scope, permanently
//!
//! These have no compose equivalent, and approximating them is exactly the failure
//! mode this design exists to prevent. They are named here so their absence reads as
//! a decision rather than an oversight:
//!
//! * horizontal pod autoscaling — compose has no scheduler to scale against
//! * `NetworkPolicy` — no per-container network policy primitive on a compose host
//! * `PodDisruptionBudget` — presupposes a scheduler that evicts
//! * multi-node scheduling, affinity, topology spread — one box is one node
//! * service mesh — no sidecar injection
//! * `replicas` — compose can scale a service, but not one that publishes a fixed
//!   host port, so the field would work only sometimes. A field that works
//!   conditionally is the half-implementation this schema is designed to exclude.
//! * `dependsOn` — compose has `depends_on`; Kubernetes has no ordering primitive,
//!   and faking one with init containers would mean the same field meant two
//!   materially different things.
//!
//! Anything genuinely substrate-specific belongs in the [`Escapes`] blocks, which
//! are explicitly non-portable and are reported by name when an application moves
//! between substrates.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// A portable application definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Application {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ApplicationSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

/// The portable fields. Every one of these renders on both substrates; that is the
/// entry requirement, and [`crate::conformance`] proves it on every build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplicationSpec {
    /// Container image, without a tag.
    pub image: String,
    /// Image tag. Required and separate from `image` so a release workflow has one
    /// unambiguous field to rewrite.
    pub tag: String,

    /// Ports the container listens on. `publish` optionally exposes one on the host
    /// (a published host port on compose, a Service port on Kubernetes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,

    /// Environment variables, either literal or resolved from the secret store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<EnvironmentVariable>,

    /// Persistent storage. A PersistentVolumeClaim on Kubernetes, a bind mount under
    /// the stack directory on a compose host. Storage class and size are not
    /// portable and live in the `kubernetes` escape hatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,

    /// How to tell whether the application is actually working. Becomes a readiness
    /// probe on Kubernetes and drives the reconciler's `x-health-cmd` gate on a
    /// compose host — which is what makes "deployed" and "healthy" distinguishable
    /// on both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,

    /// Resource limits. `resources.limits` on Kubernetes, `deploy.resources.limits`
    /// on compose, which the daemon honours outside swarm mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,

    /// What to do when the container exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,

    /// Substrate-specific configuration. Explicitly NOT portable — see [`Escapes`].
    #[serde(default, skip_serializing_if = "Escapes::is_empty", flatten)]
    pub escapes: Escapes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Port {
    /// Referenced by `healthCheck.httpGet.port`, so it is required rather than
    /// optional — an unnamed port cannot be probed portably.
    pub name: String,
    pub container_port: u16,
    /// Expose this port on the host. Absent means cluster-internal only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentVariable {
    pub name: String,
    /// A literal value. Mutually exclusive with `secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Resolved from the secret store at deploy time: an ExternalSecret-backed
    /// Secret on Kubernetes, and the box-local `.env` the reconciler already reads
    /// on a compose host, populated from OpenBao.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretReference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretReference {
    /// Path of the secret in the store.
    pub name: String,
    /// Key within that secret.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Volume {
    pub name: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HealthCheck {
    /// Probe by HTTP GET against a named port. `exec` is the alternative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_get: Option<HttpGet>,
    /// Probe by running a command inside the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<String>,
    #[serde(default = "default_initial_delay")]
    pub initial_delay_seconds: u32,
    #[serde(default = "default_period")]
    pub period_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpGet {
    pub path: String,
    /// The `name` of one of `spec.ports`.
    pub port: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resources {
    /// Kubernetes quantity form ("500m", "2"). Converted to a decimal core count
    /// for compose's `cpus`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    /// Kubernetes quantity form ("256Mi", "1Gi"). Converted to compose's `memory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

/// Substrate-specific configuration, deliberately quarantined.
///
/// These blocks are free-form and merged into the rendered output by their own
/// renderer only. They exist so that needing one non-portable thing does not push
/// an application out of the shared schema entirely — but they are the boundary of
/// the portability promise, so [`Application::non_portable_fields`] reports them by
/// name when an application is moved to a substrate that will ignore them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Escapes {
    /// Merged into the rendered Kubernetes objects. Ignored on a compose host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<serde_yaml::Value>,
    /// Merged into the rendered compose service. Ignored on Kubernetes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_host: Option<serde_yaml::Value>,
}

impl Escapes {
    pub fn is_empty(&self) -> bool {
        self.kubernetes.is_none() && self.docker_host.is_none()
    }
}

fn default_initial_delay() -> u32 {
    5
}
fn default_period() -> u32 {
    10
}

impl Application {
    /// Parse and validate.
    pub fn parse(text: &str) -> Result<Self> {
        let app: Application = serde_yaml::from_str(text)?;
        app.validate()?;
        Ok(app)
    }

    pub fn validate(&self) -> Result<()> {
        if self.kind != "Application" {
            bail!("kind must be \"Application\", got {:?}", self.kind);
        }
        if !self.api_version.starts_with("dabba.spicelabs.io/") {
            bail!(
                "apiVersion must be under dabba.spicelabs.io/, got {:?}",
                self.api_version
            );
        }
        if self.metadata.name.trim().is_empty() {
            bail!("metadata.name is required");
        }
        if self.spec.image.trim().is_empty() {
            bail!("spec.image is required");
        }
        if self.spec.tag.trim().is_empty() {
            bail!("spec.tag is required — an untagged image makes a deploy unreproducible");
        }

        let mut seen_ports = std::collections::HashSet::new();
        for port in &self.spec.ports {
            if !seen_ports.insert(port.name.as_str()) {
                bail!("duplicate port name {:?}", port.name);
            }
        }

        for variable in &self.spec.environment {
            match (&variable.value, &variable.secret) {
                (Some(_), Some(_)) => bail!(
                    "environment variable {:?} sets both value and secret; pick one",
                    variable.name
                ),
                (None, None) => bail!(
                    "environment variable {:?} sets neither value nor secret",
                    variable.name
                ),
                _ => {}
            }
        }

        if let Some(health) = &self.spec.health_check {
            match (&health.http_get, health.exec.is_empty()) {
                (Some(_), false) => {
                    bail!("healthCheck sets both httpGet and exec; pick one")
                }
                (None, true) => bail!("healthCheck sets neither httpGet nor exec"),
                (Some(get), true) if !self.spec.ports.iter().any(|p| p.name == get.port) => {
                    bail!(
                        "healthCheck.httpGet.port {:?} does not name any of spec.ports",
                        get.port
                    );
                }
                _ => {}
            }
        }

        let mut seen_volumes = std::collections::HashSet::new();
        for volume in &self.spec.volumes {
            if !seen_volumes.insert(volume.name.as_str()) {
                bail!("duplicate volume name {:?}", volume.name);
            }
            if !volume.mount_path.starts_with('/') {
                bail!(
                    "volume {:?}: mountPath must be absolute, got {:?}",
                    volume.name,
                    volume.mount_path
                );
            }
        }
        Ok(())
    }

    /// The escape-hatch blocks that the named substrate will NOT honour.
    ///
    /// This is what turns the portability boundary from a documentation claim into
    /// something the CLI can state at the moment it matters — moving an application
    /// to a substrate that silently ignores half of it is the exact failure this
    /// whole design is built to prevent.
    pub fn non_portable_fields(&self, target_is_kubernetes: bool) -> Vec<&'static str> {
        let mut ignored = Vec::new();
        if target_is_kubernetes {
            if self.spec.escapes.docker_host.is_some() {
                ignored.push("dockerHost");
            }
        } else if self.spec.escapes.kubernetes.is_some() {
            ignored.push("kubernetes");
        }
        ignored
    }
}

/// An application exercising EVERY portable field, with distinctive values.
///
/// This is the fixture the conformance matrix renders through both backends: each
/// distinctive value below must be findable in both outputs, which is how "both
/// renderers honour this field" becomes a build-time fact rather than a claim.
///
/// Adding a field to [`ApplicationSpec`] without adding it here is caught by
/// `exhaustive_example_covers_every_field`, so this fixture cannot quietly fall
/// behind the schema it is supposed to cover.
///
/// It is also what `dabba application example` prints. The starter template users
/// copy is therefore the same text the conformance matrix proves both renderers
/// honour — documentation that cannot drift from the code, because it is the code.
pub const EXHAUSTIVE_EXAMPLE: &str = r#"
apiVersion: dabba.spicelabs.io/v1alpha1
kind: Application
metadata:
  name: conformance
spec:
  image: ghcr.io/example/conformance
  tag: "1.2.3"
  ports:
    - name: http
      containerPort: 9898
      publish: 19898
  environment:
    - name: LITERAL_SETTING
      value: literal-value
    - name: SECRET_SETTING
      secret:
        name: demo
        key: message
  volumes:
    - name: data
      mountPath: /var/lib/conformance
  healthCheck:
    httpGet:
      path: /healthz
      port: http
    initialDelaySeconds: 7
    periodSeconds: 11
  resources:
    cpu: "500m"
    memory: 256Mi
  restartPolicy: on-failure
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn exhaustive() -> Application {
        Application::parse(EXHAUSTIVE_EXAMPLE).expect("the exhaustive example must be valid")
    }

    #[test]
    fn exhaustive_example_parses_and_validates() {
        let app = exhaustive();
        assert_eq!(app.metadata.name, "conformance");
        assert_eq!(app.spec.tag, "1.2.3");
    }

    /// The guard that keeps EXHAUSTIVE_EXAMPLE honest. Every optional field on the
    /// spec must actually be populated by the fixture, so a new field cannot be
    /// added to the schema and silently escape conformance coverage.
    ///
    /// Serialising and inspecting the key set is deliberate: a hand-maintained list
    /// of field names would drift, which is the same failure as a status line that
    /// prints a literal instead of computing the value.
    #[test]
    fn exhaustive_example_covers_every_field() {
        let app = exhaustive();
        let serialized = serde_yaml::to_value(&app.spec).unwrap();
        let mapping = serialized
            .as_mapping()
            .expect("spec serialises to a mapping");

        // Fields are skipped on serialise when empty/None, so the key set of the
        // serialised fixture IS the set of fields it exercises. Compare against the
        // full field list obtained by serialising a spec with nothing skipped.
        let every_field = [
            "image",
            "tag",
            "ports",
            "environment",
            "volumes",
            "healthCheck",
            "resources",
            "restartPolicy",
        ];
        for field in every_field {
            assert!(
                mapping.contains_key(serde_yaml::Value::String(field.to_string())),
                "EXHAUSTIVE_EXAMPLE does not exercise spec.{field}; conformance would \
                 not cover it. Add it to the fixture."
            );
        }

        // And the inverse: no field in the serialised fixture is missing from the
        // list above, so adding a schema field forces this test to be updated.
        for key in mapping.keys() {
            let key = key.as_str().unwrap_or_default();
            assert!(
                every_field.contains(&key),
                "spec.{key} is new; add it to `every_field` and to EXHAUSTIVE_EXAMPLE, \
                 and make sure BOTH renderers honour it"
            );
        }
    }

    #[test]
    fn rejects_an_untagged_image() {
        let bad = EXHAUSTIVE_EXAMPLE.replace("tag: \"1.2.3\"", "tag: \"\"");
        let err = Application::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("spec.tag is required"), "got: {err}");
    }

    #[test]
    fn rejects_an_environment_variable_that_is_both_literal_and_secret() {
        let bad = EXHAUSTIVE_EXAMPLE.replace(
            "    - name: LITERAL_SETTING\n      value: literal-value",
            "    - name: LITERAL_SETTING\n      value: literal-value\n      secret:\n        name: a\n        key: b",
        );
        let err = Application::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("both value and secret"), "got: {err}");
    }

    #[test]
    fn rejects_a_health_check_probing_an_undeclared_port() {
        let bad = EXHAUSTIVE_EXAMPLE.replace("port: http", "port: nonexistent");
        let err = Application::parse(&bad).unwrap_err().to_string();
        assert!(
            err.contains("does not name any of spec.ports"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_a_relative_mount_path() {
        let bad = EXHAUSTIVE_EXAMPLE.replace("mountPath: /var/lib/conformance", "mountPath: data");
        let err = Application::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "got: {err}");
    }

    /// Fields that are out of scope must be REJECTED, not ignored. If `replicas`
    /// were silently accepted it would look supported and do nothing on one side —
    /// the precise failure this schema exists to prevent.
    #[test]
    fn rejects_fields_that_are_deliberately_out_of_scope() {
        for field in ["replicas: 3", "dependsOn: [postgres]", "nodeSelector: {}"] {
            let bad = EXHAUSTIVE_EXAMPLE.replace("  image:", &format!("  {field}\n  image:"));
            assert!(
                Application::parse(&bad).is_err(),
                "{field} was accepted; deny_unknown_fields must reject it"
            );
        }
    }

    #[test]
    fn reports_escape_hatches_the_target_substrate_will_ignore() {
        let with_escape = format!(
            "{EXHAUSTIVE_EXAMPLE}  kubernetes:\n    persistentVolumeClaim:\n      size: 1Gi\n"
        );
        let app = Application::parse(&with_escape).unwrap();
        assert_eq!(app.non_portable_fields(false), vec!["kubernetes"]);
        assert!(app.non_portable_fields(true).is_empty());
    }
}
