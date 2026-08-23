---
title: Configuration
description: The DabbaConfig file — one config, many environments.
---

A dabba config is a `DabbaConfig` document. Shared platform settings live at `spec`; each
**environment** overrides what differs (substrate, and optionally domain or kubeconfig). It
mirrors a kubeconfig: one file, many environments, a default, select with `dabba env <name>`.

```yaml
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
metadata:
  name: dabba
spec:
  domain: localtest.me # shared across environments
  tls:
    issuer: selfsigned
  defaultEnvironment: kind
  environments:
    - { name: kind, substrate: kind }
    - { name: k3d, substrate: k3d }
    - { name: minikube, substrate: minikube }
  observability:
    enabled: true # Vector → OpenObserve + OTEL traces (opt-in)
  useCases:
    - demo
```

## Environments

Each entry under `environments` is a named managed boundary (one cluster today; multi-cluster is
on the roadmap):

| Field | Meaning |
|-------|---------|
| `name` | The environment's identity — also its cluster name and the `${environment}` substitution. |
| `substrate` | `kind` · `k3d` · `minikube` · `eks` (AWS Fargate) · `scaleway-kapsule` · `existing` (bring-your-own kubeconfig) · `docker-host` (a bare box running docker compose, no Kubernetes at all). |
| `domain` | Optional per-env override of the shared `spec.domain`. |
| `kubeconfig` | Required when `substrate: existing` — path to the cluster's kubeconfig. |
| `substrateConfig` | Per-substrate settings — see the table below. |

### `substrateConfig` by substrate

| Substrate | Keys |
|-----------|------|
| `eks` | `region`, `k8sVersion`, `route53ZoneId`; optionally `vpcId` + `privateSubnetIds` + `publicSubnetIds` to reuse an existing VPC |
| `scaleway-kapsule` | `region`, `zone` (must be inside the region), `k8sVersion`, `nodeType`, `nodeCount`, `autoscaling`, `maxNodeCount`, `privateNetworkId` |
| `docker-host` | `boxName` (defaults to `hostname -s`), `appsDir`, `gitopsBranch` |
| local substrates | none |

## Without Kubernetes: `docker-host`

`substrate: docker-host` runs on a plain box with Docker on it. A per-box loop
pulls the gitops repository every minute and converges the host's
`docker compose` stacks to it — the same idea Flux implements, without the
cluster it needs to run in.

What it does **not** share with the Kubernetes substrates is worth knowing
before choosing it:

- `spec.tls`, `spec.gateway`, `spec.observability` and `spec.useCases` drive the
  Kubernetes reconcile layer and are not consumed here.
- Gitops content is a different artifact format entirely: a compose host reads
  `apps/<box>/<app>/docker-compose.yml`, not kustomizations and HelmReleases. One
  repository cannot serve both, so moving an environment across is a migration
  rather than a change to one line.
- `kubeconfig` and `diagram` are meaningless on it.
- `down` inverts: it stops the reconcile loop and **leaves the stacks running**.
  Decommissioning is a deliberate act, not a side effect of turning off the thing
  that deploys.

What *is* portable is the application definition — see below.

## Portable applications

An application can be written once and rendered for whichever substrate it lands
on:

```bash
dabba application example > app.yaml
dabba application render app.yaml --substrate kind          # Kubernetes objects
dabba application render app.yaml --substrate docker-host   # a compose file
```

The shared schema is the **intersection** of what both runtimes genuinely
honour, not the union: image, tag, ports, environment (literal or from the secret
store), volumes, health check, resource limits. A field that cannot render
meaningfully on both sides is not in the schema, and a conformance test fails the
build if either renderer stops honouring one.

Genuinely substrate-specific configuration goes in explicit `kubernetes:` and
`dockerHost:` blocks. `dabba application portability <file> --substrate <s>`
reports which of them a target will ignore rather than dropping them silently.

Permanently out of scope, because no compose equivalent exists: autoscaling,
NetworkPolicy, PodDisruptionBudgets, multi-node scheduling, service mesh. Also
`replicas` and `dependsOn`, which look portable and are not — compose cannot
scale a service publishing a fixed host port, and Kubernetes has no ordering
primitive. These are rejected rather than ignored.

## Bring your own cluster

For any conformant cluster (k3s, k0s, RKE2, microk8s, a managed cloud you stood up yourself),
use `substrate: existing` and point dabba at its kubeconfig — it skips provisioning and just
configures the platform:

```yaml
environments:
  - name: my-cluster
    substrate: existing
    kubeconfig: ~/.kube/config
```

The cluster must be **Kubernetes 1.31 or newer** (the platform's External Secrets CRDs use
`selectableFields`, added in 1.31). `dabba up` checks this up front and stops with a clear
message if the cluster is too old — provisioned substrates always get a new-enough version.

## AWS Fargate EKS

`substrate: eks` provisions an EKS cluster that runs entirely on Fargate — no node groups to
manage — and installs the same platform on it. It needs the `aws` CLI on your PATH and working
AWS credentials (`dabba up` checks `aws sts get-caller-identity` up front); the kubeconfig it
writes authenticates with `aws eks get-token`.

```yaml
spec:
  domain: eks.example.com
  tls:
    issuer: acme
    acme:
      email: platform@example.com
  gateway:
    exposure: loadbalancer
  environments:
    - name: eks
      substrate: eks
      substrateConfig:
        region: us-east-1
        k8sVersion: "1.31"
        route53ZoneId: Z0123456789ABCDEFG # a Route53 hosted zone for `domain`
```

Real DNS and TLS come from a Route53 hosted zone: set `route53ZoneId` and use `tls.issuer: acme`
with `gateway.exposure: loadbalancer`. external-dns publishes the gateway hostnames into the zone
and cert-manager issues Let's Encrypt certificates via Route53 DNS-01. The `domain` may be the
zone itself or a subdomain of it (for example the `eks.example.com` domain inside an
`example.com` zone). Leave `route53ZoneId` empty to bring the cluster up with self-signed TLS on
the load-balancer hostname until a zone is available.

By default dabba provisions a dedicated VPC. To reuse an existing one, add `vpcId`,
`privateSubnetIds`, and `publicSubnetIds` to `substrateConfig`; the subnets must carry the EKS
load-balancer role tags (`kubernetes.io/role/elb` on public, `kubernetes.io/role/internal-elb`
on private).

## Shared settings

`tls.issuer` (selfsigned / acme), `gateway.exposure` (nodeport / loadbalancer), `git`,
`secrets.backend`, `observability`, and `useCases` are all set once at `spec` and inherited by
every environment.

:::tip
Secrets never live in the config. They're generated per-environment and stored in OpenBao —
retrieve them with `dabba secret get`.
:::
