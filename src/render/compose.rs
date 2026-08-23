//! Render an [`Application`] to a `docker-compose.yml` the reconciler converges.
//!
//! The output is deliberately shaped to what `backends/docker/reconcile.sh` already
//! consumes, so rendered stacks need no reconciler changes and sit alongside
//! hand-written ones:
//!
//! * **Long-form volumes.** `source: ./<name>` is the form the reconciler's bind
//!   source extractor handles and pre-creates as the invoking user. Letting dockerd
//!   create them instead makes them root-owned, which is how a non-root container
//!   ends up unable to write its own data directory.
//! * **`x-health-cmd`.** The reconciler's health gate reads this top-level key, and
//!   that gate is what makes "deployed" and "healthy" distinguishable on a compose
//!   host — the same distinction a readiness probe provides on Kubernetes.
//! * **`${VARIABLE}` for secrets.** The reconciler renders with `--no-interpolate`
//!   precisely so these survive to be resolved from the box-local `.env` at `up`.

use crate::application::{Application, EnvironmentVariable, HealthCheck, Port};
use crate::render::{cpu_to_cores, memory_to_compose, project_name};
use anyhow::{Context, Result};
use serde_yaml::{Mapping, Value};

fn string(s: impl Into<String>) -> Value {
    Value::String(s.into())
}

/// Render the application as a compose file.
pub fn render(app: &Application) -> Result<String> {
    let name = &app.metadata.name;
    let mut service = Mapping::new();

    service.insert(
        string("image"),
        string(format!("{}:{}", app.spec.image, app.spec.tag)),
    );

    // Ports. Only published ones appear here; an unpublished port needs no compose
    // entry because containers on the stack network reach each other directly.
    let published: Vec<Value> = app
        .spec
        .ports
        .iter()
        .filter_map(|port| {
            port.publish
                .map(|host| string(format!("{host}:{}", port.container_port)))
        })
        .collect();
    if !published.is_empty() {
        service.insert(string("ports"), Value::Sequence(published));
    }

    // Environment. A literal becomes its value; a secret becomes a ${VARIABLE}
    // reference that the box-local .env resolves, which is the same indirection the
    // Kubernetes side gets from a secretKeyRef.
    if !app.spec.environment.is_empty() {
        let mut environment = Mapping::new();
        for variable in &app.spec.environment {
            let rendered = match variable {
                EnvironmentVariable {
                    value: Some(value), ..
                } => value.clone(),
                EnvironmentVariable {
                    secret: Some(_), ..
                } => {
                    format!("${{{}}}", variable.name)
                }
                // validate() rejects neither-nor, so this is unreachable in
                // practice; rendering an empty string beats panicking.
                _ => String::new(),
            };
            environment.insert(string(variable.name.clone()), string(rendered));
        }
        service.insert(string("environment"), Value::Mapping(environment));
    }

    // Volumes, long form and ./-relative so the reconciler pre-creates the source.
    if !app.spec.volumes.is_empty() {
        let mounts: Vec<Value> = app
            .spec
            .volumes
            .iter()
            .map(|volume| {
                let mut mount = Mapping::new();
                mount.insert(string("type"), string("bind"));
                mount.insert(string("source"), string(format!("./{}", volume.name)));
                mount.insert(string("target"), string(volume.mount_path.clone()));
                Value::Mapping(mount)
            })
            .collect();
        service.insert(string("volumes"), Value::Sequence(mounts));
    }

    // Resource limits. Compose honours deploy.resources.limits outside swarm mode.
    if let Some(resources) = &app.spec.resources {
        let mut limits = Mapping::new();
        if let Some(cpu) = &resources.cpu {
            limits.insert(
                string("cpus"),
                string(cpu_to_cores(cpu).context("rendering resources.cpu for compose")?),
            );
        }
        if let Some(memory) = &resources.memory {
            limits.insert(
                string("memory"),
                string(
                    memory_to_compose(memory).context("rendering resources.memory for compose")?,
                ),
            );
        }
        if !limits.is_empty() {
            let mut resources_map = Mapping::new();
            resources_map.insert(string("limits"), Value::Mapping(limits));
            let mut deploy = Mapping::new();
            deploy.insert(string("resources"), Value::Mapping(resources_map));
            service.insert(string("deploy"), Value::Mapping(deploy));
        }
    }

    // A long-running service restarts on both substrates. This is not configurable
    // (see the schema's out-of-scope list): a Kubernetes Deployment cannot express
    // anything else, so making it a knob would mean it worked on one side only.
    service.insert(string("restart"), string("unless-stopped"));

    // The dockerHost escape hatch is merged last so it can override anything above.
    if let Some(Value::Mapping(escape)) = &app.spec.docker_host {
        for (key, value) in escape {
            service.insert(key.clone(), value.clone());
        }
    }

    let mut services = Mapping::new();
    services.insert(string(name.clone()), Value::Mapping(service));

    let mut root = Mapping::new();
    root.insert(string("services"), Value::Mapping(services));

    if let Some(health) = &app.spec.health_check {
        root.insert(string("x-health-cmd"), string(health_command(app, health)?));
    }

    // The compose file carries secret REFERENCES, never values: the ${VARIABLE}
    // above says "resolve me", and this says where from. The reconciler reads this
    // each tick and writes resolved values into the stack's box-local .env — the
    // compose-host equivalent of External Secrets populating a Secret on Kubernetes.
    //
    // This file goes into a git repository, so the split is the whole point. A
    // rendered artifact that inlined the value would commit every secret to git the
    // moment anyone ran the renderer.
    let secrets: Vec<(&str, String)> = app
        .spec
        .environment
        .iter()
        .filter_map(|variable| {
            variable
                .secret
                .as_ref()
                .map(|s| (variable.name.as_str(), format!("{}#{}", s.name, s.key)))
        })
        .collect();
    if !secrets.is_empty() {
        let mut mapping = Mapping::new();
        for (name, reference) in secrets {
            mapping.insert(string(name), string(reference));
        }
        root.insert(string("x-secrets"), Value::Mapping(mapping));
    }

    let body = serde_yaml::to_string(&Value::Mapping(root))?;
    Ok(format!(
        "# Rendered by dabba from the portable Application definition for {name}.\n\
         # Edit the Application, not this file — the reconciler overwrites it.\n\
         {body}"
    ))
}

