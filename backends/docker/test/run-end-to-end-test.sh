#!/usr/bin/env bash
#
# End-to-end test for the docker-host substrate, at parity with what
# hack/local-test/in-vm.sh asserts for kind/k3d/minikube.
#
# The Kubernetes end-to-end proves one thing above all: an application serves a
# value that came out of the secret store. That is the contract this mirrors —
# not "up returned zero", but "a real request returned a real secret".
#
# The arc:
#   1. `dabba env <name> up` against a local gitops seed, from a directory with
#      NO backends/docker/ in it, so the embedded reconciler is what runs;
#   2. OpenBao converges, and dabba initialises and unseals it;
#   3. a secret is written, and an application whose compose file was RENDERED
#      from a portable Application definition picks it up by reference;
#   4. the application serves that secret over HTTP;
#   5. the reconcile loop converges a change without anyone touching the box;
#   6. `dabba env <name> down` stops the loop and LEAVES THE STACKS RUNNING.
#
# Step 6 is where this deliberately diverges from the Kubernetes test. There,
# `down` destroys the cluster and the test asserts it is gone. Here the
# never-destroy contract means the correct assertion is the opposite: the loop
# is gone AND the stacks are still up. Asserting "down worked" without checking
# which of those two happened would pass for entirely the wrong reason.
#
# Self-cleaning: every container, LaunchAgent, systemd unit and directory this
# creates is removed on exit, and the removal is verified rather than assumed.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DABBA="${DABBA:-$REPO_ROOT/target/release/dabba}"

ENVIRONMENT="dabbae2e"
BOX="dabbae2ebox"
APP="secretconsumer"
APP_PROJECT="gitops-$APP"
OPENBAO_PROJECT="gitops-openbao"
CACHE_PROJECT="gitops-cache"
MC_IMAGE="minio/mc:RELEASE.2024-10-08T09-37-26Z"
PORT="18093"
SECRET_VALUE="delivered through dabba"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/dabba-e2e.XXXXXX")"
# Deliberately NOT the repo: `up` must find the reconciler inside the binary.
WORKROOT="$SCRATCH/elsewhere"
CONFIG="$WORKROOT/dabba.yaml"
SEED="$SCRATCH/gitops-seed"
WORKDIR="$WORKROOT/.dabba/$ENVIRONMENT"

fails=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; fails=$((fails + 1)); }
step() { echo; echo "=== $* ==="; }

cleanup() {
    step "cleanup"
    if [ -f "$CONFIG" ] && [ -x "$DABBA" ]; then
        "$DABBA" env "$ENVIRONMENT" down -c "$CONFIG" > /dev/null 2>&1
    fi
    # The loop again, directly, in case `down` itself is what broke.
    if [ "$(uname -s)" = "Darwin" ]; then
        launchctl bootout "gui/$(id -u)/io.spicelabs.dabba.reconcile.$ENVIRONMENT" > /dev/null 2>&1
        rm -f "$HOME/Library/LaunchAgents/io.spicelabs.dabba.reconcile.$ENVIRONMENT.plist"
        rm -f "$HOME/Library/Logs/dabba/reconcile-$ENVIRONMENT.log"
    else
        systemctl --user disable --now "gitops-reconcile-$ENVIRONMENT.timer" > /dev/null 2>&1
        rm -f "$HOME/.config/systemd/user/gitops-reconcile-$ENVIRONMENT."*
        systemctl --user daemon-reload > /dev/null 2>&1
    fi

    for project in "$APP_PROJECT" "$OPENBAO_PROJECT" "$CACHE_PROJECT"; do
        ids="$(docker ps -aq --filter "label=com.docker.compose.project=$project" 2>/dev/null)"
        if [ -n "$ids" ]; then
            # shellcheck disable=SC2086  # deliberate word splitting over ids
            docker rm -f $ids > /dev/null 2>&1
        fi
        docker network rm "${project}_default" > /dev/null 2>&1
    done
    rm -rf "$SCRATCH"

    # Prove it, rather than trusting the removals above.
    residue=""
    for project in "$APP_PROJECT" "$OPENBAO_PROJECT" "$CACHE_PROJECT"; do
        [ -n "$(docker ps -aq --filter "label=com.docker.compose.project=$project" 2>/dev/null)" ] \
            && residue="$residue $project"
    done
    if [ "$(uname -s)" = "Darwin" ] && launchctl print "gui/$(id -u)/io.spicelabs.dabba.reconcile.$ENVIRONMENT" > /dev/null 2>&1; then
        residue="$residue launchagent"
    fi
    [ -d "$SCRATCH" ] && residue="$residue scratch-dir"
    if [ -n "$residue" ]; then
        echo "  WARNING: residue survived cleanup:$residue"
    else
        echo "  nothing left behind: no containers, no scheduler unit, no directories"
    fi
}
trap cleanup EXIT

