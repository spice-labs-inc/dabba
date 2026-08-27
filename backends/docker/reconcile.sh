#!/usr/bin/env bash
#
# dabba DockerBackend — portable per-box gitops reconciler for bare-OS docker
# compose hosts. The Flux equivalent for machines that run docker compose
# instead of k8s. Runs on BOTH macOS (BSD userland + launchd) and Linux (GNU
# userland + systemd --user).
#
# Anything that differs so that one script serves both a BSD and a GNU userland
# is called out inline with "PORTABILITY:".
#
# Every minute (launchd StartInterval / systemd .timer): pull this gitops repo,
# then converge every stack filed under <GITOPS_APPS_DIR>/<this box>/<app>/
# docker-compose.yml into <STACKS_DIR>/<app>/. Placement is declared in git: a
# box only applies stacks under its own hostname. Adding an app to a box =
# adding a directory here.
#
# Runs from the clone it pulls, so it self-updates; a bad commit to this file
# stalls convergence until reverted (same self-management trade Flux makes).
#
# Optional per-stack health gate: a top-level `x-health-cmd: <shell command>`
# key in the compose file, retried for up to 2 minutes after deploy.
#
# Exit codes: 0 = in sync / converged / a transient fetch failure inside the
# retry budget (warn only); 1 = one or more stacks failed to deploy, is still
# failing its health gate, the tracked branch does not exist, or fetch has
# failed too many consecutive times. A non-zero exit is what raises an alert
# (systemd OnFailure= on Linux, reconcile-with-alerting.sh on macOS).
#
# Portable POSIX-ish bash: targets macOS's default /bin/bash 3.2 as well as
# Linux bash — no bash 4+ features (no associative arrays, no ${x^^}).
set -uo pipefail

GITOPS_DIR="${GITOPS_DIR:-$HOME/dabba-gitops}"
STACKS_DIR="${STACKS_DIR:-$HOME/stacks}"
BOX="${BOX_NAME:-$(hostname -s)}"
# PORTABILITY: the apps root is an env var (default "apps") rather than a fixed
# provider-specific subtree, because dabba is provider-neutral. The per-box
# "<box>/<app>/" selection and the base+override layout below are unaffected.
GITOPS_APPS_DIR="${GITOPS_APPS_DIR:-apps}"
# The branch this box tracks. Was hard-coded to "main"; a repo whose default
# branch is named anything else could never converge and never said so.
GITOPS_BRANCH="${GITOPS_BRANCH:-main}"
# Where to reach OpenBao, and the file holding its root token. dabba writes the
# token file at 0600 in the environment's working directory; it is deliberately
# NOT in git and NOT in this repo. Unset means secret resolution is disabled and
# a stack that needs a secret fails loudly rather than deploying half-configured.
OPENBAO_PROJECT="${OPENBAO_PROJECT:-gitops-openbao}"
OPENBAO_TOKEN_FILE="${OPENBAO_TOKEN_FILE:-}"
# This environment's scheduled-job failure alert unit, as the __JOB_ALERT__ token
# a job in gitops content writes OnFailure= against. Per environment, so tearing
# one down neither removes alerting for the others nor orphans a unit.
JOB_ALERT_UNIT="gitops-reconcile-${DABBA_ENVIRONMENT:-default}-job-alert@"
BOX_DIR="$GITOPS_DIR/$GITOPS_APPS_DIR/$BOX"

# Consecutive-fetch-failure counter, kept in the reconciler-owned stacks dir.
# Bounded retry: a blip is silent, a sustained outage eventually alerts.
FETCH_FAILURE_COUNTER="$STACKS_DIR/.gitops-fetch-failures"
FETCH_FAILURE_LIMIT=5

# PORTABILITY: detect the host so the scheduled-jobs sync (bottom of the file)
# can pick systemd --user on Linux vs launchd LaunchAgents on macOS.
PLATFORM="$(uname -s)"   # "Darwin" = macOS, "Linux" = systemd hosts

