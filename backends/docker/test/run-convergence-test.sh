#!/usr/bin/env bash
#
# Convergence and failure-handling tests for reconcile.sh.
#
# run-test.sh covers the happy path on the LEGACY verbatim compose file. This
# covers what that one cannot reach:
#
#   1. the base + override render path — including long-form `source:` bind
#      mounts, which is the form `docker compose config` actually emits and the
#      whole reason the original's GNU-only `grep -oP` had to become portable
#      awk. Until now the fix that justified the port had no test at all;
#   2. a stack that stays unhealthy keeps alerting on every tick instead of
#      exactly once;
#   3. a gitops branch that does not exist fails loudly and immediately rather
#      than looking like a transient network blip forever;
#   4. a genuine transient fetch failure stays quiet, counts, and eventually
#      escalates;
#   5. a malformed override explains itself instead of failing silently;
#   6. a non-default GITOPS_APPS_DIR is honoured.
#
# Everything lives in a temp dir with a local bare repo as "origin" — no
# network. Only tests 1 and 2 need a running Docker daemon; the rest are skipped
# with a clear message if it is absent. Fully self-cleaning. Portable bash 3.2.
set -uo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RECONCILE_SH="$BACKEND_DIR/reconcile.sh"

BOX="testbox"
PORT="18092"
HEALTH_APP="dabbahealth"
HEALTH_PROJECT="gitops-$HEALTH_APP"

# Every compose project this test can create. Cleanup walks this list rather
# than one project: an earlier version tore down only the health stack and left
# the base+override stack from test 1 running on the machine.
TEST_PROJECTS="gitops-dabbaoverlay $HEALTH_PROJECT gitops-dabbabroken"
# Containers this test starts directly, outside any compose project.
TEST_CONTAINERS="dabba-health-fix"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-converge-test.XXXXXX")"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }

have_docker() { docker info > /dev/null 2>&1; }

cleanup() {
    echo
    echo "--- cleanup ---"
    if have_docker; then
        for container in $TEST_CONTAINERS; do
            docker rm -f "$container" > /dev/null 2>&1
        done
        for project in $TEST_PROJECTS; do
            ids="$(docker ps -aq --filter "label=com.docker.compose.project=$project" 2>/dev/null)"
            if [ -n "$ids" ]; then
                # shellcheck disable=SC2086  # deliberate word splitting over ids
                docker rm -f $ids > /dev/null 2>&1
            fi
            docker network rm "${project}_default" > /dev/null 2>&1 || true
        done
        # Prove the residue is gone rather than assuming the removals worked.
        residue=""
        for project in $TEST_PROJECTS; do
            left="$(docker ps -aq --filter "label=com.docker.compose.project=$project" 2>/dev/null)"
            [ -n "$left" ] && residue="$residue $project"
        done
        for container in $TEST_CONTAINERS; do
            docker ps -aq --filter "name=^${container}$" 2>/dev/null | grep -q . \
                && residue="$residue $container"
        done
        if [ -n "$residue" ]; then
            echo "  WARNING: residue survived cleanup:$residue"
        else
            echo "  no containers or networks left for:$(printf ' %s' $TEST_PROJECTS)"
        fi
    fi
    rm -rf "$SCRATCH"
    if [ -d "$SCRATCH" ]; then
        echo "  WARNING: $SCRATCH survived cleanup"
    else
        echo "  removed $SCRATCH"
    fi
}
trap cleanup EXIT

# Build a throwaway gitops repo in $1 (work tree) with a bare origin, on branch $2.
# Everything after is committed and pushed by commit_and_push.
new_gitops_repo() {
    local work="$1" branch="$2" origin="${1}-origin.git"
    git init -q --bare "$origin"
    git clone -q "$origin" "$work" 2>/dev/null
    git -C "$work" config user.email dabba-test@localhost
    git -C "$work" config user.name "dabba test"
    git -C "$work" checkout -q -b "$branch"
}

commit_and_push() {
    local work="$1" branch="$2" message="$3"
    git -C "$work" add -A
    git -C "$work" commit -q -m "$message"
    git -C "$work" push -q -u origin "$branch" 2>/dev/null
}

# Run the reconciler against a prepared repo. Extra env comes in as KEY=VALUE args.
run_reconcile() {
    local gitops="$1" stacks="$2"; shift 2
    env GITOPS_DIR="$gitops" STACKS_DIR="$stacks" BOX_NAME="$BOX" "$@" \
        /bin/bash "$RECONCILE_SH" 2>&1
}

###############################################################################
echo "=== 1. base + override render, with a long-form bind mount ==="
###############################################################################
# The override places the app on this box; the base carries the real definition.
# `docker compose config` normalises the volume to long form (`source: ./data`),
# which is exactly the shape the original grep -oP could not see.
if ! have_docker; then
    echo "  SKIP: needs a docker daemon"