if ! docker info > /dev/null 2>&1; then
    echo "SKIP: needs a running docker daemon"
    exit 0
fi
if [ ! -x "$DABBA" ]; then
    echo "SKIP: no dabba binary at $DABBA (cargo build --release first)"
    exit 0
fi

###############################################################################
step "1. a gitops seed: OpenBao, plus an application RENDERED from a definition"
###############################################################################
mkdir -p "$WORKROOT" "$SEED/apps/$BOX/openbao" "$SEED/apps/$BOX/$APP"

cp "$REPO_ROOT/backends/docker/examples/openbao/docker-compose.yml" \
   "$SEED/apps/$BOX/openbao/docker-compose.yml"

# The application is written ONCE, portably, and rendered for this substrate.
# That is the whole claim: the same definition would render for Kubernetes.
cat > "$SCRATCH/application.yaml" <<YAML
apiVersion: dabba.spicelabs.io/v1alpha1
kind: Application
metadata:
  name: $APP
spec:
  image: hashicorp/http-echo
  tag: "1.0"
  ports:
    - name: http
      containerPort: 5678
      publish: $PORT
  environment:
    - name: ECHO_TEXT
      secret:
        name: demo
        key: message
  dockerHost:
    # http-echo takes its body as a flag; this is what the escape hatch is for.
    command: ["-listen=:5678", "-text=\$ECHO_TEXT"]
YAML

"$DABBA" application validate "$SCRATCH/application.yaml" || fail "definition did not validate"
"$DABBA" application render "$SCRATCH/application.yaml" --substrate docker-host \
    > "$SEED/apps/$BOX/$APP/docker-compose.yml" \
    || fail "rendering for docker-host failed"

grep -q 'x-secrets' "$SEED/apps/$BOX/$APP/docker-compose.yml" \
    && pass "rendered stack carries a secret reference" \
    || fail "rendered stack has no x-secrets block"
grep -q "$SECRET_VALUE" "$SEED/apps/$BOX/$APP/docker-compose.yml" \
    && fail "THE SECRET VALUE IS IN A FILE HEADED FOR GIT" \
    || pass "rendered stack contains the reference, not the value"

# The same definition must render for Kubernetes too, or portability is a claim.
"$DABBA" application render "$SCRATCH/application.yaml" --substrate kind \
    > "$SCRATCH/kubernetes.yaml" 2>/dev/null \
    && grep -q 'kind: Deployment' "$SCRATCH/kubernetes.yaml" \
    && pass "the same definition also renders for Kubernetes" \
    || fail "the same definition did not render for Kubernetes"

git init -q "$SEED"
git -C "$SEED" config user.email dabba-e2e@localhost
git -C "$SEED" config user.name "dabba e2e"
git -C "$SEED" checkout -q -b main
git -C "$SEED" add -A
git -C "$SEED" commit -q -m "openbao + $APP"

cat > "$CONFIG" <<YAML
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
metadata:
  name: dabba
spec:
  domain: localtest.me
  defaultEnvironment: $ENVIRONMENT
  environments:
    - name: $ENVIRONMENT
      substrate: docker-host
      substrateConfig:
        boxName: $BOX
YAML

###############################################################################
step "2. up, from a directory with no backends/docker/ in it"
###############################################################################
( cd "$WORKROOT" && "$DABBA" env "$ENVIRONMENT" up -c "$CONFIG" --gitops-seed "$SEED" ) 2>&1 \
    | sed 's/^/  | /'

[ -f "$WORKDIR/reconciler/reconcile.sh" ] \
    && pass "the reconciler was materialised from the binary" \
    || fail "no materialised reconciler — an installed dabba could not have run"

###############################################################################
step "3. OpenBao is initialised and unsealed"
###############################################################################
[ -s "$WORKDIR/openbao-root" ] && pass "a root token was stashed" \
    || fail "no root token stashed"
[ -s "$WORKDIR/openbao-unseal" ] && pass "an unseal key was stashed" \
    || fail "no unseal key stashed"
if [ "$(stat -f '%Lp' "$WORKDIR/openbao-root" 2>/dev/null || stat -c '%a' "$WORKDIR/openbao-root" 2>/dev/null)" = "600" ]; then
    pass "the root token is 0600"
else
    fail "the root token is not 0600"
fi

