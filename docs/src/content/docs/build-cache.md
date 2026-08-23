---
title: Build cache
description: A shared sccache bucket that survives ephemeral runners, and the trust map that makes sharing it safe.
---

CI runs the same entrypoint on a laptop, a GitHub runner, or a remote dabba. For a
cache to serve all three it has to live in an object store — GitHub's own cache
only exists on GitHub, and a cache that starts empty on every ephemeral runner is
not a cache at all.

## Where it should run, and why that is not taste

A cache is only worth operating if it outlives the thing using it. Started on an
ephemeral runner it begins empty, every lookup misses, and you pay to run a cache
that never has anything in it.

Test dependencies want the opposite. A Postgres for the suite to talk to should
be fresh and isolated per run, and a shared one across concurrent runs is a
correctness problem rather than a caching one.

Same schema, opposite lifetimes — which is why they are two environments:

```yaml
environments:
  - name: ci-runner       # ephemeral: test dependencies, on the runner
    substrate: docker-host
  - name: shared-cache    # long-lived: the cache
    substrate: scaleway-kapsule
```

## The trust map

A shared cache anyone can write to is a supply-chain problem: an untrusted pull
request could poison an entry that a later trusted build reads and links into a
release. So there are two credentials, and which one a build receives *is* the
security boundary.

| Build | Credential | Why |
|-------|-----------|-----|
| Push to the trusted branch | **read-write** | Populates the shared cache |
| Push to any other branch | read-only | Reads; cannot poison |
| Pull request from this repository | read-only | Same |
| Pull request from a fork | **none** | See below |

The root credential stays in OpenBao and never reaches CI.

### Why forks get nothing

GitHub hands fork builds no secrets at all. The only ways to give a fork cache
access are to publish a credential or to give it nothing — and publishing even a
read-only key exposes every build artifact you have ever cached to the internet.

Forks compile cold. That is a deliberate cost, not an oversight.

## Setting it up

```bash
# 1. Deploy the cache to the long-lived environment. It is a portable
#    Application, so it renders for either substrate.
dabba application render examples/applications/cache-minio.yaml \
  --substrate scaleway-kapsule

# 2. Create the bucket and both scoped credentials. Idempotent.
dabba cache up

# 3. Read them out for whatever consumes them.
dabba cache credentials --scope read-write
dabba cache credentials --scope read-only
```

Then give GitHub Actions:

- a repository **variable** `SCCACHE_ENDPOINT` — the cache's URL
- repository **secrets** `SCCACHE_READWRITE_ACCESS_KEY`,
  `SCCACHE_READWRITE_SECRET_KEY`, `SCCACHE_READONLY_ACCESS_KEY`,
  `SCCACHE_READONLY_SECRET_KEY`

The endpoint is a variable rather than a secret on purpose: the workflow uses its
presence to decide whether to use a cache at all, and that check has to work in
fork builds, which cannot read secrets.

**With no `SCCACHE_ENDPOINT` set, the cache step is inert** and builds run exactly
as they did before. Nothing about the workflow requires a cache to exist.

## Rotating a credential

`dabba cache up` deliberately leaves existing credentials alone: reissuing on
every run would invalidate whatever CI currently holds, turning a routine re-run
into an outage. Rotation is a separate act — remove the credential's fields from
OpenBao and re-run `dabba cache up`, then update the repository secrets.
