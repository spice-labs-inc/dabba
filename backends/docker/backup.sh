#!/usr/bin/env bash
#
# Back up one stack's persistent data to a timestamped archive.
#
#     backup.sh <app>
#
# The reconciler already syncs per-app scheduled jobs from git, and the shipped
# example units call this. That mechanism existed before this script did and
# nothing used it, which is the difference between having backups and having a
# way to have backups.
#
# WHAT IS ARCHIVED: the stack's `./`-relative bind-mount sources under
# <STACKS_DIR>/<app>/ — the same directories the reconciler pre-creates. That is
# where a compose stack's state lives on a bare-OS box.
#
# WHAT IS NOT, and deliberately:
#   * `.env` — it holds secrets RESOLVED from OpenBao, and the reconciler rewrites
#     it every tick. Archiving it would copy live secrets into a tarball that
#     outlives them, to restore something that regenerates itself anyway.
#   * `docker-compose.yml` and `.gitops-health` — reconciler-owned, and the
#     compose file is in git already. A backup of state should not carry a second
#     copy of the desired state; restoring one would fight the next tick.
#   * Docker images. They are named in the compose file and pullable.
#
# CONSISTENCY: archiving a live data directory can capture a torn write — half a
# database page, an index that does not match its table. So the stack is stopped
# for the copy and started again afterwards, which on one box is a few seconds of
# downtime at whatever hour the timer fires. Set BACKUP_QUIESCE=0 to archive hot
# if a stack can genuinely tolerate it (an application that fsyncs a consistent
# file, or one whose data is a cache). The default is the safe one, because a
# backup you cannot trust is worse than an outage you planned.
#
# The restart is in a trap, so a failure mid-archive still brings the stack back.
#
# Exit codes: 0 = archived and pruned; 1 = anything else, which is what the
# job unit's OnFailure= turns into an alert.
set -uo pipefail

APP="${1:-}"
if [ -z "$APP" ]; then
    echo "usage: backup.sh <app>" >&2
    exit 1
fi

STACKS_DIR="${STACKS_DIR:-$HOME/stacks}"
BACKUPS_DIR="${BACKUPS_DIR:-$HOME/backups}"
BACKUP_KEEP="${BACKUP_KEEP:-7}"
BACKUP_QUIESCE="${BACKUP_QUIESCE:-1}"

STACK_DIR="$STACKS_DIR/$APP"
DESTINATION="$BACKUPS_DIR/$APP"

if [ ! -d "$STACK_DIR" ]; then
    echo "ERROR: $APP: no applied stack at $STACK_DIR" >&2
    exit 1
fi

# The same set the reconciler pre-creates: every ./-relative bind-mount source in
# the applied compose file. Reading it from the compose file rather than taking
# the whole directory is what keeps .env and the reconciler's own files out.
sources() {
    awk '
        /^[[:space:]]*-[[:space:]]*\.\// {
            line = $0
            sub(/^[[:space:]]*-[[:space:]]*/, "", line)
            if (index(line, ":") > 0) { sub(/:.*$/, "", line); print line }
            next
        }
        /^[[:space:]]*source:[[:space:]]*\.\// {
            line = $0
            sub(/^[[:space:]]*source:[[:space:]]*/, "", line)
            gsub(/\047/, "", line); gsub(/\042/, "", line)
            sub(/[[:space:]]*$/, "", line)
            print line
        }
    ' "$STACK_DIR/docker-compose.yml" | sort -u
}

paths="$(sources)"
if [ -z "$paths" ]; then
    echo "ERROR: $APP: declares no bind-mounted data, so there is nothing to back" >&2
    echo "       up. A stack whose state is in a named volume is not covered by" >&2
    echo "       this; give it a ./-relative bind mount under its stack directory." >&2
    exit 1
fi

started=""
restart_if_needed() {
    if [ -n "$started" ]; then
        # Best effort, and unconditional: leaving the stack down after a failed
        # backup would turn a missing archive into an outage.
        (cd "$STACK_DIR" && docker compose start > /dev/null 2>&1) \
            || echo "ERROR: $APP: could not restart the stack after backup" >&2
    fi
}
trap restart_if_needed EXIT

if [ "$BACKUP_QUIESCE" != "0" ]; then
    if ! (cd "$STACK_DIR" && docker compose stop > /dev/null 2>&1); then
        echo "ERROR: $APP: could not stop the stack to archive it consistently" >&2
        exit 1
    fi
    started=1
fi

mkdir -p "$DESTINATION"
# UTC, and sortable, so `ls` order is chronological order on every box.
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
archive="$DESTINATION/$APP-$stamp.tar.gz"

# Written to a partial name and renamed only on success: an interrupted run must
# not leave something that looks like a usable backup.
partial="$archive.partial"
# shellcheck disable=SC2086  # deliberate word splitting over the path list
if ! (cd "$STACK_DIR" && tar -czf "$partial" $paths) 2>/dev/null; then
    echo "ERROR: $APP: archiving failed" >&2
    rm -f "$partial"
    exit 1
fi

# Prove the archive is readable before it counts as a backup. A tarball that
# cannot be listed is not a backup, and finding that out at restore time is
# finding out too late.
if ! tar -tzf "$partial" > /dev/null 2>&1; then
    echo "ERROR: $APP: the archive it just wrote is not readable" >&2
    rm -f "$partial"
    exit 1
fi
mv "$partial" "$archive"
echo "$APP: backed up to $archive"

# Retention. Keep the newest BACKUP_KEEP, delete the rest. Sorted by name, which
# is chronological because the stamp is.
if [ "$BACKUP_KEEP" -gt 0 ] 2>/dev/null; then
    ls -1 "$DESTINATION"/"$APP"-*.tar.gz 2>/dev/null \
        | sort -r \
        | tail -n +$((BACKUP_KEEP + 1)) \
        | while IFS= read -r old; do
            rm -f "$old"
            echo "$APP: pruned $(basename "$old")"
        done
fi
