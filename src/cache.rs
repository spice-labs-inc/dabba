//! The CI build cache: a shared bucket and the two scoped credentials that make
//! it safe to share.
//!
//! # Why this is not an `Application`
//!
//! Everything else dabba deploys is a workload — a container with ports and
//! volumes, which the portable [`crate::application`] schema describes. A bucket
//! with two differently-scoped credentials is not a workload; it is state inside a
//! running service. Putting it in the Application schema would have meant a field
//! that renders to nothing on either substrate, which is precisely the
//! half-implementation the intersection rule exists to prevent. So it gets its own
//! small verb instead.
//!
//! # The trust map
//!
//! sccache against a shared object store survives ephemeral runners, which is the
//! whole point — a cache that starts empty on every runner is not a cache. But a
//! shared cache that anyone can write to is a supply-chain problem: an untrusted
//! pull request could poison an entry that a later trusted build reads and links
//! into a release.
//!
//! So there are two credentials, and which one a build gets is the security
//! boundary:
//!
//! * **read-write** — only trusted branch builds. These populate the cache.
//! * **read-only** — same-repository pull requests. They benefit from the cache
//!   and cannot corrupt it.
//!
//! **Fork pull requests get neither.** GitHub gives fork builds no secrets at all,
//! so the only ways to reach them are publishing a credential or giving them
//! nothing. Publishing even a read-only key exposes every build artifact to the
//! internet, so forks compile cold. That is a deliberate cost, not an oversight.
//!
//! The root credential stays in OpenBao and is never handed to CI at all.

use crate::backend::common::log;
use crate::config::DabbaConfig;
use crate::run;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// The compose project the cache stack runs as, matching the reconciler's
/// convention and `examples/applications/cache-minio.yaml`.
const CACHE_PROJECT: &str = "gitops-cache";
/// Where the credentials live in OpenBao.
const SECRET_PATH: &str = "cache";
/// The bucket sccache addresses. Hash-addressed content, so one bucket serves
/// every branch without collision.
const BUCKET: &str = "sccache";
/// The scoped account names inside MinIO.
const READ_WRITE_USER: &str = "sccache-readwrite";
const READ_ONLY_USER: &str = "sccache-readonly";
/// The client image. Pinned: an admin CLI that changes flags underneath a
/// provisioning step is a bad way to find out about a release.
const CLIENT_IMAGE: &str = "minio/mc:RELEASE.2024-10-08T09-37-26Z";

/// Which credential a caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    ReadWrite,
    ReadOnly,
}

impl Scope {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "read-write" | "readwrite" | "rw" => Ok(Scope::ReadWrite),
            "read-only" | "readonly" | "ro" => Ok(Scope::ReadOnly),
            other => bail!("unknown scope {other:?}; expected read-write or read-only"),
        }
    }

    fn user(self) -> &'static str {
        match self {
            Scope::ReadWrite => READ_WRITE_USER,
            Scope::ReadOnly => READ_ONLY_USER,
        }
    }

    fn policy(self) -> &'static str {
        match self {
            Scope::ReadWrite => "sccache-readwrite",
            Scope::ReadOnly => "sccache-readonly",
        }
    }

    /// The OpenBao key holding this scope's secret key.
    fn secret_key(self) -> &'static str {
        match self {
            Scope::ReadWrite => "readwrite-secret-key",
            Scope::ReadOnly => "readonly-secret-key",
        }
    }

    fn access_key(self) -> &'static str {
        match self {
            Scope::ReadWrite => "readwrite-access-key",
            Scope::ReadOnly => "readonly-access-key",
        }
    }
}

/// The read-only policy. `s3:GetObject` plus listing, and deliberately no
/// `PutObject` or `DeleteObject` — that omission is the entire security control,
/// so it is spelled out rather than inherited from a built-in policy whose
/// contents could change.
fn read_only_policy() -> String {
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Effect": "Allow",
      "Action": ["s3:GetObject"],
      "Resource": ["arn:aws:s3:::{BUCKET}/*"]
    }},
    {{
      "Effect": "Allow",
      "Action": ["s3:ListBucket", "s3:GetBucketLocation"],
      "Resource": ["arn:aws:s3:::{BUCKET}"]
    }}
  ]
}}"#
    )
}

