//! Small process helpers for shelling out to tofu/kubectl/git/docker/curl —
//! the same tools the bash quickstart drove, now invoked from Rust.

use anyhow::{bail, Context, Result};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Run a command inheriting stdout/stderr; error on non-zero exit.
pub fn run(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("spawning {bin}"))?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Run with stdout/stderr suppressed; error on non-zero exit.
pub fn run_quiet(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawning {bin}"))?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Run, ignoring any failure (for best-effort nudges like reconcile annotations).
pub fn try_run(bin: &str, args: &[&str]) {
    let _ = Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// True if the command exits 0 (output suppressed). For `wait_for` probes.
pub fn probe(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run and return stdout (None on spawn failure or non-zero exit).
pub fn capture(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Spawn a detached background process (e.g. a port-forward).
pub fn spawn(bin: &str, args: &[&str]) -> Result<Child> {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {bin}"))
}

/// Poll until `probe` returns true, or give up after attempts*5s (default ~10m).
pub fn wait_for<F: Fn() -> bool>(desc: &str, attempts: usize, probe: F) -> Result<()> {
    for _ in 0..attempts {
        if probe() {
            return Ok(());
        }
        sleep(Duration::from_secs(5));
    }
    bail!("timed out waiting for: {desc}")
}
