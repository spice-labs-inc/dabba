#!/usr/bin/env bash
#
# macOS-only verification that reconcile.sh's launchd cron sync
# (sync_launchd_agents) installs, renders, tracks, and removes per-app
# scheduled-job LaunchAgents. Uses a calendar schedule far in the future so
# the job never actually fires; asserts the agent is bootstrapped into the
# user GUI domain with the __TOKENS__ rendered, then that dropping the plist
# from git boots it out and removes it. Fully self-cleaning. Portable bash 3.2.
set -uo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
    echo "SKIP: launchd cron test only runs on macOS"
    exit 0
fi

BACKEND_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RECONCILE_SH="$BACKEND_DIR/reconcile.sh"

APP="dabbacron"
BOX="testbox"
LABEL="io.spicelabs.dabba.cron.$APP.probe"
UID_NUM="$(id -u)"
AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-cron-test.XXXXXX")"
ORIGIN="$SCRATCH/origin.git"
GITOPS_DIR="$SCRATCH/gitops"
STACKS_DIR="$SCRATCH/stacks"
APP_DIR="$GITOPS_DIR/apps/$BOX/$APP"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }

cleanup() {
    echo "--- cleanup ---"
    launchctl bootout "gui/$UID_NUM/$LABEL" > /dev/null 2>&1 || true
    rm -f "$AGENT"
    if [ -d "$STACKS_DIR/$APP" ]; then
        ( cd "$STACKS_DIR/$APP" && docker compose down -v --remove-orphans > /dev/null 2>&1 ) || true
    fi
    rm -rf "$SCRATCH"
    echo "  booted out $LABEL, removed $AGENT, tore down stack, removed $SCRATCH"
}
trap cleanup EXIT

run_reconcile() {
    GITOPS_DIR="$GITOPS_DIR" STACKS_DIR="$STACKS_DIR" BOX_NAME="$BOX" \
        /bin/bash "$RECONCILE_SH" 2>&1
}

echo "=== build gitops repo with one app that ships a launchd cron plist ==="
git init -q --bare "$ORIGIN"
git clone -q "$ORIGIN" "$GITOPS_DIR"
git -C "$GITOPS_DIR" config user.email dabba-test@localhost
git -C "$GITOPS_DIR" config user.name "dabba test"
git -C "$GITOPS_DIR" checkout -q -b main
mkdir -p "$APP_DIR/launchd"

# A trivial long-lived service (so the compose convergence is clean) plus a
# scheduled-job plist template using the __TOKENS__ the reconciler substitutes.
# We only assert on the cron agent here; the sleeper just keeps `up` happy.
cat > "$APP_DIR/docker-compose.yml" <<'YAML'
services:
  idle:
    image: busybox
    command: ["sh", "-c", "sleep 100000"]
    restart: unless-stopped
YAML
cat > "$APP_DIR/launchd/$LABEL.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>__DOCKER__</string>
        <string>compose</string>
        <string>--profile</string>
        <string>jobs</string>
        <string>run</string>
        <string>--rm</string>
        <string>probe</string>
    </array>
    <key>WorkingDirectory</key>
    <string>__STACK_DIR__</string>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Month</key><integer>1</integer>
        <key>Day</key><integer>1</integer>
        <key>Hour</key><integer>4</integer>
        <key>Minute</key><integer>0</integer>
    </dict>
</dict>
</plist>
PLIST
git -C "$GITOPS_DIR" add -A
git -C "$GITOPS_DIR" commit -q -m "add $APP with cron plist"
git -C "$GITOPS_DIR" push -q -u origin main

echo
echo "=== reconcile: expect the LaunchAgent to be installed + bootstrapped ==="
out1="$(run_reconcile)"; echo "$out1" | sed 's/^/  | /'

echo "$out1" | grep -q "$APP: launchd agent $LABEL.plist synced" \
    && pass "reconcile reported syncing the agent" || fail "agent sync not reported"
[ -f "$AGENT" ] && pass "plist installed at $AGENT" || fail "plist not installed"
if launchctl print "gui/$UID_NUM/$LABEL" > /dev/null 2>&1; then
    pass "agent bootstrapped into gui/$UID_NUM"
else
    fail "agent not present in gui/$UID_NUM"
fi
# __TOKENS__ must be rendered, not left literal.
if grep -q "__DOCKER__\|__STACK_DIR__\|__APP__" "$AGENT"; then
    fail "installed plist still contains un-rendered __TOKENS__"
else
    pass "tokens rendered in installed plist"
fi
grep -q "$STACKS_DIR/$APP" "$AGENT" \
    && pass "WorkingDirectory rendered to STACKS_DIR/$APP" \
    || fail "WorkingDirectory not rendered"
grep -qs "^$LABEL.plist\$" "$HOME/Library/LaunchAgents/.gitops-$APP.agents" \
    && pass "agent tracked in per-app manifest" || fail "agent not in manifest"

echo
echo "=== drop the plist from git, reconcile: expect removal ==="
rm -f "$APP_DIR/launchd/$LABEL.plist"
git -C "$GITOPS_DIR" commit -q -am "remove $APP cron plist"
git -C "$GITOPS_DIR" push -q origin main
out2="$(run_reconcile)"; echo "$out2" | sed 's/^/  | /'

echo "$out2" | grep -q "$APP: removed launchd agent $LABEL.plist" \
    && pass "reconcile reported removing the agent" || fail "removal not reported"
[ ! -f "$AGENT" ] && pass "plist removed from LaunchAgents" || fail "plist still present"
if launchctl print "gui/$UID_NUM/$LABEL" > /dev/null 2>&1; then
    fail "agent still bootstrapped after removal"
else
    pass "agent booted out of gui/$UID_NUM"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "=== ALL ASSERTIONS PASSED ==="
else
    echo "=== $fails ASSERTION(S) FAILED ==="
fi
exit "$fails"