bao_container="$(docker ps -q --filter "label=com.docker.compose.project=$OPENBAO_PROJECT" --filter status=running | head -1)"
if [ -n "$bao_container" ]; then
    sealed="$(docker exec "$bao_container" sh -c 'BAO_ADDR=http://127.0.0.1:8200 bao status -format=json' 2>/dev/null | grep -o '"sealed": *[a-z]*' | head -1)"
    echo "$sealed" | grep -q 'false' && pass "OpenBao reports itself unsealed" \
        || fail "OpenBao is still sealed ($sealed)"
else
    fail "no OpenBao container is running"
fi

###############################################################################
step "4. write a secret, let the loop deliver it, and read it back over HTTP"
###############################################################################
if [ -n "$bao_container" ]; then
    docker exec -i "$bao_container" sh -c \
        "read -r T; BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN=\$T bao kv put secret/demo message='$SECRET_VALUE'" \
        < "$WORKDIR/openbao-root" > /dev/null 2>&1 \
        && pass "secret written to OpenBao" || fail "could not write the secret"
fi

# Drive a reconcile directly rather than waiting on the minute timer.
GITOPS_DIR="$WORKDIR/gitops" STACKS_DIR="$WORKDIR/stacks" BOX_NAME="$BOX" \
  DABBA_ENVIRONMENT="$ENVIRONMENT" OPENBAO_TOKEN_FILE="$WORKDIR/openbao-root" \
  /bin/bash "$WORKDIR/reconciler/reconcile.sh" 2>&1 | sed 's/^/  | /'

if [ -f "$WORKDIR/stacks/$APP/.env" ]; then
    grep -q "ECHO_TEXT=$SECRET_VALUE" "$WORKDIR/stacks/$APP/.env" \
        && pass "the reconciler resolved the reference into the stack's .env" \
        || fail "the .env does not carry the resolved secret"
    perms="$(stat -f '%Lp' "$WORKDIR/stacks/$APP/.env" 2>/dev/null || stat -c '%a' "$WORKDIR/stacks/$APP/.env" 2>/dev/null)"
    [ "$perms" = "600" ] && pass "the resolved .env is 0600" || fail "the .env is $perms, not 0600"
else
    fail "no .env was written for $APP"
fi

# THE assertion, and the one the Kubernetes end-to-end makes too.
body=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
    body="$(curl -fsS --max-time 3 "http://127.0.0.1:${PORT}/" 2>/dev/null)"
    [ -n "$body" ] && break
    sleep 2
done
echo "  response: ${body:-<empty>}"
if echo "$body" | grep -q "$SECRET_VALUE"; then
    pass "THE APPLICATION SERVES THE VALUE THAT CAME OUT OF OPENBAO"
else
    fail "the application did not serve the OpenBao-backed value"
fi

###############################################################################
step "5. the CI cache: a bucket, two credentials, and a trust map that holds"
###############################################################################
# Render the cache stack from the SAME portable definition that ships as an
# example, so this exercises the real artifact rather than a test fixture.
mkdir -p "$SEED/apps/$BOX/cache"
"$DABBA" application render "$REPO_ROOT/examples/applications/cache-minio.yaml" \
    --substrate docker-host > "$SEED/apps/$BOX/cache/docker-compose.yml"
git -C "$SEED" add -A && git -C "$SEED" commit -q -m "add the cache stack"
git -C "$SEED" push -q origin main 2>/dev/null || true

# Drive convergence in the background so `dabba cache up` does not have to wait
# out the minutely timer for the stack it just seeded credentials for.
reconcile_forever() {
    while true; do
        GITOPS_DIR="$WORKDIR/gitops" STACKS_DIR="$WORKDIR/stacks" BOX_NAME="$BOX" \
          DABBA_ENVIRONMENT="$ENVIRONMENT" OPENBAO_TOKEN_FILE="$WORKDIR/openbao-root" \
          /bin/bash "$WORKDIR/reconciler/reconcile.sh" > /dev/null 2>&1
        sleep 10
    done
}
reconcile_forever & reconciler_pid=$!

( cd "$WORKROOT" && "$DABBA" cache up -c "$CONFIG" ) 2>&1 | tail -8 | sed 's/^/  | /'
kill "$reconciler_pid" 2>/dev/null; wait "$reconciler_pid" 2>/dev/null

read_write_env="$( cd "$WORKROOT" && "$DABBA" cache credentials -c "$CONFIG" --scope read-write 2>/dev/null )"
read_only_env="$( cd "$WORKROOT" && "$DABBA" cache credentials -c "$CONFIG" --scope read-only 2>/dev/null )"

echo "$read_write_env" | grep -q 'AWS_ACCESS_KEY_ID=' \
    && pass "read-write credentials are issued" || fail "no read-write credentials"
