# dabba DockerBackend — bare-OS gitops reconcile core

The **DockerBackend** brings dabba's declarative, git-driven model to hosts that
run **docker compose** instead of Kubernetes. It is the Flux equivalent for
compose boxes: a git repo holds desired state, and a per-box reconcile loop
converges the box to it every minute. This directory is the reconcile **core** —
portable shell plus the per-platform loop installers. It runs unchanged on
**macOS** (BSD userland + launchd) and **Linux** (GNU userland + systemd).

Anything that differs so that one script serves both a BSD and a GNU userland is
commented inline in `reconcile.sh` (search `PORTABILITY:`).

## What it does

Every minute the loop runs `reconcile.sh`, which:

1. `git fetch` + `git reset --hard origin/main` the gitops clone (self-updating —
   it runs from the clone it pulls).
2. For each app declared under **this box** at
   `<GITOPS_APPS_DIR>/<box>/<app>/`, resolves desired state (a full
   `docker-compose.yml`, or a shared `base/<app>` + this box's
   `docker-compose.override.yml` merged with `docker compose config`).
3. Compares the **rendered** desired file to what was last applied in
   `<STACKS_DIR>/<app>/`; if unchanged, does nothing.
4. On a change: pre-creates `./`-relative bind-mount sources (so dockerd does not
   create them root-owned), pins `COMPOSE_PROJECT_NAME=gitops-<app>` in the
   stack's `.env`, and runs `docker compose pull && up -d --force-recreate
   --remove-orphans`.
5. Runs an optional `x-health-cmd` gate (top-level compose key), retried up to
   two minutes.
6. Syncs the app's scheduled jobs (see [Scheduled jobs](#scheduled-jobs-cron)).

### Safety guarantees

- **Namespaced**: every managed stack is compose project `gitops-<app>`, so its
  containers/networks/volumes can never collide with — or adopt — anything that
  predates the reconciler on the box.
- **Never destroys**: it never runs `compose down`, never touches volumes, never
  deletes data directories. Removing a stack from git stops *managing* it but
  leaves it running; decommissioning is always a deliberate manual act.
- **`--remove-orphans` is project-scoped**: it only removes a container this
  same stack (`gitops-<app>`) no longer declares — never another stack or a
  pre-existing or legacy workload, which is a different compose project.
- **Transient fetch failures never page**: a failed `git fetch` warns and exits
  0; the next tick retries.

## Layout

```
backends/docker/
  reconcile.sh                         the portable reconciler (runs from the clone; self-updates)
  reconcile-alert.sh                   optional failure alert (Slack webhook / journal / stderr)
  install.sh                           detects the platform, installs the reconcile loop
  uninstall.sh                         reverses install.sh
  launchd/
    io.spicelabs.dabba.reconcile.plist.template   macOS reconcile-loop LaunchAgent (rendered by install.sh)
  systemd/
    gitops-reconcile.service.template   Linux reconcile-loop service   (rendered by install.sh)
    gitops-reconcile.timer              Linux minutely trigger
    gitops-reconcile-alert.service.template   OnFailure alert unit
  examples/
    io.spicelabs.dabba.cron.example-app.nightly-backup.plist
                                        macOS scheduled-job LaunchAgent (convention example;
                                        the filename IS the Label)
    example-cron.service / .timer       Linux scheduled-job systemd user units (convention example)
  test/
    run-test.sh                         on-host convergence smoke test (portable, side-effect-free)
    run-cron-macos-test.sh              macOS launchd cron-sync verification (self-cleaning)