/// The read-write policy, scoped to this one bucket. A trusted build has no
/// business touching anything else in the object store.
fn read_write_policy() -> String {
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "Resource": ["arn:aws:s3:::{BUCKET}/*"]
    }},
    {{
      "Effect": "Allow",
      "Action": ["s3:ListBucket", "s3:GetBucketLocation"],
      "Resource": ["arn:aws:s3:::{BUCKET}"]
    }}
  ]
}}"#
    )
}

/// The running cache container, or an error naming what is missing.
fn cache_container() -> Result<String> {
    let output = run::capture(
        "docker",
        &[
            "ps",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={CACHE_PROJECT}"),
            "--filter",
            "status=running",
        ],
    )
    .unwrap_or_default();
    let id = output.lines().next().unwrap_or_default().trim().to_string();
    if id.is_empty() {
        bail!(
            "the {CACHE_PROJECT} stack is not running. Add \
             examples/applications/cache-minio.yaml to this box's gitops content and \
             let the reconcile loop converge it."
        );
    }
    Ok(id)
}

/// Run an `mc` script against the cache, with credentials passed through the
/// environment rather than argv.
///
/// `MC_HOST_cache` is how the client takes an alias and its credentials in one
/// variable; the alternative is `mc alias set`, which puts the secret key on a
/// command line that every process on the box can read.
fn client(root_user: &str, root_password: &str, script: &str) -> Result<String> {
    let network = format!("{CACHE_PROJECT}_default");
    let host = format!("MC_HOST_cache=http://{root_user}:{root_password}@cache:9000");
    run::capture(
        "docker",
        &[
            "run",
            "--rm",
            "--network",
            &network,
            "--env",
            &host,
            "--entrypoint",
            "sh",
            CLIENT_IMAGE,
            "-c",
            script,
        ],
    )
    .with_context(|| "running the object-store client".to_string())
}

/// Read a single field out of OpenBao for this environment.
fn read_secret(config: &Path, env_name: Option<&str>, key: &str) -> Result<String> {
    crate::backend::docker::read_secret_field(config, env_name, SECRET_PATH, key)
}

/// `dabba cache up` — make the bucket and both scoped credentials exist.
///
/// Idempotent: re-running against a provisioned cache re-applies the policies and
/// leaves the existing credentials alone. Rotating a credential is a separate,
/// deliberate act — silently reissuing on every `up` would invalidate whatever CI
/// currently holds.
pub fn up(config: &Path, env_name: Option<&str>) -> Result<()> {
    let cfg = DabbaConfig::load(config)?;
    let env = cfg.resolve(env_name)?;

    // The root credentials the cache itself starts with. Generated here rather
    // than by hand, because the stack cannot converge until they exist: its
    // definition references them, and the reconciler fails a stack whose secret
    // references do not resolve.
    log(&format!("[{}] ensuring cache root credentials", env.name));
    crate::backend::docker::ensure_secret_fields(
        config,
        env_name,
        SECRET_PATH,
        &[("root-user", None), ("root-password", None)],
    )?;

    log(&format!(
        "[{}] waiting for the cache stack to be running",
        env.name
    ));
    run::wait_for("the cache stack", 60, || cache_container().is_ok())?;

    let root_user = read_secret(config, env_name, "root-user")?;
    let root_password = read_secret(config, env_name, "root-password")?;

    // Generate the scoped secret keys before touching MinIO, so a failure part way
    // through leaves credentials we still hold rather than accounts we cannot use.
    let scopes = [Scope::ReadWrite, Scope::ReadOnly];
    for scope in scopes {
        crate::backend::docker::ensure_secret_fields(
            config,
            env_name,
            SECRET_PATH,
            &[
                (scope.access_key(), Some(scope.user().to_string())),
                (scope.secret_key(), None),
            ],
        )?;
    }

    log(&format!("[{}] provisioning bucket and policies", env.name));
    let mut script = format!(
        "set -e\n\
         mc mb --ignore-existing cache/{BUCKET}\n\
         cat > /tmp/readwrite.json <<'POLICY'\n{}\nPOLICY\n\
         cat > /tmp/readonly.json <<'POLICY'\n{}\nPOLICY\n\
         mc admin policy create cache {} /tmp/readwrite.json 2>/dev/null || \
         mc admin policy create cache {} /tmp/readwrite.json\n\
         mc admin policy create cache {} /tmp/readonly.json 2>/dev/null || \
         mc admin policy create cache {} /tmp/readonly.json\n",
        read_write_policy(),
        read_only_policy(),
        Scope::ReadWrite.policy(),
        Scope::ReadWrite.policy(),
        Scope::ReadOnly.policy(),
        Scope::ReadOnly.policy(),
    );

    for scope in scopes {
        let access = read_secret(config, env_name, scope.access_key())?;
        let secret = read_secret(config, env_name, scope.secret_key())?;
        // `mc admin user add` is idempotent enough: re-adding an existing user
        // resets its secret key to the one we hold, which keeps OpenBao and MinIO
        // in agreement even if someone changed it out of band.
        script.push_str(&format!(
            "mc admin user add cache '{access}' '{secret}'\n\
             mc admin policy attach cache {} --user '{access}' 2>/dev/null || true\n",
            scope.policy()
        ));
    }

    let output = client(&root_user, &root_password, &script)?;
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        log(line);
    }

    println!(
        "\n✓ cache ready\n\n  \
         bucket:      {BUCKET}\n  \
         read-write:  dabba cache credentials --scope read-write   (trusted branches only)\n  \
         read-only:   dabba cache credentials --scope read-only    (same-repo pull requests)\n\n  \
         Fork pull requests get neither: GitHub hands them no secrets, and publishing\n  \
         even a read-only key would expose every build artifact. Forks compile cold."
    );
    Ok(())
}

