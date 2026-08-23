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

/// Run with extra environment variables applied to the child only, inheriting
/// stdout/stderr; error on non-zero exit.
///
/// A `None` value removes the variable from the child's environment instead of
/// letting it be inherited — the difference matters when an unset variable is
/// itself meaningful to the child (see `backend::docker::reconciler_environment`).
/// Setting these on the child rather than on this process keeps the configuration
/// out of global state, where it would otherwise outlive the call.
pub fn run_with_environment(
    bin: &str,
    args: &[&str],
    environment: &[(String, Option<String>)],
) -> Result<()> {
    let mut command = Command::new(bin);
    command.args(args);
    for (key, value) in environment {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let status = command
        .status()
        .with_context(|| format!("spawning {bin}"))?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Run with `input` written to stdin (so a secret travels via stdin, not the argv
/// that `ps` / /proc exposes); inherit stdout/stderr; error on non-zero exit.
pub fn run_stdin(bin: &str, args: &[&str], input: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {bin}"))?;
    child
        .stdin
        .take()
        .context("capturing stdin")?
        .write_all(input.as_bytes())
        .with_context(|| format!("writing stdin to {bin}"))?;
    let status = child.wait().with_context(|| format!("waiting for {bin}"))?;
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

/// Run and return stdout regardless of exit status (None only if it cannot spawn).
///
/// Some tools use the exit code to report state rather than failure: `bao status`
/// exits 2 when the vault is sealed, which is information we need, not an error.
/// [`capture`] would discard the output in exactly that case.
pub fn capture_including_failures(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run with `input` on stdin and return stdout regardless of exit status.
///
/// The combination `capture` and `run_stdin` cannot express: a command that needs
/// a secret on stdin AND whose non-zero exit is information rather than failure
/// (reading a secret that does not exist yet).
pub fn capture_stdin_including_failures(bin: &str, args: &[&str], input: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
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
