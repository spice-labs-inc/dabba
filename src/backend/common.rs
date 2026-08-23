//! Backend-neutral helpers shared by every backend. These moved out of the old
//! monolithic `up.rs` unchanged so the k8s and docker backends use one copy rather
//! than duplicating: the per-env working directory, the local secret-zero stash
//! (read-or-generate a per-env credential; read a stashed one for teardown), small
//! path/env utilities, and the config-level `ls`/`show` commands (which are the
//! same for any backend). Process helpers live in [`crate::run`].

use crate::config::{DabbaConfig, Substrate};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `<config dir>/.dabba/<env>` — the per-env working dir (tofu state + kubeconfig for
/// the k8s backend; the gitops clone + applied stacks for the docker backend).
pub fn env_workdir(config: &Path, env_name: &str) -> Result<PathBuf> {
    let base = config
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join(".dabba").join(env_name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    Ok(dir)
}

/// Write a secret to disk so it is never readable by anyone else, not even briefly.
///
/// `std::fs::write` creates the file with the process umask — 0644 on most hosts —
/// and a `set_permissions` afterwards closes it only after the fact. Between the
/// two, OpenBao's root token sits world-readable. The mode has to be applied at
/// creation; the `set_permissions` that follows is for the case where the file
/// already existed with a looser mode.
fn write_secret_file(path: &Path, value: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(value.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, value).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Read-or-generate a per-env secret stashed in the workdir (0600). Reused across
/// re-ups so the value is stable for the life of the env. `complex` adds a fixed
/// upper/digit/special suffix to satisfy app password policies (e.g. OpenObserve).
pub fn env_secret(workdir: &Path, name: &str, complex: bool) -> Result<String> {
    let path = workdir.join(name);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    // The entropy is in the hex; the suffix only satisfies complexity policies.
    let val = if complex {
        format!("{}Aa1!", random_token(24)?)
    } else {
        random_token(24)?
    };
    write_secret_file(&path, &val)?;
    Ok(val)
}

/// Write a per-env secret into the workdir at 0600.
///
/// For values dabba is GIVEN rather than generates — OpenBao's root token and
/// unseal key come back from `bao operator init` and have to be kept, since they
/// cannot be recovered afterwards and cannot live inside the vault they open.
pub fn write_stash(workdir: &Path, name: &str, value: &str) -> Result<()> {
    let path = workdir.join(name);
    write_secret_file(&path, value)?;
    Ok(())
}

/// Read a stashed per-env secret (empty string if absent) — for `down`, which must
/// not generate.
pub fn read_stash(workdir: &Path, name: &str) -> String {
    std::fs::read_to_string(workdir.join(name))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// OS randomness as a lowercase hex string, for callers that need a VALUE rather
/// than a file — credentials that live in OpenBao rather than the local stash.
pub fn random_secret(nbytes: usize) -> Result<String> {
    random_token(nbytes)
}

/// `nbytes` of OS randomness as a lowercase hex string. Bails if `/dev/urandom`
/// can't be read — a silent all-zeros token would be a catastrophic secret.
fn random_token(nbytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; nbytes];
    let mut f = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    f.read_exact(&mut buf).context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Expand a leading `~/` to $HOME; otherwise pass through unchanged.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// True if `bin` is an executable file somewhere on PATH.
pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

/// Progress line to stderr (so stdout stays clean for machine-readable output).
pub fn log(msg: &str) {
    eprintln!("▸ {msg}");
}

/// `dabba ls` — list the configured environments and which one is the default.
/// The listing itself is backend-neutral, but "is it deployed?" is not: each
/// substrate answers that its own way (see the match below).
pub fn ls(config: &Path) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let default = cfg.default_env_name().ok();
    for env in &cfg.spec.environments {
        let marker = if Some(env.name.as_str()) == default {
            "*"
        } else {
            " "
        };
        // Ask the substrate what "deployed" means for it. This used to be a
        // Kubernetes-only probe living in a helper that called itself
        // backend-neutral, so a docker-host environment with a running reconcile
        // loop and live stacks was never reported as deployed.
        let deployed = match env.substrate {
            Substrate::DockerHost => crate::backend::docker::is_deployed(&env.name),
            _ => env_workdir(config, &env.name)
                .ok()
                .map(|w| {
                    w.join("01-cluster").join(".terraform").is_dir() || env.kubeconfig.is_some()
                })
                .unwrap_or(false),
        };
        println!(
            "{marker} {:<16} {:?}{}",
            env.name,
            env.substrate,
            if deployed { "  (deployed)" } else { "" }
        );
    }
    Ok(())
}

/// `dabba env <name>` (no verb) — show the env's resolved config.
pub fn show(config: &Path, env_name: &str) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(Some(env_name))?;
    println!("name:       {}", env.name);
    println!("substrate:  {:?}", env.substrate);
    println!("domain:     {}", env.domain);
    println!("issuer:     {:?}", env.issuer);
    if let Some(kc) = &env.kubeconfig {
        println!("kubeconfig: {kc}");
    }
    Ok(())
}

#[cfg(test)]
pub mod scratch {
    use std::path::{Path, PathBuf};
    /// A scratch directory that removes itself even when the test fails.
    ///
    /// Cleanup as the last line of a test only runs when the test passes, so a
    /// failing test leaks its directory — which is exactly when you are running the
    /// suite repeatedly. Drop runs during unwind, so this cleans up either way.
    pub struct ScratchDirectory(pub PathBuf);

    impl ScratchDirectory {
        pub fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("dabba-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("creating a scratch directory");
            ScratchDirectory(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::backend::common::scratch::ScratchDirectory;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777
    }

    /// OpenBao's root token and unseal key go through here. A `write` followed by a
    /// `set_permissions` leaves them world-readable in between, so the mode has to
    /// be applied at creation.
    #[test]
    fn a_secret_file_is_never_readable_by_anyone_else() {
        let guard = ScratchDirectory::new("common-secret-mode");
        let path = guard.path().join("token");

        write_secret_file(&path, "s3cr3t").unwrap();
        assert_eq!(mode_of(&path), 0o600, "a fresh secret file must be 0600");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "s3cr3t");

        // A file that already exists with a loose mode must be tightened too — the
        // creation mode does not apply to an existing file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&path, "rotated").unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "an existing secret file must be tightened"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rotated");
    }

    /// Both callers must go through it, or the guarantee is only half true.
    #[test]
    fn both_secret_writers_produce_a_restricted_file() {
        let guard = ScratchDirectory::new("common-secret-writers");
        let workdir = guard.path();

        let generated = env_secret(workdir, "generated", false).unwrap();
        assert!(!generated.is_empty());
        assert_eq!(mode_of(&workdir.join("generated")), 0o600);

        write_stash(workdir, "stashed", "given-value").unwrap();
        assert_eq!(mode_of(&workdir.join("stashed")), 0o600);
    }
}
