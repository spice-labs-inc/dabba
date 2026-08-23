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
| Provisioning | kind / k3d / minikube; bring-your-own or managed cloud; or a bare box running docker compose | `substrate:` |
| GitOps | FluxCD, syncing from an in-cluster git server | — |
| Gateway | Envoy Gateway (Gateway API) | a gitops component |
| TLS | cert-manager, self-signed CA locally (ACME in the cloud) | `tls.issuer:` |
| Secrets | External Secrets + [OpenBao](https://openbao.org/), per-env random | `secrets.backend:` |
| Observability | Vector → OpenObserve + an OTel collector (opt-in) | `observability:` |
| Demo | [podinfo](https://github.com/stefanprodan/podinfo) | `useCases:` |

## Without Kubernetes

Not every deployment earns a cluster. `substrate: docker-host` runs on a plain
box with Docker on it — no Kubernetes, no control plane, no API server. A
per-box loop pulls the gitops repository every minute and converges the host's
`docker compose` stacks to it, which is the same idea Flux implements, minus the
cluster it needs to run in.

```bash
dabba env box up -c examples/docker-host.yaml --gitops-seed ./my-gitops
```

The trade is explicit. You give up scheduling, self-healing beyond restart
policies, multi-node anything, and a cluster's worth of primitives. You get a
resource floor of "your containers, plus a shell script that runs for a second a
minute", and one fewer distributed system to operate. Losing the loop degrades
to "no new deploys" rather than an outage, because Docker's restart policy is
the supervisor — the reconciler only ever delivers change.

`down` reflects that: it stops the loop and leaves your stacks running.
Decommissioning is a deliberate act, not a side effect of turning off the thing
that deploys.

## One application, either substrate

An application is written once and rendered for whichever substrate it lands on:

```bash
dabba application example > app.yaml
dabba application render app.yaml --substrate kind          # Kubernetes objects
dabba application render app.yaml --substrate docker-host   # a compose file
```

The shared schema is the **intersection** of what both runtimes genuinely
honour, not the union. A field that cannot render meaningfully on both sides is
not in the schema at all, and a conformance test fails the build if either
renderer stops honouring one — so a field cannot quietly work on one substrate
and do nothing on the other.

Some things are genuinely substrate-specific, and those go in explicit
`kubernetes:` and `dockerHost:` blocks. `dabba application portability` reports
which of them a given substrate will ignore, rather than dropping them silently.

Some things are permanently out of scope because no compose equivalent exists:
autoscaling, NetworkPolicy, PodDisruptionBudgets, multi-node scheduling, service
mesh. Those are named in the schema so their absence reads as a decision.

**Gitops content itself is not portable.** A Kubernetes substrate consumes
kustomizations and HelmReleases; a compose host consumes
`apps/<box>/<app>/docker-compose.yml`. Moving an environment between them is a
migration, not a change to one line.

## The CI build cache

CI runs one portable entrypoint on a laptop, a GitHub runner, or a remote dabba.
For a cache to serve all three it has to live in an object store, because
GitHub's own cache only exists on GitHub — and a cache that starts empty on
every ephemeral runner is not a cache at all.

```bash
dabba cache up                                    # bucket + both credentials
dabba cache credentials --scope read-write        # trusted branches
dabba cache credentials --scope read-only         # same-repo pull requests
```

A shared cache anyone can write to is a supply-chain problem: an untrusted pull
request could poison an entry a later trusted build links into a release. So
there are two credentials and the difference between them is the security
boundary — read-write populates the cache, read-only benefits from it and cannot
corrupt it. The root credential stays in OpenBao and never reaches CI.

**Fork pull requests get neither, deliberately.** GitHub hands fork builds no
secrets, so the only options are publishing a credential or giving them nothing.
Publishing even a read-only key exposes every build artifact to the internet.
Forks compile cold; that is the accepted cost.

Where to run it matters more than it looks. Put the cache on the long-lived
substrate and point CI at it; put test dependencies — a Postgres for the suite —
on the ephemeral runner, where fresh and isolated is what you want. Same schema,
opposite lifetimes, which is why they are separate environments rather than one:

```yaml
environments:
  - name: ci-runner       # ephemeral, on the runner
    substrate: docker-host
  - name: shared-cache    # long-lived
    substrate: scaleway-kapsule
```

## How it fits together

```
dabba (this repo)        the CLI, the quickstart, the docs
dabba-modules            OpenTofu modules (kind / k3d / minikube / eks-fargate, git server, flux operator)
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
- **Tier 1 — a cloud environment**: the same definition on AWS Fargate EKS (`substrate: eks`),
  with a cloud overlay — real DNS via external-dns, ACME certificates through Route53, and a
  load-balancer gateway.
- **Tier 2 — scaling up**: PR-driven, multi-environment delivery. *(in progress)*

Full documentation: **[spice-labs-inc.github.io/dabba](https://spice-labs-inc.github.io/dabba/)**.

## Contributing & license

Issues are welcome. Feature PRs need a prior issue; reviews may be slow — dabba is maintained
as part of our own infrastructure and shared in the hope it is useful. See
[CONTRIBUTING.md](CONTRIBUTING.md). Apache-2.0 — see [LICENSE](LICENSE).

---

A [Spice Labs](https://spicelabs.io) project. © 2026 Spice Labs, Inc. &amp; Contributors.
