//! Render an [`Application`] to the Kubernetes objects the Flux path consumes.
//!
//! A Deployment, a Service for any declared port, and a PersistentVolumeClaim per
//! volume — emitted as a multi-document YAML stream, which is what a kustomization
//! takes as a resource file.
//!
//! Two shapes here are dictated by the portability contract rather than by
//! Kubernetes taste:
//!
//! * Secret-backed environment variables render as `secretKeyRef` against a Secret
//!   named for the application. On this substrate External Secrets populates it from
//!   OpenBao; on a compose host the same declaration resolves from the box-local
//!   `.env`. One field, two mechanisms, same meaning.
//! * The health check becomes a readiness probe, not a liveness probe. It is the
//!   answer to "is this working", which is the same question the reconciler's
//!   `x-health-cmd` gate answers on a compose host. Wiring it to liveness would make
//!   a failing probe restart the container on one substrate and merely report on the
//!   other.

use crate::application::{Application, EnvironmentVariable, HealthCheck};
use crate::render::string_map;
use anyhow::Result;
use serde_yaml::{Mapping, Value};

fn string(s: impl Into<String>) -> Value {
    Value::String(s.into())
}

fn number(n: u32) -> Value {
    Value::Number(n.into())
}

/// Render the application as a multi-document Kubernetes manifest stream.
pub fn render(app: &Application) -> Result<String> {
    let name = &app.metadata.name;
    let mut documents = vec![deployment(app)?];
    if !app.spec.ports.is_empty() {
        documents.push(service(app));
    }
    for volume in &app.spec.volumes {
        documents.push(persistent_volume_claim(app, &volume.name));
    }

    let mut out = format!(
        "# Rendered by dabba from the portable Application definition for {name}.\n\
         # Edit the Application, not this file — the reconciler overwrites it.\n"
    );
    for document in documents {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&document)?);
    }
    Ok(out)
}

fn deployment(app: &Application) -> Result<Value> {
    let name = &app.metadata.name;
    let mut container = Mapping::new();
    container.insert(string("name"), string(name.clone()));
    container.insert(
        string("image"),
        string(format!("{}:{}", app.spec.image, app.spec.tag)),
    );

    if !app.spec.ports.is_empty() {
        let ports: Vec<Value> = app
            .spec
            .ports
            .iter()
            .map(|port| {
                let mut entry = Mapping::new();
                entry.insert(string("name"), string(port.name.clone()));
                entry.insert(string("containerPort"), number(port.container_port as u32));
                Value::Mapping(entry)
            })
            .collect();
        container.insert(string("ports"), Value::Sequence(ports));
    }

    if !app.spec.environment.is_empty() {
        let variables: Vec<Value> = app
            .spec
            .environment
            .iter()
            .map(|variable| environment_variable(name, variable))
            .collect();
        container.insert(string("env"), Value::Sequence(variables));
    }

    if !app.spec.volumes.is_empty() {
        let mounts: Vec<Value> = app
            .spec
            .volumes
            .iter()
            .map(|volume| {
                let mut mount = Mapping::new();
                mount.insert(string("name"), string(volume.name.clone()));
                mount.insert(string("mountPath"), string(volume.mount_path.clone()));
                Value::Mapping(mount)
            })
            .collect();
        container.insert(string("volumeMounts"), Value::Sequence(mounts));
    }

    if let Some(resources) = &app.spec.resources {
        let mut limits = Mapping::new();
        if let Some(cpu) = &resources.cpu {
            limits.insert(string("cpu"), string(cpu.clone()));
        }
        if let Some(memory) = &resources.memory {
            limits.insert(string("memory"), string(memory.clone()));
        }
        if !limits.is_empty() {
            let mut wrapper = Mapping::new();
            wrapper.insert(string("limits"), Value::Mapping(limits));
            container.insert(string("resources"), Value::Mapping(wrapper));
        }
    }

    if let Some(health) = &app.spec.health_check {
        container.insert(string("readinessProbe"), readiness_probe(health));
    }

    let mut pod_spec = Mapping::new();
    pod_spec.insert(
        string("containers"),
        Value::Sequence(vec![Value::Mapping(container)]),
    );
    if !app.spec.volumes.is_empty() {
        let volumes: Vec<Value> = app
            .spec
            .volumes
            .iter()
            .map(|volume| {
                let mut claim = Mapping::new();
                claim.insert(
                    string("claimName"),
                    string(format!("{name}-{}", volume.name)),
                );
                let mut entry = Mapping::new();
                entry.insert(string("name"), string(volume.name.clone()));
                entry.insert(string("persistentVolumeClaim"), Value::Mapping(claim));
                Value::Mapping(entry)
            })
            .collect();
        pod_spec.insert(string("volumes"), Value::Sequence(volumes));
    }

    let selector = string_map([("app", name.as_str())]);
    let mut template = Mapping::new();
    template.insert(
        string("metadata"),
        map([("labels", Value::Mapping(selector.clone()))]),
    );
    template.insert(string("spec"), Value::Mapping(pod_spec));

    let mut spec = Mapping::new();
    spec.insert(
        string("selector"),
        map([("matchLabels", Value::Mapping(selector))]),
    );
    spec.insert(string("template"), Value::Mapping(template));

    let mut deployment = Mapping::new();
    deployment.insert(string("apiVersion"), string("apps/v1"));
    deployment.insert(string("kind"), string("Deployment"));
    deployment.insert(string("metadata"), map([("name", string(name.clone()))]));
    deployment.insert(string("spec"), Value::Mapping(spec));

    // The kubernetes escape hatch merges last so it can override anything above.
    // DEEP merge: a shallow insert let an overlay setting `spec.template` replace
    // the whole `spec`, selector and all, producing a Deployment the API server
    // rejects. See `crate::render::deep_merge`.
    let rendered = Value::Mapping(deployment);
    Ok(match &app.spec.kubernetes {
        Some(escape) => crate::render::deep_merge(rendered, escape.clone()),
        None => rendered,
    })
}

