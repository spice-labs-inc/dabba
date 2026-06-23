# Security Policy

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report privately through
GitHub: on the repository's **Security** tab, choose **Report a vulnerability** (GitHub
private vulnerability reporting). We'll acknowledge the report and work a fix with you before
any public disclosure.

## Supported versions

Security fixes target the latest released version. dabba is pre-1.0; pin a release tag and
upgrade to pick up fixes.

## Security model

dabba is **secure by default for what it is** — a platform you bring up yourself — but the
local (tier-0) defaults are tuned for a laptop, not production. Know these tradeoffs:

- **No shipped credentials.** dabba generates every secret per-environment at `up` (random,
  via `/dev/urandom`) and stores them in OpenBao. The one local secret it stashes — the OpenBao
  root token — is written `0600` under `.dabba/<env>/`, which is git-ignored. Nothing
  sensitive is committed. Retrieve secrets with `dabba secret get`.
- **OpenBao runs in dev mode locally** (in-memory, unsealed, root token injected via a
  `cluster-vars` ConfigMap in plaintext). This is fine for a throwaway local cluster and
  **local clusters only** — cloud overlays use an externally-managed OpenBao/Vault with a real
  seal. Don't put real secrets in a dev-mode instance.
- **TLS is a self-signed CA locally** (browsers warn, as expected). Cloud overlays use ACME.
- **The in-cluster git server (Forgejo) is the authoritative gitops source.** Locally it's a
  single ephemeral pod backed by SQLite with no backup; losing the pod loses gitops history.
  It's reachable only in-cluster / via port-forward. For durable use, push-mirror it or point
  Flux at an external git host.
- **Install integrity.** Each release publishes, per binary, a **SHA-256 checksum** and a signed
  **build provenance attestation** (GitHub/sigstore, keyless), plus an **SBOM** for the release.
  `install.sh` verifies the checksum before installing. You can also verify provenance yourself:

  ```bash
  gh attestation verify dabba --repo spice-labs-inc/dabba
  ```

## Scope

This policy covers the `dabba` CLI in this repository. The platform it deploys lives in
[`dabba-modules`](https://github.com/spice-labs-inc/dabba-modules) and
[`dabba-gitops`](https://github.com/spice-labs-inc/dabba-gitops); report issues there the same
way (or here — we'll route it).