else
    G="$SCRATCH/overlay"; S="$SCRATCH/overlay-stacks"
    APP="dabbaoverlay"
    new_gitops_repo "$G" main
    mkdir -p "$G/apps/base/$APP" "$G/apps/$BOX/$APP"
    cat > "$G/apps/base/$APP/docker-compose.yml" <<YAML
services:
  web:
    image: nginx:alpine
    volumes:
      - ./data:/data:rw
YAML
    printf 'services: {}\n' > "$G/apps/$BOX/$APP/docker-compose.override.yml"
    commit_and_push "$G" main "base+override for $APP"

    out="$(run_reconcile "$G" "$S")"
    echo "$out" | sed 's/^/  | /'

    if [ -f "$S/$APP/docker-compose.yml" ]; then
        pass "base+override rendered to an applied compose file"
    else
        fail "base+override produced no applied compose file"
    fi
    if grep -q 'source:' "$S/$APP/docker-compose.yml" 2>/dev/null; then
        pass "rendered file uses long-form volumes (the shape grep -oP missed)"
    else
        fail "rendered file has no long-form volume; test would not prove the awk fix"
    fi
    if [ -d "$S/$APP/data" ]; then
        pass "long-form bind mount source ./data was pre-created"
    else
        fail "long-form bind mount source ./data was NOT pre-created (the awk regression)"
    fi
fi

###############################################################################
echo
echo "=== 2. an unhealthy stack keeps alerting on every tick ==="
###############################################################################
# The regression this guards: convergence and health used to be conflated. A
# stack that deployed but never became healthy matched `cmp -s` on the next tick
# and skipped the rest of the loop, so it alerted once and then reported success
# forever while still being down.
if ! have_docker; then
    echo "  SKIP: needs a docker daemon"
else
    G="$SCRATCH/health"; S="$SCRATCH/health-stacks"
    new_gitops_repo "$G" main
    mkdir -p "$G/apps/$BOX/$HEALTH_APP"
    # nginx comes up fine; the health command probes a port nothing listens on,
    # so the stack is deployed-but-unhealthy — the exact dangerous state.
    cat > "$G/apps/$BOX/$HEALTH_APP/docker-compose.yml" <<YAML
services:
  web:
    image: nginx:alpine
    ports:
      - "127.0.0.1:${PORT}:80"
x-health-cmd: curl -fsS http://127.0.0.1:19999/ >/dev/null 2>&1
YAML
    commit_and_push "$G" main "add $HEALTH_APP with a failing health gate"

    echo "  (first tick deploys, then waits out the 120s health gate)"
    out1="$(run_reconcile "$G" "$S")"; rc1=$?
    echo "$out1" | tail -3 | sed 's/^/  | /'
    [ "$rc1" -ne 0 ] && pass "first tick exits non-zero (alerts)" \
        || fail "first tick exited 0 despite a failed health gate"
    [ "$(cat "$S/$HEALTH_APP/.gitops-health" 2>/dev/null)" = "failed" ] \
        && pass "health verdict recorded as failed" || fail "health verdict not recorded"

    out2="$(run_reconcile "$G" "$S")"; rc2=$?
    echo "$out2" | sed 's/^/  | /'
    if [ "$rc2" -ne 0 ]; then
        pass "second tick STILL exits non-zero (this is the regression guard)"
    else
        fail "second tick exited 0 — an unhealthy stack went silent again"
    fi
    echo "$out2" | grep -q "still failing its health gate" \
        && pass "second tick names the still-failing stack" \
        || fail "second tick did not report the still-failing stack"

    # Now make it healthy without changing the desired state: the recovery path
    # must be reachable from a no-diff tick too.
    docker run -d --rm --name dabba-health-fix -p 127.0.0.1:19999:80 nginx:alpine > /dev/null 2>&1
    sleep 2
    out3="$(run_reconcile "$G" "$S")"; rc3=$?
    docker rm -f dabba-health-fix > /dev/null 2>&1
    echo "$out3" | sed 's/^/  | /'
    [ "$rc3" -eq 0 ] && pass "recovered stack exits 0 again" \
        || fail "recovered stack still exits non-zero"
    echo "$out3" | grep -q "recovered" && pass "recovery is reported" \
        || fail "recovery was not reported"
fi

###############################################################################
echo
echo "=== 3. a branch that does not exist fails loudly and at once ==="
###############################################################################
G="$SCRATCH/branch"; S="$SCRATCH/branch-stacks"
new_gitops_repo "$G" main
mkdir -p "$G/apps/$BOX"
echo "placeholder" > "$G/apps/$BOX/.keep"
commit_and_push "$G" main "seed"

out="$(run_reconcile "$G" "$S" GITOPS_BRANCH=release)"; rc=$?
echo "$out" | sed 's/^/  | /'
[ "$rc" -eq 1 ] && pass "missing branch exits 1 (alerts immediately)" \
    || fail "missing branch exited $rc; it used to look transient forever"
echo "$out" | grep -q "does not exist on origin" \
    && pass "the error names the actual problem" || fail "error does not explain the cause"
