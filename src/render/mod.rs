//! Rendering a portable [`Application`] into what each substrate actually runs.
//!
//! Two renderers, one schema. [`compose`] emits a `docker-compose.yml` the existing
//! reconciler converges without modification; [`kubernetes`] emits the objects the
//! Flux path already consumes. Neither is allowed to be a superset of the other —
//! see [`crate::application`] for why the schema is the intersection, and
//! [`conformance`] for the test that enforces it.
//!
//! Quantity conversion lives here because both renderers need the same answer from
//! the same input: the schema states resource limits in Kubernetes quantity form,
//! and compose wants decimal cores and a byte-suffixed size.

pub mod compose;
pub mod kubernetes;

// The conformance matrix is a test harness, not a runtime concern: it exists to
// fail the build when a renderer stops honouring a schema field.
#[cfg(test)]
pub mod conformance;

use anyhow::{bail, Result};

/// Convert a Kubernetes CPU quantity to compose's decimal-core `cpus` value.
///
/// Kubernetes writes "500m" for half a core; compose writes "0.5". A bare number is
/// already a core count in both.
pub fn cpu_to_cores(quantity: &str) -> Result<String> {
    let quantity = quantity.trim();
    if let Some(millicores) = quantity.strip_suffix('m') {
        let millicores: f64 = millicores
            .parse()
            .map_err(|_| anyhow::anyhow!("cpu {quantity:?} is not a valid quantity"))?;
        // Trim trailing zeros so 500m reads as "0.5" rather than "0.500".
        let cores = millicores / 1000.0;
        return Ok(format!("{cores}"));
    }
    quantity
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("cpu {quantity:?} is not a valid quantity"))?;
    Ok(quantity.to_string())
}

/// Convert a Kubernetes memory quantity to compose's `memory` value.
///
/// Kubernetes uses binary suffixes (Ki/Mi/Gi) and decimal ones (K/M/G); compose
/// accepts b/k/m/g and treats them as binary multiples. The mapping is therefore
/// Mi -> m, and a decimal M is converted rather than passed through, so "256M" does
/// not silently become 256 MiB.
pub fn memory_to_compose(quantity: &str) -> Result<String> {
    let quantity = quantity.trim();
    let (number, suffix) = quantity.split_at(
        quantity
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(quantity.len()),
    );
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("memory {quantity:?} is not a valid quantity"))?;

    let bytes = match suffix {
        "" => value,
        "Ki" => value * 1024.0,
        "Mi" => value * 1024.0 * 1024.0,
        "Gi" => value * 1024.0 * 1024.0 * 1024.0,
        "K" | "k" => value * 1000.0,
        "M" => value * 1_000_000.0,
        "G" => value * 1_000_000_000.0,
        other => bail!("memory suffix {other:?} is not one of Ki, Mi, Gi, K, M, G"),
    };
    // Compose wants an integer with a binary suffix; bytes is exact and unambiguous.
    Ok(format!("{}b", bytes as u64))
}

/// A YAML mapping of plain string pairs — the shape Kubernetes labels and
/// selectors take, needed in enough places to be worth naming.
pub fn string_map<const N: usize>(pairs: [(&str, &str); N]) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    for (key, value) in pairs {
        mapping.insert(
            serde_yaml::Value::String(key.to_string()),
            serde_yaml::Value::String(value.to_string()),
        );
    }
    mapping
}

/// Merge `overlay` into `base`, the way a reader of an escape hatch expects.
///
/// The escape blocks are documented as MERGED into the rendered output, and an
/// earlier version simply inserted top-level keys — so a `kubernetes:` block
/// setting `spec.template.spec.containers` replaced the whole `spec` and took
/// `selector` with it, producing a Deployment the API server rejects. Schema
/// validation caught it; the existing test did not, because it used a fresh
/// top-level key where a shallow insert and a deep merge look identical.
///
/// Three rules, in order:
///
/// 1. Two mappings merge key by key, recursively.
/// 2. Two sequences whose elements are mappings carrying a `name` merge BY that
///    name — an element whose name exists in the base is merged into it, and one
///    whose name is new is appended. This is the subset of Kubernetes' strategic
///    merge that matters in practice: containers, ports, env and volumes all key
///    on `name`, and without it patching one field of one container means
///    restating the entire container.
/// 3. Anything else: the overlay wins.
pub fn deep_merge(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, overlay_value) in overlay {
                let merged = match base.remove(&key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => overlay_value,
                };
                base.insert(key, merged);
            }
            Value::Mapping(base)
        }
        (Value::Sequence(base), Value::Sequence(overlay)) if keyed_by_name(&base) => {
            merge_sequence_by_name(base, overlay)
        }
        (_, overlay) => overlay,
    }
}

/// Is every element a mapping with a `name`? Only then is merging by name
/// well defined; a list of scalars (`args`, `command`) must replace wholesale,
/// because appending to a command line would be nonsense.
fn keyed_by_name(items: &[serde_yaml::Value]) -> bool {
    !items.is_empty()
        && items
            .iter()
            .all(|item| item.get("name").and_then(|n| n.as_str()).is_some())
}

