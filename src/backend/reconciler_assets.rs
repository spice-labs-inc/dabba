//! The reconciler's files, embedded in the dabba binary.
//!
//! `backends/docker/` is a directory of bash scripts and unit templates, but dabba
//! ships as a single binary from a GitHub Release (see the repository's
//! `install.sh`). An installed dabba therefore has no `backends/docker/` on disk at
//! all, and resolving it relative to the working directory only ever worked when
//! dabba was run from a git checkout.
//!
//! So the files are compiled in with [`include_str!`] and written out on demand by
//! [`materialize`]. Two properties fall out of that and both matter:
//!
//! * The scripts are always the ones this binary was built with, so a reconciler
//!   and the CLI driving it can never be different versions.
//! * Upgrading dabba and re-running `up` rewrites the scripts in place at the same
//!   absolute path, so the installed LaunchAgent or systemd unit — which points at
//!   that path — picks the new content up without being reinstalled.
//!
//! `substrateConfig.backendDir` still overrides this, pointing at a working copy of
//! `backends/docker/` instead. That is for developing the reconciler itself, where
//! editing a script and re-running is the whole loop.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where the reconciler is written inside an environment's working directory.
pub const RECONCILER_DIRECTORY_NAME: &str = "reconciler";

/// One embedded file: its path relative to `backends/docker/`, its contents, and
/// whether it needs the executable bit when written out.
struct Asset {
    relative_path: &'static str,
    contents: &'static str,
    executable: bool,
}

/// Everything under `backends/docker/` that the reconciler needs at run time. The
/// `examples/`, `test/` and `README.md` entries are deliberately absent: they are
/// for people reading the repository, and embedding them would put content in every
/// installed binary that nothing ever executes.
const ASSETS: &[Asset] = &[
    Asset {
        relative_path: "reconcile.sh",
        contents: include_str!("../../backends/docker/reconcile.sh"),
        executable: true,
    },
    Asset {
        relative_path: "reconcile-alert.sh",
        contents: include_str!("../../backends/docker/reconcile-alert.sh"),
        executable: true,
    },
    Asset {
        relative_path: "reconcile-with-alerting.sh",
        contents: include_str!("../../backends/docker/reconcile-with-alerting.sh"),
        executable: true,
    },
    Asset {
        relative_path: "install.sh",
        contents: include_str!("../../backends/docker/install.sh"),
        executable: true,
    },
    Asset {
        relative_path: "uninstall.sh",
        contents: include_str!("../../backends/docker/uninstall.sh"),
        executable: true,
    },
    Asset {
        relative_path: "launchd/io.spicelabs.dabba.reconcile.plist.template",
        contents: include_str!(
            "../../backends/docker/launchd/io.spicelabs.dabba.reconcile.plist.template"
        ),
        executable: false,
    },
    Asset {
        relative_path: "systemd/gitops-reconcile.service.template",
        contents: include_str!("../../backends/docker/systemd/gitops-reconcile.service.template"),
        executable: false,
    },
    Asset {
        relative_path: "systemd/gitops-reconcile-alert.service.template",
        contents: include_str!(
            "../../backends/docker/systemd/gitops-reconcile-alert.service.template"
        ),
        executable: false,
    },
    Asset {
        relative_path: "systemd/gitops-reconcile.timer.template",
        contents: include_str!("../../backends/docker/systemd/gitops-reconcile.timer.template"),
        executable: false,
    },
];

/// The scripts that must be present for a directory to be a usable reconciler.
/// Shared with the `backendDir` override path so a wrong path fails the same way
/// whether the files were embedded or checked out.
pub const REQUIRED_SCRIPTS: &[&str] = &["reconcile.sh", "install.sh", "uninstall.sh"];

