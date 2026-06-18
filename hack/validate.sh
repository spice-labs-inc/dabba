#!/usr/bin/env bash
# Static validation across the three dabba repos — no cluster needed.
# Renders every kustomize overlay/base (skipping Components, which aren't
# standalone-buildable) and checks HCL formatting. This is the cheap, broad
# layer of the test matrix: it catches structural breakage fast, but not
# runtime/ordering bugs (those need the e2e harness in hack/local-test/).
set -uo pipefail
DEV="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fail=0
pass() { echo "  ✓ $*"; }
bad()  { echo "  ✗ $*"; fail=1; }

# dabba is an OpenTofu project; prefer tofu, fall back to terraform (same HCL).
TF="$(command -v tofu || command -v terraform || true)"
[ -n "$TF" ] || { echo "neither tofu nor terraform found"; exit 1; }

echo "== HCL formatting ($(basename "$TF")) =="
for repo in dabba-modules dabba; do
  if "$TF" fmt -check -recursive "$DEV/$repo" >/dev/null 2>&1; then
    pass "$repo fmt"
  else
    bad "$repo fmt — run: $TF fmt -recursive $DEV/$repo"
  fi
done

echo "== kustomize render (every overlay/base; components skipped) =="
while IFS= read -r kfile; do
  dir="$(dirname "$kfile")"
  rel="${dir#"$DEV"/}"
  # Components have kind: Component and cannot be built standalone.
  if grep -qE '^kind:\s*Component' "$kfile"; then continue; fi
  if kubectl kustomize "$dir" >/dev/null 2>&1; then
    pass "$rel"
  else
    bad "$rel"
    kubectl kustomize "$dir" 2>&1 | sed 's/^/      /' | head -5
  fi
done < <(find "$DEV/dabba-gitops" -name 'kustomization.y*ml' | sort)

echo
if [ "$fail" -eq 0 ]; then echo "ALL STATIC CHECKS PASSED"; else echo "STATIC CHECKS FAILED"; fi
exit $fail