# This script's own directory — where dabba materialised the reconciler, and so
# where backup.sh and restore.sh sit. Resolved BEFORE the cd below, because $0 is
# relative when the scheduler invokes it that way. Scheduled job units get it as
# the __BACKEND_DIR__ token, which is what lets a unit in git call the shipped
# backup without hard-coding a path that contains the environment name.
BACKEND_DIR="$(cd "$(dirname "$0")" && pwd)"

cd "$GITOPS_DIR" 2>/dev/null || true   # cwd-independence: tools below resolve paths absolutely

if [ ! -d "$GITOPS_DIR/.git" ]; then
    echo "ERROR: $GITOPS_DIR is not a git clone; see backends/docker/README.md" >&2
    exit 1
fi

mkdir -p "$STACKS_DIR"

# Transient network failures must not page anyone; next tick retries. But a
# PERMANENT failure must not look like a transient one. The original printed the
# same warning and exited 0 for both, so a gitops repo whose branch was misnamed
# — or whose credentials were wrong — sat there looking healthy, converging
# nothing, forever. Three outcomes now:
#   remote reachable, branch missing -> terminal, alert on this very tick
#   remote unreachable               -> transient, warn and count
#   too many consecutive failures    -> the box is not converging, alert
if ! git -C "$GITOPS_DIR" fetch -q origin "$GITOPS_BRANCH" 2>/dev/null; then
    if git -C "$GITOPS_DIR" ls-remote origin > /dev/null 2>&1 \
       && ! git -C "$GITOPS_DIR" ls-remote --exit-code --heads origin "$GITOPS_BRANCH" > /dev/null 2>&1; then
        echo "ERROR: branch '$GITOPS_BRANCH' does not exist on origin." >&2
        echo "       This box will never converge until it does. Push the branch," >&2
        echo "       or set GITOPS_BRANCH to the one the gitops repo actually uses." >&2
        exit 1
    fi
    failures=$(( $(cat "$FETCH_FAILURE_COUNTER" 2>/dev/null || echo 0) + 1 ))
    printf '%s' "$failures" > "$FETCH_FAILURE_COUNTER"
    if [ "$failures" -ge "$FETCH_FAILURE_LIMIT" ]; then
        echo "ERROR: git fetch has failed $failures consecutive times; this box is" >&2
        echo "       not converging and its stacks are drifting from git." >&2
        exit 1
    fi
    echo "WARN: git fetch failed ($failures/$FETCH_FAILURE_LIMIT); will retry next tick"
    exit 0
fi
rm -f "$FETCH_FAILURE_COUNTER"
git -C "$GITOPS_DIR" reset -q --hard "origin/$GITOPS_BRANCH"

if [ ! -d "$BOX_DIR" ]; then
    echo "no stacks declared for box $BOX"
    exit 0
fi

