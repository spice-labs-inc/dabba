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
| `substrate` | `kind` · `k3d` · `minikube` · `existing` (bring-your-own kubeconfig) · `scaleway-kapsule` / `eks` (roadmap). |
| `domain` | Optional per-env override of the shared `spec.domain`. |
| `kubeconfig` | Required when `substrate: existing` — path to the cluster's kubeconfig. |

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

## Shared settings

`tls.issuer` (selfsigned / acme), `gateway.exposure` (nodeport / loadbalancer), `git`,
`secrets.backend`, `observability`, and `useCases` are all set once at `spec` and inherited by
every environment.

:::tip
Secrets never live in the config. They're generated per-environment and stored in OpenBao —
retrieve them with `dabba secret get`.
:::