[ ! -f "$S/.gitops-fetch-failures" ] \
    && pass "a terminal failure is not filed as a transient one" \
    || fail "terminal failure incremented the transient counter"

###############################################################################
echo
echo "=== 4. a transient fetch failure stays quiet, counts, then escalates ==="
###############################################################################
G="$SCRATCH/transient"; S="$SCRATCH/transient-stacks"
new_gitops_repo "$G" main
mkdir -p "$G/apps/$BOX"
echo "placeholder" > "$G/apps/$BOX/.keep"
commit_and_push "$G" main "seed"
# Make the remote unreachable: point origin at a path that no longer exists.
git -C "$G" remote set-url origin "$SCRATCH/vanished-origin.git"

quiet_ticks=0
tick=0
while [ "$tick" -lt 4 ]; do
    tick=$((tick + 1))
    run_reconcile "$G" "$S" > /dev/null 2>&1
    [ $? -eq 0 ] && quiet_ticks=$((quiet_ticks + 1))
done
[ "$quiet_ticks" -eq 4 ] && pass "first 4 unreachable ticks stay quiet (exit 0)" \
    || fail "expected 4 quiet ticks, got $quiet_ticks"
[ "$(cat "$S/.gitops-fetch-failures" 2>/dev/null)" = "4" ] \
    && pass "consecutive failures are counted" \
    || fail "failure counter is $(cat "$S/.gitops-fetch-failures" 2>/dev/null), expected 4"

out="$(run_reconcile "$G" "$S")"; rc=$?
echo "$out" | sed 's/^/  | /'
[ "$rc" -eq 1 ] && pass "the 5th consecutive failure escalates to exit 1" \
    || fail "sustained outage never escalated (exit $rc)"

# Restoring the remote must clear the counter, not leave the box latched.
git -C "$G" remote set-url origin "${G}-origin.git"
run_reconcile "$G" "$S" > /dev/null 2>&1
[ ! -f "$S/.gitops-fetch-failures" ] \
    && pass "counter clears once fetching works again" \
    || fail "counter survived a successful fetch"

###############################################################################
echo
echo "=== 5. a malformed override explains itself ==="
###############################################################################
if ! have_docker; then
    echo "  SKIP: needs the docker CLI"
else
    G="$SCRATCH/broken"; S="$SCRATCH/broken-stacks"
    APP="dabbabroken"
    new_gitops_repo "$G" main
    mkdir -p "$G/apps/base/$APP" "$G/apps/$BOX/$APP"
    cat > "$G/apps/base/$APP/docker-compose.yml" <<'YAML'
services:
  web:
    image: nginx:alpine
YAML
    # `ports` must be a list; a mapping is a type error compose will reject.
    cat > "$G/apps/$BOX/$APP/docker-compose.override.yml" <<'YAML'
services:
  web:
    ports:
      not: a-list
YAML
    commit_and_push "$G" main "add a malformed override"

    out="$(run_reconcile "$G" "$S")"; rc=$?
    echo "$out" | sed 's/^/  | /'
    [ "$rc" -eq 1 ] && pass "a render failure exits 1" || fail "render failure exited $rc"
    echo "$out" | grep -q "rendering base+override failed" \
        && pass "the failure is reported rather than swallowed" \
        || fail "no diagnostic for the malformed override"
    if echo "$out" | grep -qi "ports\|invalid\|expected\|must be"; then
        pass "compose's own explanation is surfaced"
    else
        fail "compose's stderr was still discarded"
    fi
    [ ! -f "$S/$APP/docker-compose.yml" ] \
        && pass "nothing was applied from a failed render" \
        || fail "a failed render still applied a compose file"
fi

###############################################################################
echo
echo "=== 6. a non-default GITOPS_APPS_DIR is honoured ==="
###############################################################################
G="$SCRATCH/appsdir"; S="$SCRATCH/appsdir-stacks"
new_gitops_repo "$G" main
mkdir -p "$G/deployments/$BOX/someapp"
# No compose file: the reconciler should still find and traverse the directory,
# which it can only do if it looked under `deployments` rather than `apps`.
echo "marker" > "$G/deployments/$BOX/someapp/README"
commit_and_push "$G" main "apps under a non-default subtree"

out="$(run_reconcile "$G" "$S" GITOPS_APPS_DIR=deployments)"; rc=$?
echo "$out" | sed 's/^/  | /'
if echo "$out" | grep -q "no stacks declared for box"; then
    fail "GITOPS_APPS_DIR=deployments was ignored (looked under apps)"
else
    pass "GITOPS_APPS_DIR=deployments was used"
fi

out="$(run_reconcile "$G" "$S")"; rc=$?
echo "$out" | grep -q "no stacks declared for box" \
    && pass "the default (apps) genuinely finds nothing here — test is not vacuous" \
    || fail "default apps dir unexpectedly found stacks"

###############################################################################
echo
if [ "$fails" -eq 0 ]; then
    echo "=== ALL ASSERTIONS PASSED ==="
else
    echo "=== $fails ASSERTION(S) FAILED ==="
fi
exit "$fails"
