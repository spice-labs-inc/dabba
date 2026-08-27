---
title: Pinned environment
description: One place that decides which versions a laptop, a CI runner and a cloud box all resolve to.
---

"The same thing everywhere" is only true if something decides what *the same
thing* is. `spec.environment` is that place.

```yaml
spec:
  environment:
    toolchain:
      rust: "1.83.0"
      rustTargets:
        - wasm32-wasip2
      tools:
        just: "1.36.0"
    services:
      postgres: "16"
      openbao: "2.1.0"
```

```bash
dabba environment show     # the pins, and how this machine compares
dabba environment check    # exits non-zero on drift
dabba environment export   # shell exports, for a workflow to eval
dabba environment verify   # artifacts whose tags disagree with a service pin
```

## Two kinds of version, kept apart on purpose

**`toolchain`** is what a human or a runner must have *installed* — the compiler,
its extra targets, the command-line tools a build shells out to. Nothing deploys
these. They are checked.

**`services`** are versions of things dabba actually *runs*. Those already have a
home: the `tag` of an Application, or the `image:` line of a hand-written compose
stack.

So they are not restated here. Restating them would make two lists maintained by
different hands, which is the single bug this codebase has produced most often —
a setting honoured in one place and silently absent in another. The pin is the
authority, and `dabba environment verify` reports anything that disagrees with
it:

```
≠ cache-minio.yaml: image minio is pinned to RELEASE.2024 in
  spec.environment.services but the definition uses RELEASE.2023
```

Drift is *caught* rather than prevented, which is the only option that does not
require a templating language in the application schema.

`verify` reads both rendered Application definitions and hand-written compose
stacks. The reconciler still accepts hand-written stacks — the OpenBao example is
one — so a check that only looked at rendered artifacts would miss exactly the
stack the pin was written for.

## Why this is not an Application field

A toolchain pin is consumed by things that are not containers at all: a
developer's shell, a GitHub runner. Folding it into the portable application
schema would repeat the union-schema mistake that the
[intersection rule](/configuration/#portable-applications) exists to prevent.

It is not a substrate setting either. It is the same everywhere by definition — a
pin that varies per environment is not a pin.

## Matching, and why a major version is enough

A pin of `16` matches an installed `16.4.1`. Pinning a major version and
accepting its patch releases is a normal thing to want, and treating it as drift
would make the check cry wolf until people stopped reading it.

It is a version-boundary match, not a string prefix: `16` does **not** match
`161`.

## In CI

`dabba environment verify` runs on every pull request. It needs no toolchain
installed, because it compares declared versions against declared versions.

`dabba environment check` is the half that inspects the machine, so it belongs in
a developer setup script or a runner image build rather than in a job that only
compiles.