# ---- scheduled jobs (gitops crons): Linux systemd user units ---------------
# A stack may ship <app>/systemd/*.{service,timer} — systemd USER units that
# run its one-shot jobs. They are synced into ~/.config/systemd/user, timers
# enabled; units dropped from git are disabled and removed (tracked in a per-app
# manifest, so units are fully reconciler-owned — schedules are code, unlike
# volumes/data). Requires `loginctl enable-linger <user>` once per box so user
# units run without a login session.
#
# Like the launchd plists, these are TEMPLATES: the same __STACK_DIR__,
# __DOCKER__, __APP__ and __BACKEND_DIR__ tokens are substituted before install.
# They used to be copied verbatim, on the reasoning that systemd expands %h
# itself — but %h cannot name the directory dabba materialises its scripts into,
# so a unit wanting to run the shipped backup.sh had to hard-code an absolute
# path containing the environment name. One machine-independent file in git now
# renders to a concrete unit on any box, exactly as it already did on macOS.
sync_systemd_units() {
    local app="$1" src="$2"
    local unit_dir="$HOME/.config/systemd/user"
    local manifest="$unit_dir/.gitops-$app.units"
    [ -d "$src" ] || [ -f "$manifest" ] || return 0
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    mkdir -p "$unit_dir"
    local new="" changed="" stack_dir docker_bin
    stack_dir="$STACKS_DIR/$app"
    docker_bin="$(command -v docker || echo /usr/bin/docker)"
    if [ -d "$src" ]; then
        for u in "$src"/*.service "$src"/*.timer; do
            [ -f "$u" ] || continue
            local name rendered
            name="$(basename "$u")"
            new="$new$name"$'\n'
            # Compare the RENDERED unit against the installed one, so a
            # token-only change (STACKS_DIR moved, dabba reinstalled elsewhere)
            # is still detected as a change.
            rendered="$(mktemp "${TMPDIR:-/tmp}/dabba-unit.XXXXXX")"
            sed -e "s|__STACK_DIR__|$stack_dir|g" \
                -e "s|__DOCKER__|$docker_bin|g" \
                -e "s|__BACKEND_DIR__|$BACKEND_DIR|g" \
                -e "s|__JOB_ALERT__|$JOB_ALERT_UNIT|g" \
                -e "s|__STACKS_DIR__|$STACKS_DIR|g" \
                -e "s|__BACKUPS_DIR__|${BACKUPS_DIR:-$HOME/backups}|g" \
                -e "s|__APP__|$app|g" "$u" > "$rendered"
            if ! cmp -s "$rendered" "$unit_dir/$name"; then
                cp "$rendered" "$unit_dir/$name"
                changed=1
            fi
            rm -f "$rendered"
        done
    fi
    if [ -f "$manifest" ]; then
        while IFS= read -r old; do
            [ -n "$old" ] || continue
            if ! printf '%s' "$new" | grep -qxF "$old"; then
                systemctl --user disable --now "$old" > /dev/null 2>&1
                rm -f "$unit_dir/$old"
                changed=1
                echo "$app: removed cron unit $old"
            fi
        done < "$manifest"
    fi
    printf '%s' "$new" > "$manifest"
    if [ -n "$changed" ]; then
        systemctl --user daemon-reload
        echo "$app: cron units synced"
    fi
    printf '%s' "$new" | grep '\.timer$' | while IFS= read -r t; do
        systemctl --user enable --now "$t" > /dev/null 2>&1 \
            || echo "WARN: $app: enabling $t failed (is linger enabled for $USER?)"
    done
}

# ---- scheduled jobs (gitops crons): macOS launchd LaunchAgents -------------
# PORTABILITY: the macOS analogue of sync_systemd_units. A stack ships
# <app>/launchd/*.plist — LaunchAgents that run its one-shot jobs (same
# convention: `docker compose --profile jobs run --rm <service>`,
# WorkingDirectory=<STACKS_DIR>/<app>, scheduled with StartCalendarInterval).
# The shipped plists are TEMPLATES: the reconciler substitutes __STACK_DIR__,
# __DOCKER__ and __APP__ before installing, mirroring how systemd units use
# %h — so the same machine-independent file in git renders to a concrete
# LaunchAgent on any box. Agents are synced into ~/Library/LaunchAgents,
# bootstrapped into the per-user GUI domain, and agents dropped from git are
# booted out and removed (tracked in a per-app manifest, exactly like the
# systemd path — schedules are code, fully reconciler-owned).
sync_launchd_agents() {
    local app="$1" src="$2"
    local agent_dir="$HOME/Library/LaunchAgents"
    local manifest="$agent_dir/.gitops-$app.agents"
    [ -d "$src" ] || [ -f "$manifest" ] || return 0
    mkdir -p "$agent_dir"
    local uid stack_dir docker_bin new=""
    uid="$(id -u)"
    stack_dir="$STACKS_DIR/$app"
    docker_bin="$(command -v docker || echo /usr/local/bin/docker)"
    if [ -d "$src" ]; then
        for p in "$src"/*.plist; do
            [ -f "$p" ] || continue
            local name label rendered declared
            name="$(basename "$p")"
            label="${name%.plist}"                 # convention: Label == filename sans .plist
            # Render the template, then compare the RENDERED plist against the
            # installed one so a token-only change (e.g. STACKS_DIR moved) is
            # still detected.
            rendered="$(mktemp "${TMPDIR:-/tmp}/dabba-agent.XXXXXX")"
            sed -e "s|__STACK_DIR__|$stack_dir|g" \
                -e "s|__DOCKER__|$docker_bin|g" \
                -e "s|__BACKEND_DIR__|$BACKEND_DIR|g" \
                -e "s|__JOB_ALERT__|$JOB_ALERT_UNIT|g" \
                -e "s|__STACKS_DIR__|$STACKS_DIR|g" \
                -e "s|__BACKUPS_DIR__|${BACKUPS_DIR:-$HOME/backups}|g" \
                -e "s|__APP__|$app|g" "$p" > "$rendered"
            # The reconciler boots agents in and out by "gui/<uid>/<label>", so a
            # plist whose Label disagrees with its filename installs once and can
            # then never be updated or removed — it would outlive its own removal
            # from git. Refuse it instead of installing something unmanageable.
            declared="$(sed -n '/<key>Label<\/key>/{n;s|.*<string>\(.*\)</string>.*|\1|p;}' "$rendered")"
            if [ "$declared" != "$label" ]; then
                echo "ERROR: $app: $name declares Label '$declared'; the reconciler" >&2
                echo "       manages agents by filename, so rename the file to" >&2
                echo "       '$declared.plist' or change the Label to '$label'." >&2
                rm -f "$rendered"
                continue
            fi
            new="$new$name"$'\n'
            if ! cmp -s "$rendered" "$agent_dir/$name"; then
                # bootout the old agent (ignore if it was never loaded), install
                # the freshly rendered plist, then bootstrap it back in.
                launchctl bootout "gui/$uid/$label" > /dev/null 2>&1 || true
                cp "$rendered" "$agent_dir/$name"
                if launchctl bootstrap "gui/$uid" "$agent_dir/$name" > /dev/null 2>&1; then
                    echo "$app: launchd agent $name synced"
                else
                    echo "WARN: $app: bootstrapping $name failed"
                fi
            fi
            rm -f "$rendered"
        done
    fi
    if [ -f "$manifest" ]; then
        while IFS= read -r old; do
            [ -n "$old" ] || continue
            if ! printf '%s' "$new" | grep -qxF "$old"; then
                launchctl bootout "gui/$uid/${old%.plist}" > /dev/null 2>&1 || true
                rm -f "$agent_dir/$old"
                echo "$app: removed launchd agent $old"
            fi
        done < "$manifest"
    fi
    printf '%s' "$new" > "$manifest"
}

# Platform-aware dispatcher: same manifest-tracked, reconciler-owned contract
# on both hosts, different scheduler underneath.
sync_cron() {
    local app="$1" appdir="$2"
    if [ "$PLATFORM" = "Darwin" ]; then
        sync_launchd_agents "$app" "${appdir}launchd"
    else
        sync_systemd_units "$app" "${appdir}systemd"
    fi
}

# ---- desired-state resolution: full compose file OR base + override --------
# Kustomize-style layout: <GITOPS_APPS_DIR>/base/<app>/docker-compose.yml is the
# shared base; a box places the app by holding
# <box>/<app>/docker-compose.override.yml (its per-box patch, may be just
# `services: {}`). The two are rendered into one file with docker compose's
# native merge; change detection compares the RENDERED output, so a base edit
# redeploys every box that uses the app and an override edit redeploys one.
# --no-interpolate keeps ${VARS} for the box-local .env to resolve at `up`;
# --no-path-resolution keeps ./-relative bind mounts relative.
# Legacy: a full <box>/<app>/docker-compose.yml still wins unchanged (release
# workflows write image pins into those paths; migrate per-app deliberately).
BASE_DIR="$GITOPS_DIR/$GITOPS_APPS_DIR/base"
render_desired() {
    local app="$1" dir="$2" out="$3"
    local legacy="$dir/docker-compose.yml"
    local override="$dir/docker-compose.override.yml"
    local base="$BASE_DIR/$app/docker-compose.yml"
    if [ -f "$legacy" ]; then
        cp "$legacy" "$out"
        return 0
    fi
    if [ -f "$override" ]; then
        if [ ! -f "$base" ]; then
            echo "ERROR: $app: override present but no base at $base" >&2
            return 1
        fi
        # Capture compose's stderr instead of discarding it: a malformed base or
        # override used to fail with no indication of what was wrong with it.
        local errors
        errors="$(mktemp "${TMPDIR:-/tmp}/dabba-render-error.XXXXXX")"
        if docker compose -f "$base" -f "$override" config \
               --no-interpolate --no-path-resolution > "$out" 2> "$errors"; then
            rm -f "$errors"
            return 0
        fi
        echo "ERROR: $app: rendering base+override failed:" >&2
        sed 's/^/  /' "$errors" >&2
        rm -f "$errors"
        return 1
    fi
    return 2   # nothing declared for this app dir
}

# PORTABILITY: extract ./-relative bind-mount SOURCE paths from a rendered
# compose file, portably (BSD + GNU). Replaces the original's GNU-only
#   grep -oP '^\s*-\s*\K\./[^:]+'
# which BSD grep cannot run (no -P / \K). Two things made a straight awk port
# necessary rather than cosmetic:
#   * `docker compose config` NORMALIZES volumes to long form
#     (`source: ./data`), so the base+override render path never contains the
#     short `- ./src:/dst` form the original grep matched — it only ever worked
#     on the legacy verbatim path. We cover BOTH forms.
#   * We require a ':' in the short form so a non-mount list entry such as an
#     `env_file: [ ./app.env ]` item is NOT mistaken for a bind mount and
#     pre-created as a directory (the original grep would have matched it).
# \047 / \042 are octal for ' and " — avoids shell-quoting the awk program.
extract_bind_sources() {
    awk '
        # short form:  - ./src:/dst[:opts]   (legacy verbatim compose files)
        /^[[:space:]]*-[[:space:]]*\.\// {
            line = $0
            sub(/^[[:space:]]*-[[:space:]]*/, "", line)   # strip the "- "
            if (index(line, ":") > 0) {                   # a real host:container mount
                sub(/:.*$/, "", line)                     # keep only the host source
                print line
            }
            next
        }
        # long form:  source: ./src         (what `docker compose config` emits)
        /^[[:space:]]*source:[[:space:]]*\.\// {
            line = $0
            sub(/^[[:space:]]*source:[[:space:]]*/, "", line)
            gsub(/\047/, "", line); gsub(/\042/, "", line)   # strip any quotes
            sub(/[[:space:]]*$/, "", line)                    # trim trailing ws
            print line
        }
    ' "$1" | sort -u
}

