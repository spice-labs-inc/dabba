---
title: Quickstart
description: Bring up the full dabba platform on a local kind cluster.
---

A few minutes from clone to a working platform on a local [kind](https://kind.sigs.k8s.io/)
cluster — no cloud account, no secret manager, nothing to sign up for.

## Requirements

dabba drives **Docker**, **[OpenTofu](https://opentofu.org/)**, and **kubectl**, so they need to
be on your PATH — `dabba doctor` checks for them.

:::note
Local clusters and the platform components are demanding on inotify limits. If pods crash
with *"too many open files"*, raise them:

```bash
sudo sysctl fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=1048576
```
:::

## Install the CLI

```bash
curl -fsSL https://raw.githubusercontent.com/spice-labs-inc/dabba/main/install.sh | bash
```

On Windows, use `irm https://raw.githubusercontent.com/spice-labs-inc/dabba/main/install.ps1 | iex`.
To build from source instead: `cargo install --git https://github.com/spice-labs-inc/dabba`.

The installer also sets up shell completions. (`dabba completions <bash|zsh|fish|powershell>`
prints a completion script if you'd rather wire it up yourself.)

## Bring it up

```bash
git clone https://github.com/spice-labs-inc/dabba.git
cd dabba
dabba up -c examples/local.yaml
```

`dabba up` doesn't report success until the platform has reconciled. If a layer fails, it tells
you which one and why.

## What you get

- **`https://podinfo.localtest.me:31443`** — the demo app. Its banner was written into OpenBao
  and delivered to the app by External Secrets, so seeing it confirms the chain (gateway → TLS →
  External Secrets → OpenBao → gitops) works end to end.
- **`https://bao.localtest.me:31443`** — the OpenBao UI.

`*.localtest.me` resolves to `127.0.0.1`, so there's nothing to add to `/etc/hosts`. TLS uses a
self-signed CA, so your browser will warn — that's expected locally.

## Credentials

dabba ships **no** default credentials — everything is generated per-environment. Most secrets
live in OpenBao and are addressed by their path; retrieve them with the CLI:

```bash
dabba secret ls                  # list what's in OpenBao
dabba secret get dabba/forgejo   # the Forgejo admin login (vault path secret/dabba/forgejo)
dabba secret get demo/podinfo    # the demo app's banner secret
```

The one exception is the **OpenBao root token** — the key to the vault, so it can't live inside
it. dabba keeps it in a local per-env stash; fetch it with the keyword form:

```bash
dabba secret get local/openbao-root    # from .dabba/<env>/, not from OpenBao
```

## Look at it

```bash
dabba status        # health, declared-vs-live, component versions, endpoints
dabba diagram       # the live topology as an ASCII diagram (--mermaid to embed)
```

## Tear down

```bash
dabba down -c examples/local.yaml
```

The default config also ships `k3d` and `minikube` environments — `dabba ls` lists them, and
`dabba env k3d up` brings up a different one.
