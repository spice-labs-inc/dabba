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

    #[test]
    fn project_name_matches_the_reconciler_convention() {
        assert_eq!(project_name("podinfo"), "gitops-podinfo");
    }
}