# ---- secret resolution -----------------------------------------------------
# A rendered stack carries secret REFERENCES, never values:
#
#   environment:
#     SECRET_SETTING: ${SECRET_SETTING}
#   x-secrets:
#     SECRET_SETTING: demo#message
#
# The compose file lives in git, so it must never hold the value. This resolves
# each reference from OpenBao and writes it into the stack's own .env, which
# compose interpolates at `up` — the compose-host equivalent of External Secrets
# populating a Secret on Kubernetes. Same declaration in the Application, two
# mechanisms, same meaning.
#
# The .env is written 0600 and rewritten every tick, so a rotated secret reaches
# the box on the next minute without anyone redeploying.

# Read the x-secrets block of a rendered compose file as "VAR PATH KEY" lines.
# Portable awk: the block is a flat map of VAR: path#key under a top-level key.
extract_secret_references() {
    awk '
        /^x-secrets:/ { inside = 1; next }
        # Any other top-level key ends the block.
        /^[^[:space:]]/ { inside = 0 }
        inside && /^[[:space:]]+[A-Za-z_][A-Za-z0-9_]*:/ {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            split(line, parts, ":")
            variable = parts[1]
            reference = substr(line, index(line, ":") + 1)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", reference)
            gsub(/\047|\042/, "", reference)
            hash = index(reference, "#")
            if (hash > 0) {
                printf "%s %s %s\n", variable, substr(reference, 1, hash - 1), substr(reference, hash + 1)
            }
        }
    ' "$1"
}