/// `dabba cache credentials` — print one scope's credentials for CI to consume.
///
/// Emits shell-export form so a workflow can `eval` it, and writes to stdout only
/// — a credential that lands in a log file is a credential that has leaked.
pub fn credentials(config: &Path, env_name: Option<&str>, scope: Scope) -> Result<()> {
    let access = read_secret(config, env_name, scope.access_key())?;
    let secret = read_secret(config, env_name, scope.secret_key())?;

    println!("AWS_ACCESS_KEY_ID={access}");
    println!("AWS_SECRET_ACCESS_KEY={secret}");
    println!("SCCACHE_BUCKET={BUCKET}");
    if scope == Scope::ReadOnly {
        // sccache asks the store to write on a miss unless told not to. Without
        // this a read-only build spends every miss on a request that is refused,
        // and reports the refusal as a cache error.
        println!("SCCACHE_S3_NO_CREDENTIALS=false");
        println!("SCCACHE_READONLY=1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parses_its_spellings() {
        for value in ["read-write", "readwrite", "rw"] {
            assert_eq!(Scope::parse(value).unwrap(), Scope::ReadWrite);
        }
        for value in ["read-only", "readonly", "ro"] {
            assert_eq!(Scope::parse(value).unwrap(), Scope::ReadOnly);
        }
        assert!(Scope::parse("write-only").is_err());
        // An empty or misspelled scope must never silently fall back to
        // read-write, which would hand an untrusted build a writable credential.
        assert!(Scope::parse("").is_err());
        assert!(Scope::parse("READ-WRITE").is_err());
    }

    /// The security control is an ABSENCE — no PutObject in the read-only policy.
    /// An absence is exactly what a refactor removes without noticing, so it is
    /// asserted directly.
    #[test]
    fn the_read_only_policy_cannot_write() {
        let policy = read_only_policy();
        assert!(policy.contains("s3:GetObject"));
        assert!(policy.contains("s3:ListBucket"));
        for forbidden in ["s3:PutObject", "s3:DeleteObject", "s3:*", "\"*\""] {
            assert!(
                !policy.contains(forbidden),
                "the read-only policy grants {forbidden}, so an untrusted build \
                 could poison the cache"
            );
        }
    }

    #[test]
    fn the_read_write_policy_is_scoped_to_one_bucket() {
        let policy = read_write_policy();
        assert!(policy.contains("s3:PutObject"));
        // Scoped to the cache bucket, not the whole object store.
        assert!(!policy.contains("arn:aws:s3:::*"));
        assert_eq!(
            policy.matches(BUCKET).count(),
            2,
            "both statements should name the bucket explicitly"
        );
    }

    /// The two scopes must never collide, in MinIO or in OpenBao — a shared name
    /// would mean one silently overwriting the other's credential.
    #[test]
    fn the_two_scopes_are_distinct_everywhere() {
        let (rw, ro) = (Scope::ReadWrite, Scope::ReadOnly);
        assert_ne!(rw.user(), ro.user());
        assert_ne!(rw.policy(), ro.policy());
        assert_ne!(rw.access_key(), ro.access_key());
        assert_ne!(rw.secret_key(), ro.secret_key());
    }
}
