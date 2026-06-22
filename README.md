# 🍱 dabba

[![GitHub Release](https://img.shields.io/github/v/release/spice-labs-inc/dabba?label=Release)](https://github.com/spice-labs-inc/dabba/releases)
[![CI](https://github.com/spice-labs-inc/dabba/actions/workflows/ci.yml/badge.svg)](https://github.com/spice-labs-inc/dabba/actions/workflows/ci.yml)
[![quickstart](https://github.com/spice-labs-inc/dabba/actions/workflows/quickstart.yml/badge.svg)](https://github.com/spice-labs-inc/dabba/actions/workflows/quickstart.yml)
[![Docs](https://img.shields.io/badge/docs-dabba-1FDB7D)](https://spice-labs-inc.github.io/dabba/)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)

dabba brings up a full Kubernetes platform from a single config file — the same way on a
laptop and on managed cloud. `dabba up` reads the config, provisions a cluster, and brings up
the platform on it: gateway, TLS, secrets, gitops, and your apps.

It ships no credentials — every secret is generated per-environment and kept in your own
store — and `dabba up` waits for the platform to actually reconcile before it reports success.
Components are defaults you change in the config, not forks you maintain.

## Quickstart

dabba drives Docker, [OpenTofu](https://opentofu.org/), and kubectl (`dabba doctor` checks
they're on your PATH). No cloud account, no secret manager, nothing to sign up for.

Install the CLI:

```bash
curl -fsSL https://raw.githubusercontent.com/spice-labs-inc/dabba/main/install.sh | bash
```

On Windows: `irm https://raw.githubusercontent.com/spice-labs-inc/dabba/main/install.ps1 | iex`.
Or build from source with `cargo install --git https://github.com/spice-labs-inc/dabba`.

Then bring up the local quickstart:

```bash
git clone https://github.com/spice-labs-inc/dabba.git
cd dabba
dabba up -c examples/local.yaml
```

A few minutes later you have a platform on a [kind](https://kind.sigs.k8s.io/) cluster (the
default `kind` environment):

- **https://podinfo.localtest.me:31443** — the demo app. The message on its banner was written
  into OpenBao and delivered to the app by External Secrets, so seeing it confirms the chain
  (gateway → TLS → External Secrets → OpenBao → gitops) works end to end.
- **https://bao.localtest.me:31443** — the OpenBao UI. There's no default token; get this
  environment's root token with `dabba secret get local/openbao-root`.

`*.localtest.me` resolves to `127.0.0.1`, so there's nothing to add to `/etc/hosts`. The
gateway listens on `31443` (a high port, so dabba doesn't contend for `443` on your machine).
TLS is a self-signed CA, so browsers will warn locally.

The config ships three local environments (`kind`/`k3d`/`minikube`); `dabba ls` lists them and
`dabba env k3d up` brings up a different one. `dabba status` reports what's running, and
`dabba down -c examples/local.yaml` tears it down.

## What you get

| Layer | Default | Change it with |
|-------|---------|----------------|
| Provisioning | kind / k3d / minikube; bring-your-own or managed cloud | `substrate:` |
| GitOps | FluxCD, syncing from an in-cluster git server | — |
| Gateway | Envoy Gateway (Gateway API) | a gitops component |
| TLS | cert-manager, self-signed CA locally (ACME in the cloud) | `tls.issuer:` |
| Secrets | External Secrets + [OpenBao](https://openbao.org/), per-env random | `secrets.backend:` |
| Observability | Vector → OpenObserve + an OTel collector (opt-in) | `observability:` |
| Demo | [podinfo](https://github.com/stefanprodan/podinfo) | `useCases:` |

## How it fits together

```
dabba (this repo)        the CLI, the quickstart, the docs
dabba-modules            OpenTofu modules (kind / k3d / minikube, git server, flux operator)
dabba-gitops             the platform as gitops (clusters / crds / platform / use-cases)
```

`dabba up` reads your config, provisions a cluster, and points the GitOps engine at the
platform definition; the cluster then reconciles itself from git. The platform's structure
lives in [dabba-gitops](https://github.com/spice-labs-inc/dabba-gitops); the OpenTofu that
wires it onto a cluster lives in `dabba-modules` and the local `quickstart/`. OpenTofu only
stamps a `cluster-vars` ConfigMap and aims gitops at it — the cluster is self-describing, so
the same definition runs unchanged from laptop to cloud. The two quickstart steps are small
enough to read and run by hand; the CLI is a convenience over them.

## Built on

- **[OpenTofu](https://opentofu.org/)** — provisions the cluster.
- **[FluxCD](https://fluxcd.io/), via the [Flux Operator](https://github.com/controlplaneio-fluxcd/flux-operator)** —
  reconciles the platform and your apps from git. Flux's own lifecycle is declared by a
  `FluxInstance`, so provisioning never hand-writes Flux objects.
- **[OpenBao](https://openbao.org/)** — the secret store; External Secrets delivers secrets to apps.
- **In-cluster git** — a self-contained authoritative source the cluster reconciles from,
  pluggable to GitHub or a shared-services hub.

## Beyond the laptop

dabba is built in tiers; every tier uses the same modules and the same gitops repo:

- **Tier 0 — local**: the quickstart above. The real platform, just small.
- **Tier 1 — a cloud environment**: the same definition against a managed cluster, with a cloud
  overlay (real DNS, ACME certs, an external OpenBao). *(in progress)*
- **Tier 2 — scaling up**: PR-driven, multi-environment delivery. *(in progress)*

Full documentation: **[spice-labs-inc.github.io/dabba](https://spice-labs-inc.github.io/dabba/)**.

## Contributing & license

Issues are welcome. Feature PRs need a prior issue; reviews may be slow — dabba is maintained
as part of our own infrastructure and shared in the hope it is useful. See
[CONTRIBUTING.md](CONTRIBUTING.md). Apache-2.0 — see [LICENSE](LICENSE).

---

A [Spice Labs](https://spicelabs.io) project. © 2026 Spice Labs, Inc. &amp; Contributors.