/// The shell command the reconciler's health gate runs.
///
/// An HTTP probe runs from a throwaway curl container attached to the stack's own
/// network. Probing the published host port instead would make `healthCheck` work
/// only for published ports, and running curl inside the application container
/// would require the image to ship curl — Kubernetes needs neither, because the
/// kubelet probes from outside. This keeps the field genuinely portable.
fn health_command(app: &Application, health: &HealthCheck) -> Result<String> {
    let project = project_name(&app.metadata.name);
    let service = &app.metadata.name;

    if let Some(http) = &health.http_get {
        let port = resolve_named_port(&app.spec.ports, &http.port)?;
        return Ok(format!(
            "docker run --rm --network {project}_default curlimages/curl:latest \
             -fsS --max-time 5 http://{service}:{port}{path} >/dev/null 2>&1",
            path = http.path
        ));
    }

    // An exec probe runs in the container, exactly as Kubernetes runs one.
    let command = health.exec.join(" ");
    Ok(format!(
        "docker compose -p {project} exec -T {service} {command} >/dev/null 2>&1"
    ))
}

fn resolve_named_port(ports: &[Port], name: &str) -> Result<u16> {
    ports
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.container_port)
        .with_context(|| format!("healthCheck.httpGet.port {name:?} names no declared port"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::EXHAUSTIVE_EXAMPLE;

    fn rendered() -> String {
        let app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        render(&app).unwrap()
    }

    /// The output must be a compose file docker itself accepts, not merely YAML.
    #[test]
    fn renders_parseable_compose_yaml() {
        let text = rendered();
        let value: Value = serde_yaml::from_str(&text).unwrap();
        let services = value.get("services").expect("has services");
        assert!(services.get("conformance").is_some());
    }

    #[test]
    fn renders_image_with_its_tag() {
        assert!(rendered().contains("ghcr.io/example/conformance:1.2.3"));
    }

    #[test]
    fn publishes_only_ports_that_ask_to_be_published() {
        let text = rendered();
        assert!(text.contains("19898:9898"), "published port missing");

        let mut app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        app.spec.ports[0].publish = None;
        let unpublished = render(&app).unwrap();
        assert!(
            !unpublished.contains("ports:"),
            "an unpublished port should produce no ports entry"
        );
    }

    /// Bind sources must be ./-relative and long form, or the reconciler will not
    /// pre-create them and dockerd will make them root-owned.
    #[test]
    fn renders_bind_mounts_the_reconciler_can_pre_create() {
        let text = rendered();
        assert!(text.contains("type: bind"));
        assert!(text.contains("source: ./data"));
        assert!(text.contains("target: /var/lib/conformance"));
    }

    #[test]
    fn renders_literal_environment_and_indirects_secrets() {
        let text = rendered();
        assert!(text.contains("LITERAL_SETTING: literal-value"));
        // The secret's VALUE must never appear; only the indirection.
        assert!(text.contains("SECRET_SETTING: ${SECRET_SETTING}"));
    }

    /// The reference has to travel with the stack, or the reconciler cannot know
    /// which secret a ${VARIABLE} wants. It must be a reference and nothing more.
    #[test]
    fn emits_secret_references_for_the_reconciler_to_resolve() {
        let text = rendered();
        assert!(
            text.contains("x-secrets:"),
            "no secret reference block:\n{text}"
        );
        assert!(text.contains("SECRET_SETTING: demo#message"));
    }

    /// A stack with no secrets must not carry an empty block that looks like a
    /// resolution step the reconciler has to take.
    #[test]
    fn omits_the_secret_block_when_nothing_is_secret_backed() {
        let mut app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        app.spec.environment.retain(|v| v.secret.is_none());
        let text = render(&app).unwrap();
        assert!(!text.contains("x-secrets"));
    }

    #[test]
    fn converts_resource_limits_to_compose_units() {
        let text = rendered();
        assert!(text.contains("cpus: '0.5'") || text.contains("cpus: \"0.5\""));
        assert!(text.contains("268435456b"), "256Mi should be exact bytes");
    }

    /// An HTTP probe must not require the port to be published, and must not
    /// require the application image to ship curl.
    #[test]
    fn health_probe_works_without_publishing_or_in_container_tooling() {
        let mut app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        app.spec.ports[0].publish = None;
        let text = render(&app).unwrap();
        assert!(text.contains("x-health-cmd:"));
        assert!(text.contains("--network gitops-conformance_default"));
        assert!(text.contains("http://conformance:9898/healthz"));
    }

    #[test]
    fn renders_an_exec_probe_as_a_compose_exec() {
        let source = EXHAUSTIVE_EXAMPLE.replace(
            "    httpGet:\n      path: /healthz\n      port: http",
            "    exec: [\"/bin/true\"]",
        );
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        assert!(text.contains("docker compose -p gitops-conformance exec -T conformance /bin/true"));
    }

    #[test]
    fn the_docker_host_escape_hatch_overrides_generated_keys() {
        let source = format!("{EXHAUSTIVE_EXAMPLE}  dockerHost:\n    restart: \"no\"\n");
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        // The escape hatch is free-form: whatever the author wrote is what lands,
        // and it must replace the generated key rather than sit beside it.
        assert!(
            text.contains("restart: no"),
            "escape hatch value missing:\n{text}"
        );
        assert!(
            !text.contains("restart: unless-stopped"),
            "generated value survived"
        );
    }

    /// The kubernetes escape hatch must not leak into compose output.
    #[test]
    fn the_kubernetes_escape_hatch_is_absent_from_compose_output() {
        let source =
            format!("{EXHAUSTIVE_EXAMPLE}  kubernetes:\n    marker: kubernetes-only-value\n");
        let app = Application::parse(&source).unwrap();
        let text = render(&app).unwrap();
        assert!(!text.contains("kubernetes-only-value"));
    }

    /// The reconciler parses this file with sed and awk, not a YAML library, so the
    /// renderer's output has to be the exact SHAPE those extractors match. Nothing
    /// else asserts that: the Rust tests check the renderer, the bash tests use
    /// hand-written fixtures, and the two never meet. A rendering change that is
    /// still valid YAML can therefore silently stop being readable on the box.
    ///
    /// Each assertion below mirrors one extractor in `backends/docker/reconcile.sh`.
    #[test]
    fn the_reconcilers_extractors_can_read_what_this_renderer_emits() {
        let text = rendered();

        // health_command(): `sed -n 's/^x-health-cmd: //p' | head -1`. The key must
        // be at column zero and the command entirely on that one line — a folded or
        // block scalar leaves the gate reading an indicator or half a command, which
        // is how a crash-looping stack once reported itself healthy.
        let health = text
            .lines()
            .find_map(|line| line.strip_prefix("x-health-cmd: "))
            .expect("x-health-cmd is emitted at column zero with a single-space separator");
        assert!(
            !matches!(health.trim(), ">" | ">-" | ">+" | "|" | "|-" | "|+"),
            "x-health-cmd rendered as a YAML block scalar; health_command_is_usable \
             refuses it and the stack can never converge"
        );
        assert!(
            health.contains("docker ") && health.ends_with("2>&1"),
            "the health command was truncated or folded: {health:?}"
        );

        // extract_bind_sources(): long-form `source: ./path`, which is what it
        // pre-creates as this user so dockerd does not create it root-owned.
        assert!(
            text.lines()
                .any(|line| line.trim().starts_with("source: ./")),
            "no long-form bind source was emitted; the reconciler pre-creates nothing \
             and dockerd creates the data directory root-owned"
        );
    }
}
