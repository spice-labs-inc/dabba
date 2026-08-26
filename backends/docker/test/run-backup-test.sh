#!/usr/bin/env bash
#
# Backup and RESTORE tests for backup.sh / restore.sh.
#
# The restore is the point. An archive nobody has ever unpacked is a hypothesis,
# and "we have backups" is a claim about restores. So the central test writes a
# known value into a converged stack, backs it up, destroys the data, restores,
# and asserts the value came back — through the same scripts a scheduled job runs
# on a real box, not a simulation of them.
#
# Also covered, because each is a way for backups to be quietly worthless:
#
#   1. the archive is verified readable before it counts as a backup;
#   2. an interrupted run leaves no file that looks like a usable backup;
#   3. `.env` is NOT archived — it holds secrets resolved from OpenBao, and the
#      reconciler rewrites it every tick;
#   4. retention keeps the newest N and prunes the rest;
#   5. a stack with nothing to back up FAILS rather than silently writing an
#      empty archive, because that is what makes the job unit's OnFailure= fire;
#   6. restore refuses a corrupt archive BEFORE deleting the live data.
#
# Needs a running Docker daemon. Fully self-cleaning. Portable bash 3.2.
set -uo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BACKUP_SH="$BACKEND_DIR/backup.sh"
RESTORE_SH="$BACKEND_DIR/restore.sh"

APP="dabbabackup"
PROJECT="gitops-$APP"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-backup-test.XXXXXX")"
export STACKS_DIR="$SCRATCH/stacks"
export BACKUPS_DIR="$SCRATCH/backups"
STACK_DIR="$STACKS_DIR/$APP"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }
have_docker() { docker info > /dev/null 2>&1; }

cleanup() {
    echo
    echo "--- cleanup ---"
    if have_docker; then
        ids="$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
        if [ -n "$ids" ]; then
            # shellcheck disable=SC2086  # deliberate word splitting over ids
            docker rm -f $ids > /dev/null 2>&1
        fi
        docker network rm "${PROJECT}_default" > /dev/null 2>&1
    fi
    rm -rf "$SCRATCH"
    # Prove it rather than trusting the removals above.
    if have_docker; then
        left="$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null | wc -l | tr -d ' ')"
        [ "$left" = "0" ] || echo "  WARN: $left container(s) left behind"
    fi
    [ -d "$SCRATCH" ] && echo "  WARN: $SCRATCH left behind"
    echo "  cleaned up"
}
trap cleanup EXIT

if ! have_docker; then
    echo "SKIP: these tests need a running Docker daemon"
    exit 0
fi

echo "=== backup and restore ==="

# A stack whose only state is a bind-mounted file, so "did the data come back"
# is a question with an exact answer.
mkdir -p "$STACK_DIR/data"
cat > "$STACK_DIR/docker-compose.yml" <<YAML
services:
  keeper:
    image: busybox
    command: ["sh", "-c", "sleep 100000"]
    volumes:
      - type: bind
        source: ./data
        target: /data
YAML
printf 'COMPOSE_PROJECT_NAME=%s\n' "$PROJECT" > "$STACK_DIR/.env"
# A secret-shaped line, to prove .env is excluded rather than merely absent.
printf '# dabba-secret: resolved from OpenBao; do not edit or commit\nAPI_TOKEN=super-secret-value\n' >> "$STACK_DIR/.env"
printf 'the-original-value\n' > "$STACK_DIR/data/payload"

if ! (cd "$STACK_DIR" && docker compose up -d > /dev/null 2>&1); then
    fail "could not start the fixture stack"
    exit 1
fi

# --- 1. a backup is produced, and verified readable -------------------------
if bash "$BACKUP_SH" "$APP" > "$SCRATCH/backup.log" 2>&1; then
    archive="$(ls -1 "$BACKUPS_DIR/$APP"/*.tar.gz 2>/dev/null | head -1)"
    if [ -n "$archive" ] && tar -tzf "$archive" > /dev/null 2>&1; then
        pass "backup wrote a readable archive"
    else
        fail "backup reported success but produced no readable archive"
    fi
else
    fail "backup failed: $(cat "$SCRATCH/backup.log")"
    archive=""
fi

# --- 2. nothing partial is left behind --------------------------------------
if [ -z "$(ls -1 "$BACKUPS_DIR/$APP"/*.partial 2>/dev/null)" ]; then
    pass "no partial archive left behind"
