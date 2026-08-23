#!/usr/bin/env bash
#
# On-host smoke test for reconcile.sh. Runs a real reconcile against a
# throwaway gitops git repo containing one app that runs a long-lived
# nginx:alpine published on 127.0.0.1. Asserts, in order:
#
#   1. first reconcile brings the container up as project gitops-<app>,
#      pre-creates its ./-relative bind mount, and passes the x-health-cmd gate;
#   2. a second reconcile is a clean no-op (no redeploy, same container id);
#   3. changing the desired compose triggers exactly one redeploy (new id, new
#      label visible).
#
# Everything lives in a temp dir and the container is torn down on exit, so the
# test is side-effect-free apart from pulling nginx:alpine. Portable bash 3.2.
set -uo pipefail

BACKEND_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RECONCILE_SH="$BACKEND_DIR/reconcile.sh"

APP="dabbatest"
BOX="testbox"
PROJECT="gitops-$APP"
PORT="18091"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-recon-test.XXXXXX")"
ORIGIN="$SCRATCH/origin.git"
GITOPS_DIR="$SCRATCH/gitops"
STACKS_DIR="$SCRATCH/stacks"
APP_DIR="$GITOPS_DIR/apps/$BOX/$APP"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }

cleanup() {
    echo "--- cleanup ---"
    if [ -d "$STACKS_DIR/$APP" ]; then
        ( cd "$STACKS_DIR/$APP" && docker compose down -v --remove-orphans > /dev/null 2>&1 ) || true
    fi
    # belt-and-suspenders in case .env/compose was mid-write
    ids="$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
    [ -n "$ids" ] && docker rm -f $ids > /dev/null 2>&1
    docker network rm "${PROJECT}_default" > /dev/null 2>&1 || true
    rm -rf "$SCRATCH"
    echo "  removed $SCRATCH and container/network for $PROJECT"
}
trap cleanup EXIT

running_id() {
    docker ps -q --filter "label=com.docker.compose.project=$PROJECT" \
               --filter "status=running" 2>/dev/null | head -1
}

echo "=== build throwaway gitops repo ==="
git init -q --bare "$ORIGIN"
git clone -q "$ORIGIN" "$GITOPS_DIR"
git -C "$GITOPS_DIR" config user.email dabba-test@localhost
git -C "$GITOPS_DIR" config user.name "dabba test"
git -C "$GITOPS_DIR" checkout -q -b main
mkdir -p "$APP_DIR"

# Legacy full-file stack (copied verbatim by the reconciler). Exercises:
#   - a ./-relative bind mount (portable extractor must pre-create ./data)
#   - loopback-only publish (127.0.0.1)
#   - a top-level x-health-cmd gate
cat > "$APP_DIR/docker-compose.yml" <<YAML
services:
  web:
    image: nginx:alpine
    ports:
      - "127.0.0.1:${PORT}:80"
    volumes:
      - ./data:/data:rw
    restart: unless-stopped
x-health-cmd: curl -fsS http://127.0.0.1:${PORT}/ >/dev/null 2>&1
YAML
git -C "$GITOPS_DIR" add -A
git -C "$GITOPS_DIR" commit -q -m "add $APP"
git -C "$GITOPS_DIR" push -q -u origin main

run_reconcile() {
    GITOPS_DIR="$GITOPS_DIR" STACKS_DIR="$STACKS_DIR" BOX_NAME="$BOX" \
        /bin/bash "$RECONCILE_SH" 2>&1
}

echo
echo "=== reconcile #1 (expect deploy) ==="
out1="$(run_reconcile)"; echo "$out1" | sed 's/^/  | /'
id1="$(running_id)"

echo "$out1" | grep -q "$APP: desired state changed; deploying" \
    && pass "reconcile reported a deploy" || fail "no deploy reported"
echo "$out1" | grep -q "$APP: converged and healthy" \
    && pass "x-health-cmd gate passed" || fail "health gate did not pass"
[ -n "$id1" ] && pass "container running as project $PROJECT (id ${id1})" \
    || fail "no running container for project $PROJECT"
[ -d "$STACKS_DIR/$APP/data" ] && pass "./data bind mount pre-created" \
    || fail "./data bind mount was not pre-created"
grep -q "^COMPOSE_PROJECT_NAME=$PROJECT\$" "$STACKS_DIR/$APP/.env" 2>/dev/null \
    && pass ".env pins COMPOSE_PROJECT_NAME=$PROJECT" || fail ".env missing project pin"
if curl -fsS "http://127.0.0.1:${PORT}/" > /dev/null 2>&1; then
    pass "nginx answers on 127.0.0.1:${PORT}"
else
    fail "nginx did not answer on 127.0.0.1:${PORT}"
fi

echo
echo "=== reconcile #2 (expect clean no-op) ==="
out2="$(run_reconcile)"; echo "$out2" | sed 's/^/  | /'
id2="$(running_id)"

if echo "$out2" | grep -q "deploying"; then
    fail "second run redeployed (should have been a no-op)"
else
    pass "second run did not redeploy"
fi
[ -n "$id2" ] && [ "$id1" = "$id2" ] \
    && pass "same container id across runs (${id2})" \
    || fail "container id changed on no-op run ($id1 -> $id2)"

echo
echo "=== change desired state, reconcile #3 (expect redeploy) ==="
cat > "$APP_DIR/docker-compose.yml" <<YAML
services:
  web:
    image: nginx:alpine
    labels:
      - "dabba.test=v2"
    ports:
      - "127.0.0.1:${PORT}:80"
    volumes:
      - ./data:/data:rw
    restart: unless-stopped
x-health-cmd: curl -fsS http://127.0.0.1:${PORT}/ >/dev/null 2>&1
YAML
git -C "$GITOPS_DIR" commit -q -am "change $APP: add label"
git -C "$GITOPS_DIR" push -q origin main

out3="$(run_reconcile)"; echo "$out3" | sed 's/^/  | /'
id3="$(running_id)"

echo "$out3" | grep -q "$APP: desired state changed; deploying" \
    && pass "change triggered a redeploy" || fail "change did not redeploy"
[ -n "$id3" ] && [ "$id3" != "$id2" ] \
    && pass "container recreated with new id (${id2} -> ${id3})" \
    || fail "container id did not change on redeploy"
label="$(docker inspect -f '{{ index .Config.Labels "dabba.test" }}' "$id3" 2>/dev/null)"
[ "$label" = "v2" ] && pass "new label dabba.test=v2 present on container" \
    || fail "new label not found on redeployed container (got '${label}')"

echo
if [ "$fails" -eq 0 ]; then
    echo "=== ALL ASSERTIONS PASSED ==="
else
    echo "=== $fails ASSERTION(S) FAILED ==="
fi
exit "$fails"