fn merge_sequence_by_name(
    base: Vec<serde_yaml::Value>,
    overlay: Vec<serde_yaml::Value>,
) -> serde_yaml::Value {
    let name_of = |item: &serde_yaml::Value| {
        item.get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
    };
    let mut merged = base;
    for overlay_item in overlay {
        match name_of(&overlay_item).and_then(|name| {
            merged
                .iter()
                .position(|b| name_of(b).as_deref() == Some(&name))
        }) {
            Some(index) => {
                let existing = merged.remove(index);
                merged.insert(index, deep_merge(existing, overlay_item));
            }
            None => merged.push(overlay_item),
        }
    }
    serde_yaml::Value::Sequence(merged)
}

/// The compose project a stack runs under, matching the reconciler's convention.
/// Both renderers need it: compose to name the project, and the health command to
/// address the stack's network.
pub fn project_name(application_name: &str) -> String {
    format!("gitops-{application_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_cpu_quantities() {
        assert_eq!(cpu_to_cores("500m").unwrap(), "0.5");
        assert_eq!(cpu_to_cores("1500m").unwrap(), "1.5");
        assert_eq!(cpu_to_cores("2").unwrap(), "2");
        assert!(cpu_to_cores("half").is_err());
    }

    #[test]
    fn converts_memory_quantities() {
        assert_eq!(memory_to_compose("256Mi").unwrap(), "268435456b");
        assert_eq!(memory_to_compose("1Gi").unwrap(), "1073741824b");
        // A decimal M is 1e6, not 1 MiB. Passing the suffix through unchanged would
        // have quietly inflated the limit by 5%.
        assert_eq!(memory_to_compose("256M").unwrap(), "256000000b");
        assert_ne!(
            memory_to_compose("256M").unwrap(),
            memory_to_compose("256Mi").unwrap()
        );
        assert!(memory_to_compose("256Zi").is_err());
    }

    fn yaml(text: &str) -> serde_yaml::Value {
        serde_yaml::from_str(text).unwrap()
    }

    /// The bug this exists to prevent: a shallow insert let an overlay setting
    /// `spec.template` replace the whole `spec`, taking `selector` with it and
    /// producing a Deployment the API server rejects.
    #[test]
    fn merging_a_nested_key_keeps_its_siblings() {
        let merged = deep_merge(
            yaml("spec:\n  selector: {matchLabels: {app: x}}\n  replicas: 1\n  template: {a: 1}"),
            yaml("spec:\n  template: {b: 2}"),
        );
        assert!(merged["spec"]["selector"]["matchLabels"]["app"] == yaml("x"));
        assert_eq!(merged["spec"]["replicas"], yaml("1"));
        // The overlay merged into template rather than replacing it.
        assert_eq!(merged["spec"]["template"]["a"], yaml("1"));
        assert_eq!(merged["spec"]["template"]["b"], yaml("2"));
    }

    /// Lists of named things merge by name, so patching one field of one
    /// container does not mean restating the whole container.
    #[test]
    fn sequences_of_named_maps_merge_by_name() {
        let merged = deep_merge(
            yaml("containers:\n  - name: a\n    image: img\n    ports: [{name: p}]"),
            yaml("containers:\n  - name: a\n    args: [run]"),
        );
        let containers = merged["containers"].as_sequence().unwrap();
        assert_eq!(containers.len(), 1, "merged by name, not appended");
        assert_eq!(
            containers[0]["image"],
            yaml("img"),
            "image survived the patch"
        );
        assert_eq!(containers[0]["args"][0], yaml("run"));
        assert_eq!(containers[0]["ports"][0]["name"], yaml("p"));
    }

    #[test]
    fn a_new_name_is_appended_rather_than_replacing() {
        let merged = deep_merge(
            yaml("containers:\n  - name: a\n    image: one"),
            yaml("containers:\n  - name: sidecar\n    image: two"),
        );
        let containers = merged["containers"].as_sequence().unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0]["image"], yaml("one"));
        assert_eq!(containers[1]["name"], yaml("sidecar"));
    }

    /// Scalar lists must REPLACE. Appending to a command line would be nonsense,
    /// and is the reason merging by name is gated on every element having one.
    #[test]
    fn scalar_sequences_replace_rather_than_merge() {
        let merged = deep_merge(yaml("args: [a, b, c]"), yaml("args: [x]"));
        assert_eq!(merged["args"].as_sequence().unwrap().len(), 1);
        assert_eq!(merged["args"][0], yaml("x"));
    }

    /// A list of maps where any element lacks a name cannot be merged by name,
    /// so it replaces — better than merging some elements and not others.
    #[test]
    fn unnamed_maps_in_a_sequence_fall_back_to_replacing() {
        let merged = deep_merge(
            yaml("items:\n  - name: a\n  - other: b"),
            yaml("items:\n  - name: c"),
        );
        assert_eq!(merged["items"].as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn a_scalar_overlay_wins_over_a_mapping() {
        let merged = deep_merge(yaml("restart: {a: 1}"), yaml("restart: never"));
        assert_eq!(merged["restart"], yaml("never"));
    }

    #[test]
    fn project_name_matches_the_reconciler_convention() {
        assert_eq!(project_name("podinfo"), "gitops-podinfo");
    }
}
