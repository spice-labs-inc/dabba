#!/usr/bin/env bash
#
# Reverse install.sh for THIS host: stop and remove the dabba DockerBackend
# reconcile loop (LaunchAgent on macOS, systemd user units on Linux).
#
# This removes only the RECONCILE LOOP. It never runs `docker compose down`,
# never touches stacks under STACKS_DIR, and never removes per-app scheduled
# jobs — decommissioning a stack or its cron agents stays a deliberate manual
# act, matching the reconciler's own never-destroy contract.
#
# DABBA_ENVIRONMENT selects WHICH environment's loop to remove (default
# "default"), matching install.sh. Removing one environment's loop must never
# stop another's.
set -euo pipefail

PLATFORM="$(uname -s)"
DABBA_ENVIRONMENT="${DABBA_ENVIRONMENT:-default}"
LABEL="io.spicelabs.dabba.reconcile.$DABBA_ENVIRONMENT"
UNIT_BASE="gitops-reconcile-$DABBA_ENVIRONMENT"
JOB_ALERT="$UNIT_BASE-job-alert@"

if [ "$PLATFORM" = "Darwin" ]; then
    UID_NUM="$(id -u)"
    DEST="$HOME/Library/LaunchAgents/$LABEL.plist"

    launchctl bootout "gui/$UID_NUM/$LABEL" > /dev/null 2>&1 || true
    rm -f "$DEST"
    echo "removed LaunchAgent $LABEL"
    echo "(left running stacks and any per-app cron LaunchAgents untouched)"
else
    UNIT_DIR="$HOME/.config/systemd/user"

    systemctl --user disable --now "$UNIT_BASE.timer" > /dev/null 2>&1 || true
    rm -f "$UNIT_DIR/$UNIT_BASE.timer" \
          "$UNIT_DIR/$UNIT_BASE.service" \
          "$UNIT_DIR/$UNIT_BASE-alert.service" \
          "$UNIT_DIR/$JOB_ALERT.service"
    systemctl --user daemon-reload
    echo "removed systemd user units $UNIT_BASE.{timer,service,-alert,-job-alert@}"
    echo "(left running stacks and any per-app cron units untouched)"
fi