else
    fail "a .partial file survived, and would look like a backup"
fi

# --- 3. the resolved secrets are NOT in the archive -------------------------
if [ -n "$archive" ]; then
    if tar -tzf "$archive" 2>/dev/null | grep -q '\.env'; then
        fail ".env was archived, copying live secrets into a tarball that outlives them"
    elif tar -xzOf "$archive" 2>/dev/null | grep -q 'super-secret-value'; then
        fail "a resolved secret value reached the archive"
    else
        pass "the archive carries data, not resolved secrets"
    fi
fi

# --- 4. THE RESTORE: destroy the data, bring it back ------------------------
rm -rf "$STACK_DIR/data"
if [ -f "$STACK_DIR/data/payload" ]; then
    fail "could not destroy the data, so the restore would prove nothing"
else
    if bash "$RESTORE_SH" "$APP" latest > "$SCRATCH/restore.log" 2>&1; then
        if [ "$(cat "$STACK_DIR/data/payload" 2>/dev/null)" = "the-original-value" ]; then
            pass "restore brought the exact value back"
        else
            fail "restore ran but the value did not come back: $(cat "$SCRATCH/restore.log")"
        fi
    else
        fail "restore failed: $(cat "$SCRATCH/restore.log")"
    fi
fi

# --- 5. the stack is running again after a restore --------------------------
running="$(docker ps -q --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null | wc -l | tr -d ' ')"
if [ "$running" != "0" ]; then
    pass "the stack is running again after the restore"
else
    fail "the restore left the stack stopped"
fi

# --- 6. retention keeps the newest and prunes the rest ----------------------
# Stamps are one-second resolution, so make distinct ones rather than racing.
for n in 1 2 3 4; do
    printf 'value-%s\n' "$n" > "$STACK_DIR/data/payload"
    BACKUP_KEEP=2 bash "$BACKUP_SH" "$APP" > /dev/null 2>&1
    sleep 1
done
kept="$(ls -1 "$BACKUPS_DIR/$APP"/*.tar.gz 2>/dev/null | wc -l | tr -d ' ')"
if [ "$kept" = "2" ]; then
    pass "retention kept exactly BACKUP_KEEP archives"
else
    fail "retention kept $kept archives, expected 2"
fi
# And the ones it kept are the NEWEST, not any two.
newest="$(ls -1 "$BACKUPS_DIR/$APP"/*.tar.gz | sort -r | head -1)"
if tar -xzOf "$newest" 2>/dev/null | grep -q 'value-4'; then
    pass "the archives it kept are the newest ones"
else
    fail "retention pruned the wrong end"
fi

# --- 7. a stack with nothing to back up FAILS -------------------------------
# This is what makes a scheduled job's OnFailure= fire. Exiting 0 here would turn
# a misconfigured stack into a backup that silently never contains anything.
mkdir -p "$STACKS_DIR/dabbanodata"
cat > "$STACKS_DIR/dabbanodata/docker-compose.yml" <<'YAML'
services:
  stateless:
    image: busybox
    command: ["true"]
YAML
if bash "$BACKUP_SH" dabbanodata > "$SCRATCH/nodata.log" 2>&1; then
    fail "a stack with no bind-mounted data reported a successful backup"
else
    if grep -q 'nothing to back' "$SCRATCH/nodata.log"; then
        pass "a stack with nothing to back up fails, and says why"
    else
        fail "it failed, but not for the stated reason: $(cat "$SCRATCH/nodata.log")"
    fi
fi

# --- 8. restore refuses a corrupt archive BEFORE deleting anything ----------
printf 'not a tarball\n' > "$BACKUPS_DIR/$APP/$APP-99999999T999999Z.tar.gz"
printf 'still-here\n' > "$STACK_DIR/data/payload"
if bash "$RESTORE_SH" "$APP" "$BACKUPS_DIR/$APP/$APP-99999999T999999Z.tar.gz" > /dev/null 2>&1; then
    fail "restore accepted a corrupt archive"
elif [ "$(cat "$STACK_DIR/data/payload" 2>/dev/null)" = "still-here" ]; then
    pass "restore refused the corrupt archive and left the live data alone"
else
    fail "restore refused the archive but destroyed the data first"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "=== all backup/restore tests passed ==="
    exit 0
fi
echo "=== $fails backup/restore test(s) FAILED ==="
exit 1