# Read one key out of OpenBao. Token travels on stdin, never in argv, because
# argv is visible to every process on the box via ps.
openbao_read() {
    local path="$1" key="$2"
    docker exec -i "$(openbao_container)" sh -c \
        'read -r BAO_TOKEN; export BAO_TOKEN BAO_ADDR=http://127.0.0.1:8200; \
         bao kv get -field="$1" "secret/$2" 2>/dev/null' _ "$key" "$path" \
        < "$OPENBAO_TOKEN_FILE"
}

openbao_container() {
    docker ps -q --filter "label=com.docker.compose.project=$OPENBAO_PROJECT" \
                --filter "status=running" | head -1
}

# Resolve every reference in $2 into $1/.env. Returns non-zero if any reference
# could not be resolved — a stack deployed with a missing secret is a stack that
# will fail in a way nobody connects back to this.
sync_secrets() {
    local applied_dir="$1" compose_file="$2" app="$3"
    local references
    references="$(extract_secret_references "$compose_file")"
    [ -n "$references" ] || return 0

    if [ -z "$OPENBAO_TOKEN_FILE" ] || [ ! -f "$OPENBAO_TOKEN_FILE" ]; then
        echo "ERROR: $app: needs secrets but no OpenBao token is available" >&2
        echo "       (expected OPENBAO_TOKEN_FILE; run \`dabba env <name> up\`)" >&2
        return 1
    fi
    if [ -z "$(openbao_container)" ]; then
        echo "ERROR: $app: needs secrets but the $OPENBAO_PROJECT stack is not running" >&2
        return 1
    fi

    # Rebuild the secret half of .env from scratch each tick, preserving the
    # non-secret half (COMPOSE_PROJECT_NAME and anything an operator added).
    #
    # The marker line is the boundary: everything above it is operator-owned and
    # kept verbatim, everything below it dabba wrote last tick and is rebuilt.
    # Selecting by variable NAME instead got this wrong in both directions: an
    # unanchored "PASSWORD=" also matched an operator's DB_PASSWORD= line and
    # deleted it, and a reference dropped from the compose file matched nothing,
    # so its resolved value stayed in .env indefinitely.
    local env_file="$applied_dir/.env" kept failed=""
    kept="$(sed '/^# dabba-secret/,$d' "$env_file" 2>/dev/null || true)"

    local resolved="" variable path key value
    while read -r variable path key; do
        [ -n "$variable" ] || continue
        value="$(openbao_read "$path" "$key")"
        if [ -z "$value" ]; then
            echo "ERROR: $app: secret $path#$key is empty or unreadable" >&2
            failed=1
            continue
        fi
        resolved="$resolved$variable=$value"$'\n'
    done <<EOF
$references
EOF

    [ -n "$failed" ] && return 1

    # Restored below: this runs mid-tick, and leaving 077 set would silently
    # change the mode of every file the rest of the reconcile pass creates.
    local prior_umask
    prior_umask="$(umask)"
    umask 077
    {
        [ -n "$kept" ] && printf '%s\n' "$kept"
        echo "# dabba-secret: resolved from OpenBao; do not edit or commit"
        printf '%s' "$resolved"
    } > "$env_file"
    umask "$prior_umask"
    chmod 600 "$env_file"
    echo "$app: resolved $(printf '%s\n' "$references" | grep -c .) secret(s) from OpenBao"
    return 0
}

