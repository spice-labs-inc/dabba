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

/// Run and return stdout, putting the command's OWN output in the error when it
/// fails.
///
/// [`capture`] throws stderr away, so a failure arrives as whatever context the
/// caller attached and nothing else. `bao operator init` refusing to write its data
/// directory surfaced as exactly that: "running `bao operator init`", with the
/// actual cause — a permission error naming a path inside the container —
/// discarded. The reconciler already captures compose's stderr for the same
/// reason; this is that lesson on this side.
pub fn capture_explaining_failure(bin: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("spawning {bin}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    // Some tools explain themselves on stdout, some on stderr. Report whatever
    // came back rather than guessing which.
    let mut detail = String::new();
    for stream in [&out.stderr, &out.stdout] {
        let text = String::from_utf8_lossy(stream);
        if !text.trim().is_empty() {
            detail.push_str(text.trim());
            detail.push('\n');
        }
    }
    if detail.is_empty() {
        detail.push_str("(the command printed nothing)");
    }
    bail!("`{bin} {}` failed:\n{}", args.join(" "), detail.trim_end())
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

/// A value quoted for a POSIX shell.
///
/// The interesting case is the apostrophe: single quoting cannot contain one, so the
/// quoting is closed, an escaped quote emitted, and the quoting reopened. Anything
/// that interpolates a value into a command string needs this — a secret out of the
/// store, a health command out of a gitops repo.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The health command the compose renderer builds is a string, spliced together
    /// from parts that came out of a git repository and run with `bash -c` on the
    /// box. Single quoting alone does not survive an apostrophe: the quoting ends
    /// early and whatever follows is read as shell.
    #[test]
    fn a_quoted_value_cannot_break_out_of_the_command_it_is_spliced_into() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);

        // A substring check would be the wrong test: correct quoting still CONTAINS
        // the dangerous text, safely inside the quotes. The only honest check is to
        // run it. The argument the command sees must be the original value byte for
        // byte, which it cannot be if anything else executed.
        let hostile = "x'; touch /tmp/dabba-should-not-exist; echo '";
        let script = format!("printf '%s' {}", shell_quote(hostile));
        let seen = capture("sh", &["-c", &script]).expect("the shell ran");
        assert_eq!(seen, hostile, "quoting changed the value");
    }

    /// A failing command has to explain itself. `capture` discards stderr, so
    /// `bao operator init` refusing to write its data directory arrived as nothing
    /// but the caller's own context string, and the permission error naming the
    /// path was thrown away.
    #[test]
    fn a_failing_command_reports_what_it_actually_said() {
        let error = capture_explaining_failure("sh", &["-c", "echo the-real-cause >&2; exit 1"])
            .expect_err("a non-zero exit must be an error");
        assert!(
            error.to_string().contains("the-real-cause"),
            "the command's own stderr was discarded: {error}"
        );

        // Some tools explain themselves on stdout instead.
        let error = capture_explaining_failure("sh", &["-c", "echo said-on-stdout; exit 1"])
            .expect_err("a non-zero exit must be an error");
        assert!(
            error.to_string().contains("said-on-stdout"),
            "stdout was discarded: {error}"
        );

        // And success still returns the output.
        let ok = capture_explaining_failure("sh", &["-c", "echo fine"]).unwrap();
        assert_eq!(ok.trim(), "fine");
    }
}