echo "$read_only_env" | grep -q 'SCCACHE_READONLY=1' \
    && pass "read-only credentials say so to sccache" \
    || fail "read-only credentials do not set SCCACHE_READONLY"

rw_key="$(echo "$read_write_env" | sed -n 's/^AWS_ACCESS_KEY_ID=//p')"
rw_secret="$(echo "$read_write_env" | sed -n 's/^AWS_SECRET_ACCESS_KEY=//p')"
ro_key="$(echo "$read_only_env" | sed -n 's/^AWS_ACCESS_KEY_ID=//p')"
ro_secret="$(echo "$read_only_env" | sed -n 's/^AWS_SECRET_ACCESS_KEY=//p')"

[ -n "$rw_key" ] && [ "$rw_key" != "$ro_key" ] \
    && pass "the two scopes are different accounts" \
    || fail "the scopes share an account ($rw_key / $ro_key)"

# A tiny client run on the cache network, as one scope.
as_scope() {
    docker run --rm --network "${CACHE_PROJECT}_default" \
        --env "MC_HOST_s=http://$1:$2@cache:9000" --entrypoint sh \
        "$MC_IMAGE" -c "$3" 2>&1
}

echo "  --- read-write scope ---"
echo "cache entry" > "$SCRATCH/object"
if as_scope "$rw_key" "$rw_secret" "echo 'cache entry' > /tmp/o && mc cp /tmp/o s/sccache/probe" | grep -qiE 'error|denied'; then
    fail "the read-write credential could not write"
else
    pass "read-write CAN write to the cache"
fi

echo "  --- read-only scope ---"
if as_scope "$ro_key" "$ro_secret" "mc cat s/sccache/probe" | grep -q "cache entry"; then
    pass "read-only CAN read the cache"
else
    fail "the read-only credential could not read"
fi

# THE security control. A read-only credential that can write means an untrusted
# pull request can poison what a later trusted build links into a release.
write_attempt="$(as_scope "$ro_key" "$ro_secret" "echo poison > /tmp/p && mc cp /tmp/p s/sccache/poisoned")"
if echo "$write_attempt" | grep -qiE 'denied|forbidden|error'; then
    pass "read-only CANNOT write (the trust map holds)"
else
    fail "READ-ONLY CREDENTIAL WAS ABLE TO WRITE — the cache can be poisoned"
fi
# And prove the refusal was real, not a client-side no-op.
if as_scope "$rw_key" "$rw_secret" "mc ls s/sccache/poisoned" | grep -q 'poisoned'; then
    fail "the poisoned object EXISTS despite the write appearing to fail"
else
    pass "no poisoned object exists in the bucket"
fi

# Deleting is a write too, and a cache you can empty is a cache you can degrade.
delete_attempt="$(as_scope "$ro_key" "$ro_secret" "mc rm s/sccache/probe")"
if echo "$delete_attempt" | grep -qiE 'denied|forbidden|error'; then
    pass "read-only CANNOT delete"
else
    fail "READ-ONLY CREDENTIAL WAS ABLE TO DELETE"
fi

###############################################################################
step "6. down stops the loop and LEAVES THE STACKS RUNNING"
###############################################################################
( cd "$WORKROOT" && "$DABBA" env "$ENVIRONMENT" down -c "$CONFIG" ) 2>&1 | sed 's/^/  | /'

if [ "$(uname -s)" = "Darwin" ]; then
    if launchctl print "gui/$(id -u)/io.spicelabs.dabba.reconcile.$ENVIRONMENT" > /dev/null 2>&1; then
        fail "the reconcile loop is still installed after down"
    else
        pass "the reconcile loop was removed"
    fi
fi

# Distinguish "down destroyed it" from "it never started": both leave nothing
# running, and reporting a broken never-destroy contract for a stack that never
# came up would send the next person hunting the wrong bug.
still_running="$(docker ps -q --filter "label=com.docker.compose.project=$APP_PROJECT" --filter status=running | head -1)"
ever_existed="$(docker ps -aq --filter "label=com.docker.compose.project=$APP_PROJECT" | head -1)"
if [ -n "$still_running" ]; then
    pass "the stack is STILL RUNNING (never-destroy contract honoured)"
elif [ -z "$ever_existed" ]; then
    fail "the stack never started, so the never-destroy contract was not exercised"
else
    fail "down tore the stack down; the never-destroy contract was broken"
fi

###############################################################################
echo
if [ "$fails" -eq 0 ]; then
    echo "=== ALL ASSERTIONS PASSED ==="
else
    echo "=== $fails ASSERTION(S) FAILED ==="
fi
exit "$fails"
