#!/usr/bin/env bash
#
# Scheduled-job sync on Linux: the systemd half of what run-cron-macos-test.sh
# covers for launchd.
#
# There was a test for the platform CI cannot run and none for the platform it
# does. That gap mattered more once systemd units stopped being copied verbatim
# and started being token-rendered like the launchd plists: a unit shipped in git
# now depends on the reconciler substituting __BACKEND_DIR__, __STACKS_DIR__,
# __BACKUPS_DIR__, __APP__ and __JOB_ALERT__, and nothing on this platform checked
# that it does.
#
# It asserts what can be asserted without a systemd session, which is everything
# that decides whether a shipped unit is usable:
#
#   1. the unit is installed, with every token substituted;
#   2. the values are the RIGHT ones, not merely present — a job that runs
#      backup.sh has to receive the directories this environment actually uses;
#   3. the unit is tracked in the per-app manifest;
#   4. a unit dropped from git is removed again.
#
# HOME is redirected at the scratch directory, so this never writes into the real
# user's units. The `systemctl --user` calls inside the reconciler then find
# nothing to act on and fail harmlessly, which is fine: enabling a timer is
# systemd's job, and rendering a correct unit is dabba's.
#
# Needs Docker (the reconciler converges a stack alongside the units). Fully
# self-cleaning. Portable bash 3.2.
set -uo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RECONCILE_SH="$BACKEND_DIR/reconcile.sh"

APP="dabbacronlinux"
BOX="testbox"
PROJECT="gitops-$APP"
UNIT="$APP-probe.service"
TIMER="$APP-probe.timer"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-cron-linux.XXXXXX")"
ORIGIN="$SCRATCH/origin.git"
GITOPS_DIR="$SCRATCH/gitops"
STACKS_DIR="$SCRATCH/stacks"
BACKUPS_DIR="$SCRATCH/backups"
FAKE_HOME="$SCRATCH/home"
UNIT_DIR="$FAKE_HOME/.config/systemd/user"
APP_DIR="$GITOPS_DIR/apps/$BOX/$APP"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }
have_docker() { docker info > /dev/null 2>&1; }

cleanup() {
    echo "--- cleanup ---"
    if have_docker && [ -d "$STACKS_DIR/$APP" ]; then
        ( cd "$STACKS_DIR/$APP" && docker compose down -v --remove-orphans > /dev/null 2>&1 ) || true
    fi
    if have_docker; then
        ids="$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
        if [ -n "$ids" ]; then
            # shellcheck disable=SC2086  # deliberate word splitting over ids
            docker rm -f $ids > /dev/null 2>&1
        fi
    fi
    rm -rf "$SCRATCH"
    # Prove the real user's units were never touched.
    if [ -f "$HOME/.config/systemd/user/$UNIT" ]; then
        echo "  WARN: the test wrote into the real unit directory"
    fi
    [ -d "$SCRATCH" ] && echo "  WARN: $SCRATCH survived"
    echo "  cleaned up"
}
trap cleanup EXIT

if [ "$(uname -s)" != "Linux" ]; then
    echo "SKIP: the systemd cron test only runs on Linux"
    exit 0
fi
if ! have_docker; then
    echo "SKIP: needs a running Docker daemon"
    exit 0
fi

run_reconcile() {
    HOME="$FAKE_HOME" GITOPS_DIR="$GITOPS_DIR" STACKS_DIR="$STACKS_DIR" \
        BACKUPS_DIR="$BACKUPS_DIR" BOX_NAME="$BOX" DABBA_ENVIRONMENT="cronlinux" \
        /bin/bash "$RECONCILE_SH" 2>&1
}

echo "=== a gitops repo with one app that ships a systemd cron unit ==="
mkdir -p "$UNIT_DIR"
git init -q --bare "$ORIGIN"
git clone -q "$ORIGIN" "$GITOPS_DIR"
git -C "$GITOPS_DIR" config user.email dabba-test@localhost
git -C "$GITOPS_DIR" config user.name "dabba test"
git -C "$GITOPS_DIR" checkout -q -b main
mkdir -p "$APP_DIR/systemd"

cat > "$APP_DIR/docker-compose.yml" <<'YAML'
services:
  idle:
    image: busybox
    command: ["sh", "-c", "sleep 100000"]
    restart: unless-stopped
YAML

# Every token the reconciler substitutes, in the shape a real backup job uses.
cat > "$APP_DIR/systemd/$UNIT" <<'UNITFILE'
[Unit]
Description=probe job
OnFailure=__JOB_ALERT__%n.service

[Service]
Type=oneshot
ExecStart=/bin/bash __BACKEND_DIR__/backup.sh __APP__
Environment=STACKS_DIR=__STACKS_DIR__
Environment=BACKUPS_DIR=__BACKUPS_DIR__
WorkingDirectory=__STACK_DIR__
UNITFILE
cat > "$APP_DIR/systemd/$TIMER" <<'TIMERFILE'
[Unit]
Description=probe schedule

