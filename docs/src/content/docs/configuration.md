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
| `substrate` | `kind` · `k3d` · `minikube` · `eks` (AWS Fargate) · `existing` (bring-your-own kubeconfig). `scaleway-kapsule` is on the roadmap. |
| `domain` | Optional per-env override of the shared `spec.domain`. |
| `kubeconfig` | Required when `substrate: existing` — path to the cluster's kubeconfig. |
| `substrateConfig` | Per-substrate settings. For `eks`: `region`, `k8sVersion`, `route53ZoneId`, and optionally `vpcId` + `privateSubnetIds` + `publicSubnetIds` to reuse an existing VPC. |

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
