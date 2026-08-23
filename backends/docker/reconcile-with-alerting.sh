#!/usr/bin/env bash
#
# macOS entry point for the reconcile loop: run reconcile.sh, and if it exits
# non-zero, raise the alert.
#
# WHY THIS EXISTS. On Linux, systemd's `OnFailure=gitops-reconcile-alert.service`
# does this for us — the unit manager notices the failure and activates the alert
# unit. launchd has no OnFailure equivalent: a LaunchAgent that exits non-zero is
# simply a job that exited, and the exit code goes nowhere. Without this wrapper
# a failing reconcile on macOS writes to a log file and nothing else, which means
# the platform detects its own failures on exactly one of the two platforms it
# claims to support — and macOS is the one people develop on.
#
# It also gives the Mac path an equivalent of the systemd unit's
# `EnvironmentFile=-%h/.dabba-reconcile.env`, so SLACK_WEBHOOK_URL and friends can
# reach reconcile-alert.sh. launchd plists cannot read an environment file, and a
# webhook URL does not belong baked into a world-readable plist in any case.
#
# Portable bash 3.2 (macOS default), though only launchd invokes it today.
set -uo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")" && pwd)"

# Optional per-box secrets for the alert transport. Absent is the normal case.
ENV_FILE="$HOME/.dabba-reconcile.env"
if [ -f "$ENV_FILE" ]; then
    # shellcheck disable=SC1090  # path is per-box by design
    . "$ENV_FILE"
fi

"$BACKEND_DIR/reconcile.sh"
rc=$?

if [ "$rc" -ne 0 ]; then
    # The alert script takes a unit name for its journal lookup; on macOS there is
    # no journal, so pass the launchd label — it is what identifies this job in
    # `launchctl print` and in the log file.
    "$BACKEND_DIR/reconcile-alert.sh" "io.spicelabs.dabba.reconcile" || true
fi

exit "$rc"
