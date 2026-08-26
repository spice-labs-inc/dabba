#!/usr/bin/env bash
#
# Restore one stack's persistent data from an archive written by backup.sh.
#
#     restore.sh <app> [archive|latest]
#
# THIS REPLACES LIVE DATA. The stack is stopped, the directories the archive
# contains are deleted, the archive is unpacked in their place, and the stack is
# started again. Anything written since that archive is gone.
#
# It is a deliberate manual act, not something the reconcile loop ever does. The
# reconciler's contract is that it never destroys data; restoring is the one
# operation that does, so it stays a thing a person runs on purpose.
#
# It is also the half that decides whether the backups counted. An archive nobody
# has ever restored is a hypothesis, so the end-to-end test performs a real
# restore and asserts the value comes back.
#
# Exit codes: 0 = restored and the stack is running again; 1 = anything else.
set -uo pipefail

APP="${1:-}"
WHICH="${2:-latest}"
if [ -z "$APP" ]; then
    echo "usage: restore.sh <app> [archive|latest]" >&2
    exit 1
fi

STACKS_DIR="${STACKS_DIR:-$HOME/stacks}"
BACKUPS_DIR="${BACKUPS_DIR:-$HOME/backups}"
STACK_DIR="$STACKS_DIR/$APP"
DESTINATION="$BACKUPS_DIR/$APP"

if [ ! -d "$STACK_DIR" ]; then
    echo "ERROR: $APP: no applied stack at $STACK_DIR to restore into" >&2
    exit 1
fi

if [ "$WHICH" = "latest" ]; then
    archive="$(ls -1 "$DESTINATION"/"$APP"-*.tar.gz 2>/dev/null | sort -r | head -1)"
    if [ -z "$archive" ]; then
        echo "ERROR: $APP: no archives in $DESTINATION" >&2
        exit 1
    fi
else
    archive="$WHICH"
fi

if [ ! -f "$archive" ]; then
    echo "ERROR: $APP: no such archive: $archive" >&2
    exit 1
fi

# Read the archive before destroying anything. Discovering it is corrupt after
# deleting the live data is the worst possible order to find out.
contents="$(tar -tzf "$archive" 2>/dev/null)"
if [ -z "$contents" ]; then
    echo "ERROR: $APP: $archive is empty or unreadable; refusing to restore" >&2
    exit 1
fi

# Only ever touch the top-level entries the archive actually carries, so a
# restore cannot reach outside the directories backup.sh captured.
tops="$(printf '%s\n' "$contents" | sed 's|^\./||; s|/.*$||' | sort -u | grep -v '^$')"
for top in $tops; do
    case "$top" in
        ..*|/*)
            echo "ERROR: $APP: archive contains an unsafe path ($top); refusing" >&2
            exit 1 ;;
    esac
done

echo "$APP: restoring from $(basename "$archive")"
echo "$APP: this replaces $(printf '%s' "$tops" | tr '\n' ' ')under $STACK_DIR"

if ! (cd "$STACK_DIR" && docker compose stop > /dev/null 2>&1); then
    echo "ERROR: $APP: could not stop the stack; not restoring under a running one" >&2
    exit 1
fi

restore_failed=""
for top in $tops; do
    rm -rf "${STACK_DIR:?}/$top"
done
if ! tar -xzf "$archive" -C "$STACK_DIR" 2>/dev/null; then
    echo "ERROR: $APP: unpacking failed; the stack's data is now incomplete" >&2
    restore_failed=1
fi

if ! (cd "$STACK_DIR" && docker compose start > /dev/null 2>&1); then
    echo "ERROR: $APP: restored but the stack did not start" >&2
    exit 1
fi

[ -n "$restore_failed" ] && exit 1
echo "$APP: restored and running"