[Timer]
OnCalendar=*-*-* 03:30:00

[Install]
WantedBy=timers.target
TIMERFILE
git -C "$GITOPS_DIR" add -A
git -C "$GITOPS_DIR" commit -qm "app with a systemd cron unit"
git -C "$GITOPS_DIR" push -q origin main

echo
echo "=== 1. the unit is installed and fully rendered ==="
out="$(run_reconcile)"
if [ -f "$UNIT_DIR/$UNIT" ]; then
    pass "unit installed at $UNIT"
else
    fail "unit not installed (reconcile said: $(printf '%s' "$out" | tail -3))"
fi
if [ -f "$UNIT_DIR/$UNIT" ] && grep -q '__[A-Z_]*__' "$UNIT_DIR/$UNIT"; then
    fail "installed unit still contains un-rendered tokens: $(grep -o '__[A-Z_]*__' "$UNIT_DIR/$UNIT" | sort -u | tr '\n' ' ')"
else
    pass "every token was substituted"
fi

echo
echo "=== 2. the values are the right ones, not merely present ==="
# This is the half that matters: a job pointed at the wrong directories renders
# cleanly and then fails every night looking in the wrong place.
if grep -q "ExecStart=/bin/bash $BACKEND_DIR/backup.sh $APP" "$UNIT_DIR/$UNIT" 2>/dev/null; then
    pass "__BACKEND_DIR__ and __APP__ resolve to this reconciler and this app"
else
    fail "ExecStart is wrong: $(grep '^ExecStart=' "$UNIT_DIR/$UNIT" 2>/dev/null)"
fi
if grep -q "^Environment=STACKS_DIR=$STACKS_DIR$" "$UNIT_DIR/$UNIT" 2>/dev/null; then
    pass "__STACKS_DIR__ resolves to this environment stacks directory"
else
    fail "STACKS_DIR is wrong: $(grep 'STACKS_DIR' "$UNIT_DIR/$UNIT" 2>/dev/null)"
fi
if grep -q "^Environment=BACKUPS_DIR=$BACKUPS_DIR$" "$UNIT_DIR/$UNIT" 2>/dev/null; then
    pass "__BACKUPS_DIR__ resolves to this environment archives directory"
else
    fail "BACKUPS_DIR is wrong: $(grep 'BACKUPS_DIR' "$UNIT_DIR/$UNIT" 2>/dev/null)"
fi
if grep -q "^WorkingDirectory=$STACKS_DIR/$APP$" "$UNIT_DIR/$UNIT" 2>/dev/null; then
    pass "__STACK_DIR__ resolves to this app applied stack"
else
    fail "WorkingDirectory is wrong: $(grep '^WorkingDirectory=' "$UNIT_DIR/$UNIT" 2>/dev/null)"
fi
if grep -q "^OnFailure=gitops-reconcile-cronlinux-job-alert@%n.service$" "$UNIT_DIR/$UNIT" 2>/dev/null; then
    pass "__JOB_ALERT__ resolves to this environment alert unit"
else
    fail "OnFailure is wrong: $(grep '^OnFailure=' "$UNIT_DIR/$UNIT" 2>/dev/null)"
fi

echo
echo "=== 3. the units are tracked in the per-app manifest ==="
MANIFEST="$UNIT_DIR/.gitops-$APP.units"
if [ -f "$MANIFEST" ] && grep -qx "$UNIT" "$MANIFEST" && grep -qx "$TIMER" "$MANIFEST"; then
    pass "both units recorded in the manifest"
else
    fail "manifest missing entries: $(cat "$MANIFEST" 2>/dev/null | tr '\n' ' ')"
fi

echo
echo "=== 4. a unit dropped from git is removed again ==="
rm -f "$APP_DIR/systemd/$UNIT"
git -C "$GITOPS_DIR" add -A
git -C "$GITOPS_DIR" commit -qm "drop the service, keep the timer"
git -C "$GITOPS_DIR" push -q origin main
out="$(run_reconcile)"
if [ ! -f "$UNIT_DIR/$UNIT" ]; then
    pass "the dropped unit was removed from the box"
else
    fail "the dropped unit is still installed"
fi
if [ -f "$MANIFEST" ] && ! grep -qx "$UNIT" "$MANIFEST"; then
    pass "the manifest no longer lists it"
else
    fail "the manifest still lists the dropped unit"
fi
if [ -f "$UNIT_DIR/$TIMER" ]; then
    pass "the unit still in git was left alone"
else
    fail "removing one unit took the other with it"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "=== all systemd cron tests passed ==="
    exit 0
fi
echo "=== $fails systemd cron test(s) FAILED ==="
exit 1