# ---- health gate -----------------------------------------------------------
# The optional per-stack `x-health-cmd`, as a re-runnable function. `attempts`
# is how many 5s tries to give it: 24 (two minutes) right after a deploy, 1 on a
# routine re-check of a stack already known to be unhealthy.
#
# head -1 because two x-health-cmd lines would otherwise concatenate into one
# multi-line command whose second half ran unconditionally.
health_command() {
    sed -n 's/^x-health-cmd: //p' "$1" | head -1
}

# A YAML block scalar (`x-health-cmd: >-`) puts the command on the FOLLOWING
# lines, so the extraction above returns the indicator itself. `bash -c '>-'` is
# a redirect into a file named '-', which succeeds — turning the health gate into
# something that passes unconditionally while looking like it ran. A
# crash-looping container was reported "converged and healthy" this way.
#
# Rather than teach the extractor to fold block scalars, refuse them: the gate
# must never be able to pass without having actually run a command.
health_command_is_usable() {
    case "$1" in
        ''|'>'|'>-'|'>+'|'|'|'|-'|'|+')
            return 1 ;;
    esac
    return 0
}

run_health_gate() {
    local applied="$1" attempts="$2" health i
    health="$(health_command "$applied")"
    [ -n "$health" ] || return 0
    if ! health_command_is_usable "$health"; then
        echo "ERROR: x-health-cmd must be a single-line command; got ${health}" >&2
        echo "       (a YAML block scalar such as '>-' puts the command on the next" >&2
        echo "        line, where this cannot read it — write it on one line)" >&2
        return 1
    fi
    for i in $(seq 1 "$attempts"); do
        if bash -c "$health" > /dev/null 2>&1; then
            return 0
        fi
        [ "$i" -lt "$attempts" ] && sleep 5
    done
    return 1
}

