//! The guard that makes half-implementation impossible.
//!
//! The portable schema is only worth anything if BOTH renderers actually honour
//! every field in it. That property cannot rest on reviewer vigilance — the
//! multitool build produced three separate surfaces printing a hand-maintained
//! literal zero where a computed value belonged, and a whole crate that was
//! complete, tested and imported by nobody. Anything maintained by hand drifts.
//!
//! So the property is enforced here. [`MATRIX`] pairs each field of
//! [`crate::application::ApplicationSpec`] with a check against each renderer's
//! output, and the tests below:
//!
//! 1. render [`EXHAUSTIVE_EXAMPLE`] through both renderers,
//! 2. assert every field's effect is visible in both outputs, and
//! 3. assert the matrix itself covers every field the fixture exercises — so
//!    adding a field to the schema and forgetting a renderer turns CI red rather
//!    than shipping a field that silently works on one substrate.
//!
//! Point 3 is the load-bearing one. Without it this file is just another
//! hand-maintained list.

use crate::application::{Application, EXHAUSTIVE_EXAMPLE};
use crate::render::{compose, kubernetes};

/// One schema field and how to recognise that each renderer honoured it.
pub struct FieldExpectation {
    /// The field name as it serialises in `spec`.
    pub field: &'static str,
    /// What the field means, for the failure message.
    pub honoured_by: &'static str,
    /// Recognises the field's effect in the compose output.
    pub in_compose: fn(&str) -> bool,
    /// Recognises the field's effect in the Kubernetes output.
    pub in_kubernetes: fn(&str) -> bool,
}

/// Every portable field, and proof that each renderer does something with it.
///
/// The predicates look for the DISTINCTIVE values in `EXHAUSTIVE_EXAMPLE`, so a
/// renderer that emits a plausible-looking default instead of the declared value
/// still fails.
pub const MATRIX: &[FieldExpectation] = &[
    FieldExpectation {
        field: "image",
        honoured_by: "the container image",
        in_compose: |text| text.contains("ghcr.io/example/conformance"),
        in_kubernetes: |text| text.contains("ghcr.io/example/conformance"),
    },
    FieldExpectation {
        field: "tag",
        honoured_by: "the image tag",
        in_compose: |text| text.contains(":1.2.3"),
        in_kubernetes: |text| text.contains(":1.2.3"),
    },
    FieldExpectation {
        field: "ports",
        honoured_by: "the port the application listens on, and its exposure",
        in_compose: |text| text.contains("19898:9898"),
        in_kubernetes: |text| text.contains("containerPort: 9898") && text.contains("port: 19898"),
    },
    FieldExpectation {
        field: "environment",
        honoured_by: "literal values, and secrets by indirection rather than inlining",
        in_compose: |text| {
            text.contains("LITERAL_SETTING: literal-value") && text.contains("${SECRET_SETTING}")
        },
        in_kubernetes: |text| text.contains("literal-value") && text.contains("secretKeyRef"),
    },
    FieldExpectation {
        field: "volumes",
        honoured_by: "persistent storage mounted at the declared path",
        in_compose: |text| {
            text.contains("source: ./data") && text.contains("target: /var/lib/conformance")
        },
        in_kubernetes: |text| {
            text.contains("mountPath: /var/lib/conformance")
                && text.contains("PersistentVolumeClaim")
        },
    },
    FieldExpectation {
        field: "healthCheck",
        honoured_by: "distinguishing deployed from working",
        in_compose: |text| text.contains("x-health-cmd") && text.contains("/healthz"),
        in_kubernetes: |text| text.contains("readinessProbe") && text.contains("/healthz"),
    },
    FieldExpectation {
        field: "resources",
        honoured_by: "cpu and memory limits, in each runtime's own units",
        in_compose: |text| text.contains("0.5") && text.contains("268435456b"),
        in_kubernetes: |text| text.contains("cpu: 500m") && text.contains("memory: 256Mi"),
    },
];