fn environment_variable(application: &str, variable: &EnvironmentVariable) -> Value {
    let mut entry = Mapping::new();
    entry.insert(string("name"), string(variable.name.clone()));
    match variable {
        EnvironmentVariable {
            value: Some(value), ..
        } => {
            entry.insert(string("value"), string(value.clone()));
        }
        EnvironmentVariable {
            secret: Some(secret),
            ..
        } => {
            let mut key_ref = Mapping::new();
            key_ref.insert(
                string("name"),
                string(format!("{application}-{}", secret.name)),
            );
            key_ref.insert(string("key"), string(secret.key.clone()));
            entry.insert(
                string("valueFrom"),
                map([("secretKeyRef", Value::Mapping(key_ref))]),
            );
        }
        _ => {}
    }
    Value::Mapping(entry)
}

fn readiness_probe(health: &HealthCheck) -> Value {
    let mut probe = Mapping::new();
    if let Some(http) = &health.http_get {
        let mut get = Mapping::new();
        get.insert(string("path"), string(http.path.clone()));
        get.insert(string("port"), string(http.port.clone()));
        probe.insert(string("httpGet"), Value::Mapping(get));
    } else {
        let command: Vec<Value> = health.exec.iter().map(|c| string(c.clone())).collect();
        probe.insert(string("exec"), map([("command", Value::Sequence(command))]));
    }
    probe.insert(
        string("initialDelaySeconds"),
        number(health.initial_delay_seconds),
    );
    probe.insert(string("periodSeconds"), number(health.period_seconds));
    Value::Mapping(probe)
}

fn service(app: &Application) -> Value {
    let name = &app.metadata.name;
    let ports: Vec<Value> = app
        .spec
        .ports
        .iter()
        .map(|port| {
            let mut entry = Mapping::new();
            entry.insert(string("name"), string(port.name.clone()));
            // publish is the port callers use; unpublished ports are addressed by
            // their container port, which is the compose behaviour too.
            entry.insert(
                string("port"),
                number(port.publish.unwrap_or(port.container_port) as u32),
            );
            entry.insert(string("targetPort"), string(port.name.clone()));
            Value::Mapping(entry)
        })
        .collect();

    let mut spec = Mapping::new();
    spec.insert(
        string("selector"),
        Value::Mapping(string_map([("app", name.as_str())])),
    );
    spec.insert(string("ports"), Value::Sequence(ports));

    let mut service = Mapping::new();
    service.insert(string("apiVersion"), string("v1"));
    service.insert(string("kind"), string("Service"));
    service.insert(string("metadata"), map([("name", string(name.clone()))]));
    service.insert(string("spec"), Value::Mapping(spec));
    Value::Mapping(service)
}

fn persistent_volume_claim(app: &Application, volume: &str) -> Value {
    let name = format!("{}-{volume}", app.metadata.name);
    let mut resources = Mapping::new();
    // Size is not portable — a compose bind mount has none — so it lives in the
    // kubernetes escape hatch. This is the floor a claim needs to be valid.
    resources.insert(
        string("requests"),
        Value::Mapping(string_map([("storage", "1Gi")])),
    );

    let mut spec = Mapping::new();
    spec.insert(
        string("accessModes"),
        Value::Sequence(vec![string("ReadWriteOnce")]),
    );
    spec.insert(string("resources"), Value::Mapping(resources));

    let mut claim = Mapping::new();
    claim.insert(string("apiVersion"), string("v1"));
    claim.insert(string("kind"), string("PersistentVolumeClaim"));
    claim.insert(string("metadata"), map([("name", string(name))]));
    claim.insert(string("spec"), Value::Mapping(spec));
    Value::Mapping(claim)
}