# Last known health verdict for a stack, so an unhealthy one is re-checked on
# later ticks. Without this, convergence and health were conflated: a stack that
# deployed cleanly but never came up matched `cmp -s` on the next tick, skipped
# the rest of the loop, and reported success forever while still being down.
health_state() {
    cat "$1/.gitops-health" 2>/dev/null || echo unknown
}
set_health_state() {
    printf '%s' "$2" > "$1/.gitops-health"
}

rc=0
for dir in "$BOX_DIR"/*/; do
    app="$(basename "$dir")"
    applied_dir="$STACKS_DIR/$app"
    applied="$applied_dir/docker-compose.yml"
    # PORTABILITY: explicit template so BSD mktemp (which historically required
    # one) behaves identically to GNU mktemp.
    desired="$(mktemp "${TMPDIR:-/tmp}/dabba-reconcile.XXXXXX")"
    render_desired "$app" "$dir" "$desired"
    case "$?" in
        0) ;;                                      # rendered
        2) rm -f "$desired"; continue ;;           # nothing declared here
        *) rm -f "$desired"; rc=1; continue ;;     # any failure is a failure
    esac

    # Cron units sync independently of compose convergence (a unit-only
    # change must land even when the compose file is unchanged).
    sync_cron "$app" "$dir"

    if grep -q "PLACEHOLDER-SET-BY-FIRST-RELEASE" "$desired"; then
        echo "$app: placeholder pins; waiting for first release"
        rm -f "$desired"
        continue
    fi
    if [ -f "$applied" ] && cmp -s "$desired" "$applied"; then
        rm -f "$desired"
        # A rotated secret changes nothing in git, so a no-diff tick must still
        # refresh the .env — otherwise rotation would require a cosmetic commit.
        if ! sync_secrets "$applied_dir" "$applied" "$app" > /dev/null; then
            echo "ERROR: $app: secret refresh failed" >&2
            rc=1
        fi
        # In sync. That is not the same as working: re-check a stack we last saw
        # unhealthy, so it keeps alerting until it recovers or someone fixes it.
        if [ "$(health_state "$applied_dir")" = "failed" ]; then
            if run_health_gate "$applied" 1; then
                set_health_state "$applied_dir" ok
                echo "$app: recovered (health gate passing again)"
            else
                echo "ERROR: $app: still failing its health gate" >&2
                rc=1
            fi
        fi
        continue
    fi

    echo "$app: desired state changed; deploying"
    mkdir -p "$applied_dir"
    # Pre-create ./-relative bind-mount sources as this user; otherwise
    # dockerd creates them root-owned and non-root containers cannot write
    # their own data dirs (sqlite error 14, learned the hard way).
    extract_bind_sources "$desired" | while IFS= read -r rel; do
        mkdir -p "$applied_dir/$rel"
    done
    # Namespace every managed stack as project "gitops-<app>" so its
    # containers/volumes/networks can never collide with (or adopt) anything
    # that predates this process on the box. Written to .env so manual
    # docker compose runs in the stack dir land in the same project.
    #
    # DABBA_UID/DABBA_GID exist because pre-creating the bind sources above is only
    # half of the ownership problem. The directories are made with this user and
    # this umask, so on a host with the common umask 022 they are mode 755 owned by
    # the host user — and an image whose server runs as a different uid cannot write
    # them. OpenBao runs its server as uid 100: `bao operator init` fails with a
    # permission error naming a path inside the container, and the whole secrets arc
    # fails with it. It survives only where the umask happens to be 002 and the
    # container shares the group. A stack in that position declares
    # `user: "${DABBA_UID}:${DABBA_GID}"` and writes as the uid that owns its data.
    ensure_env() {
        grep -qs "^$1=" "$applied_dir/.env" || echo "$1=$2" >> "$applied_dir/.env"
    }
    ensure_env COMPOSE_PROJECT_NAME "gitops-$app"
    ensure_env DABBA_UID "$(id -u)"
    ensure_env DABBA_GID "$(id -g)"
    cp "$desired" "$applied"
    rm -f "$desired"

    # Secrets must land before `compose up` interpolates the .env, and a failure
    # here must stop the deploy: a stack brought up with an unresolved secret
    # starts with an empty value and fails somewhere far from the cause.
    if ! sync_secrets "$applied_dir" "$applied" "$app"; then
        rm -f "$applied"    # keep the diff so the next tick retries
        rc=1
        continue
    fi
    # --force-recreate: compose does not consider inline `configs: content:`
    # changes when deciding whether to recreate, so a config-only edit would
    # silently keep the old container running. We only reach this point when
    # the rendered file changed, so recreation is always the intent.
    # --remove-orphans is scoped to THIS stack's project (gitops-<app>) only,
    # so it removes a service this stack no longer declares but can never touch
    # another stack or a pre-existing/legacy workload (different project).
    if ! (cd "$applied_dir" && docker compose pull -q && docker compose up -d --force-recreate --remove-orphans); then
        echo "ERROR: $app: compose pull/up failed" >&2
        rm -f "$applied"    # keep the diff so the next tick retries
        rc=1
        continue
    fi

    if run_health_gate "$applied" 24; then
        set_health_state "$applied_dir" ok
        if [ -n "$(health_command "$applied")" ]; then
            echo "$app: converged and healthy"
        else
            echo "$app: converged (no x-health-cmd defined)"
        fi
    else
        set_health_state "$applied_dir" failed
        echo "ERROR: $app: deployed but health check failed after 120s" >&2
        rc=1
    fi
done
exit $rc
