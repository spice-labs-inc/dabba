#!/usr/bin/env bash
# Host-side driver: runs the full dabba quickstart in a multipass VM using the
# local copies of all three repos — no GitHub, no pushing.
#
# Usage:
#   ./run.sh [--substrate kind|k3d|minikube] [--keep|--reuse]
#     --substrate   local cluster to provision (default kind). Each gets its own VM.
#     --keep        leave the VM running afterwards
#     --reuse       reuse the existing VM (warm cache; faster). Implies --keep.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV="$(cd "$HERE/../../.." && pwd)"   # ~/dev (parent of dabba/)
SUBSTRATE=kind
MODE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --substrate) SUBSTRATE="$2"; shift 2 ;;
    --keep | --reuse) MODE="$1"; shift ;;
    *) echo "unknown arg: $1"; exit 1 ;;
  esac
done
VM="dabba-test-$SUBSTRATE"
# The multipass snap has a private /tmp and its home interface excludes hidden
# files, so stage the bundle as a non-hidden file under $HOME.
BUNDLE="$HOME/dabba-bundle.tgz"
trap 'rm -f "$BUNDLE"' EXIT

echo "▸ [$SUBSTRATE] bundling repos from $DEV"
tar czf "$BUNDLE" -C "$DEV" \
  --exclude='.git' --exclude='.terraform' --exclude='*.tfstate*' --exclude='kubeconfig' --exclude='target' \
  dabba dabba-modules dabba-gitops

if [ "$MODE" = "--reuse" ] && multipass info "$VM" >/dev/null 2>&1; then
  echo "▸ reusing existing VM $VM (warm cache)"
  multipass start "$VM" >/dev/null 2>&1 || true
else
  echo "▸ (re)launching VM $VM"
  multipass delete --purge "$VM" 2>/dev/null || true
  multipass launch 24.04 --name "$VM" --cpus 4 --memory 8G --disk 40G \
    --cloud-init "$HERE/cloud-init.yaml"
  multipass exec "$VM" -- cloud-init status --wait
fi

echo "▸ transferring bundle"
multipass transfer "$BUNDLE" "$VM":/home/ubuntu/dabba-bundle.tgz
multipass exec "$VM" -- bash -c 'cd /home/ubuntu && tar xzf dabba-bundle.tgz'

echo "▸ running the quickstart in the VM ($SUBSTRATE)"
set +e
multipass exec "$VM" -- sg docker -c "SUBSTRATE=$SUBSTRATE bash /home/ubuntu/dabba/hack/local-test/in-vm.sh"
rc=$?
set -e

echo
if [ $rc -eq 0 ]; then echo "✓ [$SUBSTRATE] local test PASSED"; else echo "✗ [$SUBSTRATE] local test FAILED (rc=$rc)"; fi
if [ "$MODE" = "--keep" ] || [ "$MODE" = "--reuse" ]; then
  echo "VM kept. Reuse (fast):  $0 --substrate $SUBSTRATE --reuse"
  echo "Delete:                 multipass delete --purge $VM"
else
  multipass delete --purge "$VM" 2>/dev/null || true
fi
exit $rc
