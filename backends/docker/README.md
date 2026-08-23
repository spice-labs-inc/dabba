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

Today dabba (`src/`) provisions Kubernetes **substrates** (`kind`/`k3d`/`eks`/…)
and lets Flux reconcile. The DockerBackend is the parallel path for compose
hosts. The seam for the forthcoming increment:

- **`Backend` trait (next increment, not built here).** dabba's `up`/`down`/
  `status` will dispatch through a `Backend` trait. The existing Kubernetes flow
  becomes one implementation; a `DockerBackend` becomes another whose `up`
  clones the gitops repo, drops `reconcile.sh` in place, and calls this
  directory's `install.sh` to start the per-box loop — then `status` shells out
  to `docker compose ps` per stack. This directory is deliberately **pure shell +
  plists + docs** so it carries no Rust and does not touch dabba's Cargo build;
  the Rust trait wraps it later.
- **Config selection (later).** A `runtime: docker` selector in `dabba.yaml`
  (alongside today's `substrate:`) will choose the DockerBackend for an
  environment. Not wired yet.

### Deferred to subsequent increments

- **Secrets / OpenBao** — the compose-host equivalent of External Secrets
  (today the reconciler only reads a box-local `.env`).
- **The multitool compose artifact** — dabba's own stack (Postgres / OpenBao /
  MinIO / server) rendered as a compose bundle this backend can converge.
- **`dabba.yaml runtime: docker`** config selection and the `Backend` trait
  itself.
