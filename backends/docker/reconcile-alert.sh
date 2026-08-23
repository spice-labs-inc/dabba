#!/usr/bin/env bash
#
# Failure alert for the dabba DockerBackend reconcile loop.
#
# On Linux it is wired via `OnFailure=gitops-reconcile-alert.service` on the
# systemd user unit. On macOS launchd has no OnFailure equivalent — the loop's
# stdout/stderr is captured to a log file by the LaunchAgent instead, and this
# script can be run by hand against that log. It is kept portable (BSD + GNU)
# so the same file serves both hosts.
#
# Delivers, in order of preference:
#   1. Slack incoming webhook (SLACK_WEBHOOK_URL from the environment)
#   2. loud journal/stderr entry + a context file under the temp dir
# Rate-limited to one alert per hour; the context file is always refreshed.
set -euo pipefail

UNIT="${1:-gitops-reconcile.service}"
TMP="${TMPDIR:-/var/tmp}"
STAMP="$TMP/dabba-reconcile-last-alert"
CONTEXT="$TMP/dabba-reconcile-last-failure.txt"

# PORTABILITY: mtime of a file — BSD `stat -f %m` first, GNU `stat -c %Y` fallback.
file_mtime() {
    stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

{
    date -u +"%Y-%m-%dT%H:%M:%SZ"
    echo "unit: $UNIT  box: $(hostname -s)"
    # journalctl only exists on systemd hosts; harmless no-op elsewhere.
    journalctl --user -u "$UNIT" -n 40 --no-pager 2>/dev/null || true
} > "$CONTEXT"

if [ -f "$STAMP" ] && [ "$(( $(date +%s) - $(file_mtime "$STAMP") ))" -lt 3600 ]; then
    echo "alert suppressed (rate limit); context in $CONTEXT"
    exit 0
fi

MSG="dabba reconcile FAILED on $(hostname -s) ($UNIT). Last log lines in $CONTEXT on the box."

if [ -n "${SLACK_WEBHOOK_URL:-}" ]; then
    if curl -sf --max-time 15 -X POST -H 'Content-type: application/json' \
        --data "$(printf '{"text":"%s"}' "$MSG")" "$SLACK_WEBHOOK_URL" > /dev/null; then
        touch "$STAMP"
        exit 0
    fi
fi

# systemd-cat on Linux, plain stderr everywhere else.
if command -v systemd-cat > /dev/null 2>&1; then
    echo "DABBA-RECONCILE-FAILURE: $MSG" | systemd-cat -t dabba-reconcile -p err
else
    echo "DABBA-RECONCILE-FAILURE: $MSG" >&2
fi
touch "$STAMP"