```

Desired state lives in a **separate gitops repo** (not here), laid out as:

```
<GITOPS_APPS_DIR>/
  base/<app>/docker-compose.yml            optional shared base
  <box>/<app>/docker-compose.yml           full stack (legacy path), OR
  <box>/<app>/docker-compose.override.yml  per-box patch on base/<app>
  <box>/<app>/launchd/*.plist              macOS scheduled jobs   (see below)
  <box>/<app>/systemd/*.{service,timer}    Linux scheduled jobs   (see below)
```

`GITOPS_APPS_DIR` defaults to `apps`, and is an environment variable rather than
a fixed provider-specific subtree because dabba is provider-neutral. Placement is
declared in git: a box only applies stacks under its own hostname.

## Configuration

All are environment variables, read by `reconcile.sh` and by `install.sh`:

| Var              | Default             | Meaning                                   |
|------------------|---------------------|-------------------------------------------|
| `GITOPS_DIR`     | `~/dabba-gitops`    | the gitops clone to converge from         |
| `STACKS_DIR`     | `~/stacks`          | where stacks are applied (reconciler-owned) |
| `BOX_NAME`       | `hostname -s`       | this box's directory name in the gitops repo |
| `GITOPS_APPS_DIR`| `apps`              | apps subtree under `GITOPS_DIR`           |
| `SLACK_WEBHOOK_URL` | *(unset)*        | optional; used by `reconcile-alert.sh`    |

## Installing the reconcile loop

Clone the gitops repo to `GITOPS_DIR` first, then:

```bash
GITOPS_DIR=~/dabba-gitops STACKS_DIR=~/stacks BOX_NAME=mybox \
  ./install.sh          # detects macOS vs Linux by `uname`
./uninstall.sh          # reverses it
```

### macOS vs Linux: how the split works

`install.sh` and `reconcile.sh` both branch on `uname -s`:

|                     | macOS (`Darwin`)                                   | Linux                                             |
|---------------------|----------------------------------------------------|---------------------------------------------------|
| **Reconcile loop**  | launchd **LaunchAgent** `io.spicelabs.dabba.reconcile` with `StartInterval` 60 + `RunAtLoad` | systemd **user** units `gitops-reconcile.service` + `.timer` (every 1 min) |
| **Install**         | `launchctl bootstrap gui/$(id -u)` + `enable` + `kickstart` (legacy form `launchctl load -w` is noted in the plist template) | `systemctl --user enable --now gitops-reconcile.timer`; `loginctl enable-linger` so it runs without a login session |
| **Logs**            | file at `~/Library/Logs/dabba/reconcile.log` (launchd has no journal) | `journalctl --user -u gitops-reconcile.service` |
| **Failure alert**   | none native — stdout/stderr captured to the log; `reconcile-alert.sh` can run by hand | `OnFailure=gitops-reconcile-alert.service` runs `reconcile-alert.sh` |

The systemd `.service`/alert units and the launchd plist are **templates**;
`install.sh` renders `__TOKENS__` (`__RECONCILE_SH__`, `__GITOPS_DIR__`,
`__STACKS_DIR__`, `__BOX_NAME__`, `__PATH__`, `__LOG__`, `__BACKEND_DIR__`) into
the real unit so the same file works on any user/box. The captured `__PATH__`
matters on macOS because launchd hands jobs a spartan PATH that otherwise cannot
find `docker`/`git`.

## Scheduled jobs (cron)

Compose has no scheduler, so batch jobs ship as per-platform scheduled units
next to the stack, and `reconcile.sh` syncs them — manifest-tracked and fully
reconciler-owned (schedules are *code*, unlike volumes/data): units dropped from
git are disabled/booted-out and removed.

Convention on both platforms: the job is a **one-shot container of the stack**,
run behind the `jobs` compose profile so the reconciler's `up -d` never starts
it as a long-running service:

```
docker compose --profile jobs run --rm <service>
```

- **Linux** — `<box>/<app>/systemd/*.{service,timer}` (systemd **user** units).
  Synced into `~/.config/systemd/user/`, reloaded, timers enabled; installed
  verbatim (systemd expands `%h`). Tracked in
  `~/.config/systemd/user/.gitops-<app>.units`. See `examples/example-cron.*`.
- **macOS** — `<box>/<app>/launchd/*.plist` (LaunchAgents, `StartCalendarInterval`).
  The shipped plists are **templates**: the reconciler substitutes `__DOCKER__`,
  `__STACK_DIR__`, `__APP__` (the launchd analogue of systemd's `%h`), installs
  them into `~/Library/LaunchAgents/`, and bootstraps them into the per-user GUI
  domain. Tracked in `~/Library/LaunchAgents/.gitops-<app>.agents`. The plist
  `Label` must equal the filename without `.plist`; the reconciler refuses a plist
where they disagree, because it boots agents in and out by that label. See
`examples/io.spicelabs.dabba.cron.example-app.nightly-backup.plist`.

## Testing on this host

```bash
./test/run-test.sh              # portable; builds a throwaway gitops repo, runs a
                                # real nginx stack, asserts up / no-op / redeploy,
                                # tears the container down on exit
./test/run-cron-macos-test.sh   # macOS only; verifies launchd cron install/remove,
                                # self-cleaning
```

`run-test.sh` was run on macOS (OrbStack docker, bash 3.2) during development:
first reconcile brought the container up as `gitops-dabbatest` and passed the
`x-health-cmd` gate, a second reconcile was a clean no-op (identical container
id), and changing the compose triggered exactly one redeploy (new id, new label).

## How this slots into dabba

`src/backend/` dispatches dabba's per-environment verbs through a `Backend`
trait. The Kubernetes flow (tofu provisions a substrate, Flux reconciles the
platform) is one implementation; `DockerBackend` is the other, and it drives this
directory.

An environment selects it with `substrate: docker-host`. That is a value of the
existing `substrate` field rather than a separate `runtime:` selector — an
earlier sketch proposed the latter, and folding it in makes contradictory states
such as `runtime: docker` with `substrate: eks` unrepresentable.

These scripts are also **compiled into the dabba binary** and written into the
environment's working directory on demand. dabba ships as a single binary from a
GitHub Release, so an installed dabba has no `backends/docker/` on disk;
resolving it relative to the working directory only ever worked from a git
checkout. Set `substrateConfig.backendDir` to a working copy when developing the
reconciler itself.

## Portability: what carries across substrates, and what does not

An application can be written once, portably, and rendered for either substrate:

```bash
dabba application render app.yaml --substrate docker-host   # a compose file
dabba application render app.yaml --substrate kind          # Kubernetes objects
```

The shared schema is the **intersection** of what both runtimes genuinely
honour, not the union — a field that cannot render meaningfully on both sides is
not in the schema, and a conformance test fails the build if either renderer
stops honouring one. `dabba application example` prints a definition exercising
every portable field.

What is **not** portable, and is reported by name rather than silently dropped:

- `kubernetes:` and `dockerHost:` escape blocks, for genuinely substrate-specific
  configuration. `dabba application portability <file> --substrate <s>` says
  which of them the target will ignore.
- Hand-written compose files. They remain fully supported alongside rendered
  ones — the reconciler's three input paths (verbatim, base+override, rendered)
  all converge the same way — but a hand-written stack is by definition
  compose-only.
- Everything in `spec.tls`, `spec.gateway`, `spec.observability` and
  `spec.useCases`. Those drive the Kubernetes reconcile layer and a docker-host
  environment does not consume them.

## Egress policy

multitool's runner screens resolved addresses before connecting — refusing
loopback, private, link-local, unique-local and carrier-grade NAT ranges, with
the screening resolver *being* the HTTP client's resolver so DNS rebinding has
nowhere to stand. That closed a real credential-theft path through the cloud
metadata endpoint.

Application-level screening and platform-level egress policy are complementary,
not alternatives. An application cannot be trusted to police itself once it is
compromised, and the platform cannot know an application's intent. The original
five-box design had Envoy with per-app allowlists for the platform half; Envoy on
a single compose host is a great deal of machinery for the job.

**Decided for this backend:**

1. **A stack that needs no egress gets none.** Declare its network `internal:
   true` through the `dockerHost:` escape hatch. Docker enforces this at the
   network layer, it costs nothing, and it is the strongest available control.
   Prefer it wherever it applies.
2. **Block the cloud metadata endpoint at the host.** `169.254.169.254` is the
   single highest-value target reachable from a container on a cloud box: it
   hands out instance credentials to anything that asks. This is a host-level
   prerequisite, not something the reconciler can do for you, and it is called
   out here because the reconciler will happily run stacks on a box where it has
   not been done.
3. **Per-app allowlists are deliberately NOT built.** The shape, if this is
   needed later, is an egress proxy container on the stack's network with the
   application's environment pointed at it — an approximation of Envoy's
   allowlists without Envoy's weight. Recorded as an option, not a plan, so its
   absence reads as a decision.

### Trust boundary

**The gitops repository is a trusted input.** `x-health-cmd` is arbitrary shell
executed on the box every tick, and a stack's compose file can mount any host
path. Anyone who can commit to a box's subtree of the gitops repo can run code as
the reconciling user on that box. That is a deliberate design — it is the same
trust Flux places in its source repository — but it means the gitops repo needs
the same branch protection and review that the boxes themselves warrant.

Secrets are the exception that proves it: rendered compose files carry secret
*references*, never values, precisely so the repository can be trusted with the
former and never holds the latter.

## Testing on this host

| Script | Needs Docker | What it proves |
| --- | --- | --- |
| `test/run-test.sh` | yes | deploy, clean no-op, redeploy on change |
| `test/run-convergence-test.sh` | partly | base+override rendering, health re-checking, fetch failure handling, render diagnostics, a non-default apps dir |
| `test/run-cron-macos-test.sh` | macOS | per-app scheduled-job agents sync and unsync |
| `test/run-end-to-end-test.sh` | yes | the whole arc: up, OpenBao initialised and unsealed, a secret resolved into a running application, down leaving stacks up |

The first two run in CI on every pull request. Each one removes every container,
scheduler unit and directory it creates, and then verifies the removal rather
than assuming it.

`run-end-to-end-test.sh` is deliberately NOT a CI gate. It installs a real
scheduler unit, which needs a live user session — a launchd GUI domain on macOS,
or systemd `--user` with lingering enabled on Linux. Hosted runners generally
have neither, so running it there would test the runner rather than the
backend. Run it locally before merging anything that touches `up`, `down`, the
reconciler, or the secrets path.