/// Write the embedded reconciler into `parent/reconciler/`, returning that path.
///
/// Rewrites a file only when its contents differ, so the common case — re-running
/// `up` on an environment that is already converged — touches nothing and leaves
/// mtimes alone.
pub fn materialize(parent: &Path) -> Result<PathBuf> {
    let root = parent.join(RECONCILER_DIRECTORY_NAME);
    for asset in ASSETS {
        let path = root.join(asset.relative_path);
        let directory = path
            .parent()
            .expect("every asset path has a parent under the reconciler root");
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;

        let already_current = std::fs::read_to_string(&path)
            .map(|existing| existing == asset.contents)
            .unwrap_or(false);
        if !already_current {
            std::fs::write(&path, asset.contents)
                .with_context(|| format!("writing {}", path.display()))?;
        }

        #[cfg(unix)]
        if asset.executable {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
            if mode.map(|m| m != 0o755).unwrap_or(true) {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("making {} executable", path.display()))?;
            }
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common::scratch::ScratchDirectory;

    /// The embedded copies must be the real scripts, not empty or truncated. A
    /// silently empty asset would install a reconcile loop that does nothing.
    #[test]
    fn every_asset_has_real_content() {
        for asset in ASSETS {
            assert!(
                asset.contents.len() > 100,
                "{} embedded with only {} bytes",
                asset.relative_path,
                asset.contents.len()
            );
        }
    }

    /// Each shell asset must be a script, and each unit template a unit — catching
    /// an `include_str!` path that was repointed at the wrong file.
    #[test]
    fn assets_are_the_files_they_claim_to_be() {
        for asset in ASSETS {
            if asset.relative_path.ends_with(".sh") {
                assert!(
                    asset.contents.starts_with("#!/usr/bin/env bash"),
                    "{} is not a bash script",
                    asset.relative_path
                );
                assert!(
                    asset.executable,
                    "{} must be executable",
                    asset.relative_path
                );
            } else {
                assert!(
                    !asset.executable,
                    "{} is not a script and must not be executable",
                    asset.relative_path
                );
            }
        }
        let plist = ASSETS
            .iter()
            .find(|a| a.relative_path.ends_with(".plist.template"))
            .expect("the launchd plist template is embedded");
        assert!(plist.contents.contains("<plist version=\"1.0\">"));
    }

    /// Everything `docker_backend_dir` insists on must actually be embedded,
    /// otherwise a materialized reconciler would fail its own validation.
    #[test]
    fn required_scripts_are_all_embedded() {
        for script in REQUIRED_SCRIPTS {
            assert!(
                ASSETS.iter().any(|a| a.relative_path == *script),
                "{script} is required but not embedded"
            );
        }
    }

    #[test]
    fn materialize_writes_an_executable_tree() {
        let guard = ScratchDirectory::new("materialize-writes");
        let scratch = guard.path().to_path_buf();
        let root = materialize(&scratch).unwrap();

        assert_eq!(root, scratch.join(RECONCILER_DIRECTORY_NAME));
        for asset in ASSETS {
            let path = root.join(asset.relative_path);
            assert!(path.is_file(), "{} was not written", asset.relative_path);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), asset.contents);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o111;
                assert_eq!(
                    mode != 0,
                    asset.executable,
                    "{} has the wrong executable bit",
                    asset.relative_path
                );
            }
        }
    }

    /// Upgrading dabba must replace a stale script at the same path, so the
    /// installed unit picks the new content up without being reinstalled.
    #[test]
    fn materialize_repairs_a_stale_or_damaged_script() {
        let guard = ScratchDirectory::new("materialize-repairs");
        let scratch = guard.path().to_path_buf();
        let root = materialize(&scratch).unwrap();
        let reconcile = root.join("reconcile.sh");

        std::fs::write(&reconcile, "#!/usr/bin/env bash\nexit 0\n").unwrap();
        materialize(&scratch).unwrap();

        let restored = std::fs::read_to_string(&reconcile).unwrap();
        assert!(
            restored.contains("GITOPS_DIR"),
            "a stale reconcile.sh was not rewritten"
        );
    }

    /// Re-running `up` on a converged environment must not rewrite files.
    #[test]
    fn materialize_leaves_current_files_untouched() {
        let guard = ScratchDirectory::new("materialize-idempotent");
        let scratch = guard.path().to_path_buf();
        let root = materialize(&scratch).unwrap();
        let reconcile = root.join("reconcile.sh");
        let before = std::fs::metadata(&reconcile).unwrap().modified().unwrap();

        materialize(&scratch).unwrap();

        let after = std::fs::metadata(&reconcile).unwrap().modified().unwrap();
        assert_eq!(before, after, "an unchanged script was rewritten");
    }
}
