//! Config-file mutations for `init`/`use`/`env add`/`env rm`. These do targeted
//! TEXT edits rather than a serde load→dump, so comments and formatting survive,
//! then re-validate the result (reverting the file if the edit broke it).

use crate::config::DabbaConfig;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

const STARTER: &str = "\
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
metadata:
  name: dabba
spec:
  domain: localtest.me # shared across envs
  tls:
    issuer: selfsigned
  defaultEnvironment: kind
  environments:
    - { name: kind, substrate: kind }
    - { name: k3d, substrate: k3d }
    - { name: minikube, substrate: minikube }
  observability:
    enabled: false # Vector -> OpenObserve (+ OTEL traces); opt-in, off by default
  useCases:
    - demo
";

/// `dabba init` — write a starter config (the 3 local envs) if it doesn't exist.
pub fn init(config: &Path) -> Result<()> {
    if config.exists() {
        bail!("{} already exists", config.display());
    }
    std::fs::write(config, STARTER).with_context(|| format!("writing {}", config.display()))?;
    println!("✓ wrote {} (envs: kind, k3d, minikube)", config.display());
    Ok(())
}

/// `dabba use <name>` — set spec.defaultEnvironment.
pub fn use_env(config: &Path, name: &str) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    if !cfg.env_names().contains(&name) {
        bail!(
            "no environment named {:?} (have: {})",
            name,
            cfg.env_names().join(", ")
        );
    }
    let text = std::fs::read_to_string(config)?;
    let out = set_default_environment(&text, name);
    write_validated(config, &text, &out)?;
    println!("✓ default environment is now {name}");
    Ok(())
}

/// `dabba env <name> add` — append an environment.
pub fn add_env(config: &Path, name: &str, substrate: &str, domain: Option<&str>) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    if cfg.env_names().contains(&name) {
        bail!("environment {:?} already exists", name);
    }
    let text = std::fs::read_to_string(config)?;
    let out = add_environment(&text, name, substrate, domain)?;
    write_validated(config, &text, &out)?;
    println!("✓ added environment {name} (substrate {substrate})");
    Ok(())
}

/// `dabba env <name> rm` — remove an environment.
pub fn rm_env(config: &Path, name: &str) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    if !cfg.env_names().contains(&name) {
        bail!("no environment named {:?}", name);
    }
    let text = std::fs::read_to_string(config)?;
    let out = remove_environment(&text, name)?;
    write_validated(config, &text, &out)?;
    println!("✓ removed environment {name}");
    Ok(())
}

fn write_validated(config: &Path, original: &str, new_text: &str) -> Result<()> {
    std::fs::write(config, new_text)?;
    if let Err(e) = DabbaConfig::load(config) {
        let _ = std::fs::write(config, original); // revert
        bail!("edit produced an invalid config (reverted): {e}");
    }
    Ok(())
}

fn indent_of(line: &str) -> String {
    line.chars().take_while(|c| c.is_whitespace()).collect()
}

/// Replace an existing `defaultEnvironment:` line, else insert one before
/// `environments:` at the same indent.
fn set_default_environment(text: &str, name: &str) -> String {
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("defaultEnvironment:"))
    {
        let out: Vec<String> = text
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("defaultEnvironment:") {
                    format!("{}defaultEnvironment: {name}", indent_of(l))
                } else {
                    l.to_string()
                }
            })
            .collect();
        return out.join("\n") + "\n";
    }
    let mut out = Vec::new();
    let mut inserted = false;
    for l in text.lines() {
        if !inserted && l.trim_start().starts_with("environments:") {
            out.push(format!("{}defaultEnvironment: {name}", indent_of(l)));
            inserted = true;
        }
        out.push(l.to_string());
    }
    out.join("\n") + "\n"
}

/// Append a flow-style env entry after the last item in the `environments:` list.
fn add_environment(
    text: &str,
    name: &str,
    substrate: &str,
    domain: Option<&str>,
) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let env_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with("environments:"))
        .ok_or_else(|| anyhow!("no `environments:` block found in the config"))?;

    // The list items' indent (the first `- …` after `environments:`), defaulting to
    // the `environments:` indent + 2 if the list is empty.
    let item_indent = lines[env_idx + 1..]
        .iter()
        .find(|l| l.trim_start().starts_with('-'))
        .map(|l| indent_of(l))
        .unwrap_or_else(|| format!("{}  ", indent_of(lines[env_idx])));
    let item_indent_len = item_indent.len();

    // The list ends at the first non-blank line indented less than the items.
    let mut insert_at = env_idx + 1;
    for (off, l) in lines[env_idx + 1..].iter().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        if indent_of(l).len() >= item_indent_len {
            insert_at = env_idx + 1 + off + 1;
        } else {
            break;
        }
    }

    let domain_part = domain.map(|d| format!(", domain: {d}")).unwrap_or_default();
    let new_item =
        format!("{item_indent}- {{ name: {name}, substrate: {substrate}{domain_part} }}");
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out.insert(insert_at, new_item);
    Ok(out.join("\n") + "\n")
}

/// Remove a flow-style (`- { name: <name>, … }`) env entry.
fn remove_environment(text: &str, name: &str) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let target = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with('-') && t.contains('{') && t.contains(&format!("name: {name}"))
    });
    let Some(idx) = target else {
        bail!(
            "could not find a single-line entry for env {:?} — remove it by hand \
             (block-style entries aren't auto-edited yet)",
            name
        );
    };
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out.remove(idx);
    Ok(out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "\
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
spec:
  domain: localtest.me
  defaultEnvironment: kind
  environments:
    - { name: kind, substrate: kind }
    - { name: k3d, substrate: k3d }
  useCases:
    - demo
";

    fn parses(text: &str) -> DabbaConfig {
        let cfg: DabbaConfig = serde_yaml::from_str(text).unwrap();
        cfg.validate().unwrap();
        cfg
    }

    #[test]
    fn set_default_replaces() {
        let out = set_default_environment(CFG, "k3d");
        assert!(out.contains("defaultEnvironment: k3d"));
        assert!(!out.contains("defaultEnvironment: kind"));
        assert_eq!(parses(&out).default_env_name().unwrap(), "k3d");
    }

    #[test]
    fn set_default_inserts_when_absent() {
        let no_default = CFG.replace("  defaultEnvironment: kind\n", "");
        let out = set_default_environment(&no_default, "k3d");
        assert_eq!(parses(&out).default_env_name().unwrap(), "k3d");
        // inserted at the environments: indent (2 spaces)
        assert!(out.contains("\n  defaultEnvironment: k3d\n"));
    }

    #[test]
    fn add_appends_after_last_item() {
        let out = add_environment(CFG, "minikube", "minikube", None).unwrap();
        let cfg = parses(&out);
        assert_eq!(cfg.env_names(), vec!["kind", "k3d", "minikube"]);
        // comments / useCases preserved
        assert!(out.contains("useCases:"));
    }

    #[test]
    fn add_with_domain() {
        let out = add_environment(
            CFG,
            "staging",
            "scaleway-kapsule",
            Some("staging.example.com"),
        )
        .unwrap();
        assert!(out.contains("domain: staging.example.com"));
        assert!(parses(&out).env_names().contains(&"staging"));
    }

    #[test]
    fn remove_drops_the_entry() {
        let out = remove_environment(CFG, "k3d").unwrap();
        let cfg = parses(&out);
        assert_eq!(cfg.env_names(), vec!["kind"]);
    }
}