fn map<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut mapping = Mapping::new();
    for (key, value) in pairs {
        mapping.insert(string(key), value);
    }
    Value::Mapping(mapping)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::EXHAUSTIVE_EXAMPLE;

    fn documents() -> Vec<Value> {
        let app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        let text = render(&app).unwrap();
        serde_yaml::Deserializer::from_str(&text)
            .map(|d| Value::deserialize(d).unwrap())
            .filter(|v| !v.is_null())
            .collect()
    }

    use serde::Deserialize;

    fn kind<'a>(documents: &'a [Value], kind: &str) -> &'a Value {
        documents
            .iter()
            .find(|d| d.get("kind").and_then(|k| k.as_str()) == Some(kind))
            .unwrap_or_else(|| panic!("no {kind} in the rendered output"))
    }

    #[test]
    fn renders_a_deployment_service_and_claim() {
        let documents = documents();
        assert_eq!(documents.len(), 3, "expected Deployment, Service, PVC");
        kind(&documents, "Deployment");
        kind(&documents, "Service");
        kind(&documents, "PersistentVolumeClaim");
    }

    #[test]
    fn renders_image_with_its_tag() {
        let documents = documents();
        let text = serde_yaml::to_string(kind(&documents, "Deployment")).unwrap();
        assert!(text.contains("ghcr.io/example/conformance:1.2.3"));
    }

    #[test]
    fn renders_literal_environment_and_indirects_secrets() {
        let documents = documents();
        let text = serde_yaml::to_string(kind(&documents, "Deployment")).unwrap();
        assert!(text.contains("LITERAL_SETTING"));
        assert!(text.contains("literal-value"));
        assert!(text.contains("secretKeyRef"));
        // The secret's value is never inlined, exactly as on compose.
        assert!(text.contains("key: message"));
    }

    /// The probe must be readiness, not liveness: it answers "is this working",
    /// which is the same question the compose health gate answers. As liveness it
    /// would restart the container on one substrate and only report on the other.
    #[test]
    fn health_check_becomes_a_readiness_probe() {
        let documents = documents();
        let text = serde_yaml::to_string(kind(&documents, "Deployment")).unwrap();
        assert!(text.contains("readinessProbe"));
        assert!(!text.contains("livenessProbe"));
        assert!(text.contains("initialDelaySeconds: 7"));
        assert!(text.contains("periodSeconds: 11"));
    }

    #[test]
    fn renders_resource_limits_in_kubernetes_units() {
        let documents = documents();
        let text = serde_yaml::to_string(kind(&documents, "Deployment")).unwrap();
        assert!(text.contains("cpu: 500m"));
        assert!(text.contains("memory: 256Mi"));
    }

    #[test]
    fn the_kubernetes_escape_hatch_overrides_generated_keys() {
        let source = format!("{EXHAUSTIVE_EXAMPLE}  kubernetes:\n    marker: applied\n");
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        assert!(text.contains("marker: applied"));
    }

    /// The realistic escape-hatch use, and the one that was broken: patching
    /// something NESTED must not take out its siblings.
    ///
    /// The test above passed for as long as the feature was broken, because a
    /// fresh top-level key looks identical under a shallow insert and a deep
    /// merge. Only a nested patch tells them apart — the shallow version replaced
    /// `spec` wholesale, producing a Deployment with no selector. Schema
    /// validation of a real example is what surfaced it.
    #[test]
    fn a_nested_escape_hatch_patch_keeps_the_generated_spec() {
        const NESTED_PATCH: &str = r#"  kubernetes:
    spec:
      template:
        spec:
          containers:
            - name: conformance
              args:
                - serve
"#;
        let source = format!("{EXHAUSTIVE_EXAMPLE}{NESTED_PATCH}");
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&text)
            .map(|d| Value::deserialize(d).unwrap())
            .filter(|v| !v.is_null())
            .collect();
        let deployment = kind(&documents, "Deployment");

        assert!(
            deployment["spec"].get("selector").is_some(),
            "the generated selector was replaced by the escape hatch:\n{text}"
        );
        let containers = deployment["spec"]["template"]["spec"]["containers"]
            .as_sequence()
            .expect("containers survived");
        assert_eq!(
            containers.len(),
            1,
            "the container was appended, not merged"
        );
        assert_eq!(containers[0]["args"][0], Value::String("serve".into()));
        // Everything the renderer produced for that container is still there.
        assert!(containers[0]["image"]
            .as_str()
            .unwrap_or_default()
            .contains("conformance"));
        assert!(containers[0].get("readinessProbe").is_some());
        assert!(containers[0].get("env").is_some());
        assert!(containers[0].get("volumeMounts").is_some());
    }

    #[test]
    fn the_docker_host_escape_hatch_is_absent_from_kubernetes_output() {
        let source = format!("{EXHAUSTIVE_EXAMPLE}  dockerHost:\n    marker: compose-only-value\n");
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        assert!(!text.contains("compose-only-value"));
    }
}