/// Render the exhaustive fixture through both renderers.
pub fn render_both() -> (String, String) {
    let app = Application::parse(EXHAUSTIVE_EXAMPLE).expect("the exhaustive example is valid");
    (
        compose::render(&app).expect("compose render"),
        kubernetes::render(&app).expect("kubernetes render"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline property: every portable field is honoured on BOTH substrates.
    #[test]
    fn every_field_is_honoured_by_both_renderers() {
        let (compose_output, kubernetes_output) = render_both();
        let mut failures = Vec::new();

        for expectation in MATRIX {
            if !(expectation.in_compose)(&compose_output) {
                failures.push(format!(
                    "spec.{} is in the portable schema but the COMPOSE renderer does not \
                     honour it ({}). Either implement it or remove it from the schema — \
                     a field that works on one substrate is exactly what this schema \
                     exists to prevent.",
                    expectation.field, expectation.honoured_by
                ));
            }
            if !(expectation.in_kubernetes)(&kubernetes_output) {
                failures.push(format!(
                    "spec.{} is in the portable schema but the KUBERNETES renderer does \
                     not honour it ({}). Either implement it or remove it from the \
                     schema.",
                    expectation.field, expectation.honoured_by
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "portability conformance failed:\n  {}",
            failures.join("\n  ")
        );
    }

    /// The fields of `ApplicationSpec`, read out of the source text rather than out
    /// of a rendered instance.
    ///
    /// This check used to derive them from `serde_yaml::to_value(&app.spec)`, which
    /// only ever saw the fields `EXHAUSTIVE_EXAMPLE` happened to set. Every optional
    /// field carries `skip_serializing_if`, so a field the fixture forgot was
    /// invisible to the very check meant to catch it: a new portable field honoured
    /// by one renderer, or by neither, left the whole suite green.
    ///
    /// Deriving the list from source is the same approach the reconciler guards
    /// take, and for the same reason — a hand-maintained list is the thing that
    /// keeps going wrong.
    fn schema_fields() -> Vec<String> {
        let source = include_str!("../application.rs");
        let body = source
            .split_once("pub struct ApplicationSpec {")
            .expect("ApplicationSpec is declared in application.rs")
            .1
            .split_once("\n}")
            .expect("ApplicationSpec has a closing brace")
            .0;

        let mut fields = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("pub ") else {
                continue;
            };
            let Some((name, _)) = rest.split_once(':') else {
                continue;
            };
            if !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                continue;
            }
            // `#[serde(rename_all = "camelCase")]` on the struct.
            let mut camel = String::new();
            let mut capitalise = false;
            for character in name.chars() {
                if character == '_' {
                    capitalise = true;
                } else if capitalise {
                    camel.extend(character.to_uppercase());
                    capitalise = false;
                } else {
                    camel.push(character);
                }
            }
            fields.push(camel);
        }
        fields
    }

    /// The escape hatches are fields of the schema but deliberately have no matrix
    /// entry: they are the boundary of the portability promise, and each is meant to
    /// reach exactly ONE renderer. `escape_hatches_reach_exactly_one_renderer` is
    /// what holds them.
    const NOT_PORTABLE: &[&str] = &["kubernetes", "dockerHost"];

    /// Without this, MATRIX is just another hand-maintained list that drifts.
    #[test]
    fn the_matrix_covers_every_field_the_schema_exercises() {
        let fields = schema_fields();
        assert!(
            fields.len() >= 7,
            "parsed only {} fields out of ApplicationSpec; the parser broke and this \
             test would pass vacuously",
            fields.len()
        );
        for excluded in NOT_PORTABLE {
            assert!(
                fields.iter().any(|f| f == excluded),
                "NOT_PORTABLE names {excluded}, which the schema no longer has"
            );
        }

        let app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        let serialized = serde_yaml::to_value(&app.spec).unwrap();
        let mapping = serialized.as_mapping().expect("spec is a mapping");

        for field in &fields {
            if NOT_PORTABLE.contains(&field.as_str()) {
                continue;
            }
            assert!(
                MATRIX.iter().any(|e| e.field == field),
                "spec.{field} is in the portable schema but has no entry in MATRIX, so \
                 nothing checks that both renderers honour it. Add one."
            );
            // The matrix predicates read the RENDERED fixture, so a field the
            // fixture never sets cannot be checked no matter what MATRIX claims.
            assert!(
                mapping.contains_key(serde_yaml::Value::String(field.clone())),
                "spec.{field} is in the portable schema but EXHAUSTIVE_EXAMPLE does not \
                 set it, so its MATRIX entry reads output the fixture never produced. \
                 Add it to the fixture."
            );
        }

        // And the inverse, so a removed field cannot leave a stale entry behind
        // asserting on output that no longer exists.
        for expectation in MATRIX {
            assert!(
                fields.iter().any(|f| f == expectation.field),
                "MATRIX checks spec.{}, which the schema no longer has. Remove it.",
                expectation.field
            );
        }
    }

    /// Every shipped example must parse and render for BOTH substrates.
    ///
    /// The exhaustive fixture exercises every field, but it is written to be
    /// convenient. Real examples combine features in ways a fixture does not — the
    /// escape-hatch deep-merge bug was found by schema-validating the MinIO
    /// example, not by the fixture, because only a real definition patched
    /// something nested.
    #[test]
    fn every_shipped_example_renders_for_both_substrates() {
        let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/applications");
        let entries = std::fs::read_dir(directory).expect("examples/applications exists");
        let mut checked = 0;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();

            let app = Application::parse(&text)
                .unwrap_or_else(|e| panic!("{name} is not a valid definition: {e}"));
            let compose_output = compose::render(&app)
                .unwrap_or_else(|e| panic!("{name} does not render for docker-host: {e}"));
            let kubernetes_output = kubernetes::render(&app)
                .unwrap_or_else(|e| panic!("{name} does not render for Kubernetes: {e}"));

            // Both outputs must be parseable, not merely produced.
            serde_yaml::from_str::<serde_yaml::Value>(&compose_output)
                .unwrap_or_else(|e| panic!("{name} compose output is not valid YAML: {e}"));
            let documents: Vec<serde_yaml::Value> =
                serde_yaml::Deserializer::from_str(&kubernetes_output)
                    .map(|d| serde::Deserialize::deserialize(d).unwrap())
                    .filter(|v: &serde_yaml::Value| !v.is_null())
                    .collect();

            // A Deployment without a selector is rejected by the API server. This
            // is the exact shape the shallow escape-hatch merge produced.
            for document in &documents {
                if document.get("kind").and_then(|k| k.as_str()) == Some("Deployment") {
                    assert!(
                        document["spec"].get("selector").is_some(),
                        "{name} renders a Deployment with no selector"
                    );
                    assert!(
                        document["spec"]["template"]["spec"]
                            .get("containers")
                            .and_then(|c| c.as_sequence())
                            .is_some_and(|c| !c.is_empty()),
                        "{name} renders a Deployment with no containers"
                    );
                }
            }
            checked += 1;
        }

        assert!(
            checked > 0,
            "no example applications were checked — the directory moved or emptied, \
             and this test would pass vacuously forever"
        );
    }

    /// Escape hatches are the boundary of the promise, so each must reach exactly
    /// one renderer. A leak in either direction would mean substrate-specific
    /// configuration silently applying where it was never meant to.
    #[test]
    fn escape_hatches_reach_exactly_one_renderer() {
        let source = format!(
            "{EXHAUSTIVE_EXAMPLE}  kubernetes:\n    marker: kubernetes-only\n  \
             dockerHost:\n    marker: compose-only\n"
        );
        let app = Application::parse(&source).unwrap();
        let compose_output = compose::render(&app).unwrap();
        let kubernetes_output = kubernetes::render(&app).unwrap();

        assert!(compose_output.contains("compose-only"));
        assert!(!compose_output.contains("kubernetes-only"));
        assert!(kubernetes_output.contains("kubernetes-only"));
        assert!(!kubernetes_output.contains("compose-only"));
    }

    /// A `secretKeyRef` is a promise that some Secret exists with that name and key.
    /// Emitting the reference without the object that satisfies it is not a rendered
    /// application, it is a pod that will never start — and the matrix above cannot
    /// see the difference, because both cases contain the string `secretKeyRef`.
    ///
    /// So this walks the actual objects: every reference the Deployment makes must be
    /// matched by an ExternalSecret this same render emitted, by target name AND by
    /// key. That is the Kubernetes half of what the reconciler does on a compose host
    /// when it resolves `x-secrets` into the stack `.env`.
    #[test]
    fn every_secret_reference_is_satisfied_by_an_emitted_external_secret() {
        let app = Application::parse(EXHAUSTIVE_EXAMPLE).unwrap();
        let rendered = kubernetes::render(&app).unwrap();
        let documents: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(&rendered)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .filter(|v: &serde_yaml::Value| !v.is_null())
            .collect();

        // What each emitted ExternalSecret provides: target name -> the keys it fills.
        let provided: Vec<(String, Vec<String>)> = documents
            .iter()
            .filter(|d| d.get("kind").and_then(|k| k.as_str()) == Some("ExternalSecret"))
            .map(|d| {
                let target = d["spec"]["target"]["name"].as_str().unwrap().to_string();
                let keys = d["spec"]["data"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["secretKey"].as_str().unwrap().to_string())
                    .collect();
                (target, keys)
            })
            .collect();

        // What the container asks for.
        let deployment = documents
            .iter()
            .find(|d| d.get("kind").and_then(|k| k.as_str()) == Some("Deployment"))
            .expect("a Deployment was rendered");
        let container = &deployment["spec"]["template"]["spec"]["containers"][0];
        let mut references = 0;
        for variable in container["env"].as_sequence().expect("env was rendered") {
            let Some(key_ref) = variable
                .get("valueFrom")
                .and_then(|v| v.get("secretKeyRef"))
            else {
                continue;
            };
            let wanted_secret = key_ref["name"].as_str().unwrap();
            let wanted_key = key_ref["key"].as_str().unwrap();
            let satisfied = provided.iter().any(|(target, keys)| {
                target == wanted_secret && keys.iter().any(|k| k == wanted_key)
            });
            assert!(
                satisfied,
                "the container reads {wanted_key:?} from Secret {wanted_secret:?}, which no \
                 emitted ExternalSecret produces — the pod would stay in \
                 CreateContainerConfigError. Emitted: {provided:?}"
            );
            references += 1;
        }
        assert!(
            references > 0,
            "the fixture declares a secret but the Deployment makes no secretKeyRef, so \
             this test would pass vacuously"
        );
    }

    /// A secret's VALUE must never be rendered into either artifact — both go into
    /// a git repository. Only the indirection belongs there.
    #[test]
    fn neither_renderer_inlines_a_secret_value() {
        let (compose_output, kubernetes_output) = render_both();
        for output in [&compose_output, &kubernetes_output] {
            assert!(
                !output.contains("secret:\n        name: demo\n        key: message"),
                "a secret declaration was copied verbatim into a rendered artifact"
            );
        }
        // Compose indirects through the box-local .env; Kubernetes through a
        // secretKeyRef. Neither carries the value itself.
        assert!(compose_output.contains("${SECRET_SETTING}"));
        assert!(kubernetes_output.contains("secretKeyRef"));
    }
}
