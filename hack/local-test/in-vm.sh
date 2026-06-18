#!/usr/bin/env bash
# Runs INSIDE the multipass VM: installs tooling, builds the dabba CLI, and runs
# `dabba up` for one environment (named after the substrate) seeding Forgejo from
# the local dabba-gitops — the whole platform with no GitHub at all — then asserts
# the demo app serves the OpenBao-seeded message and runs `dabba down`.
#
# SUBSTRATE (env, default kind) selects the local cluster: kind | k3d | minikube.
# The configure layer and gitops are identical across substrates — only the
# provisioning module changes. That is the kubeconfig seam, proven.
set -euo pipefail
H=/home/ubuntu
SUBSTRATE="${SUBSTRATE:-kind}"
ENVNAME="$SUBSTRATE"
WORK="$H/.dabba/$ENVNAME"
KC="$WORK/kubeconfig"
step() { echo; echo "############## [$SUBSTRATE] $* ##############"; }

step "Installing tooling (kubectl, opentofu, flux, rust, $SUBSTRATE)"
if ! command -v kubectl >/dev/null; then
  KVER=$(curl -fsSL https://dl.k8s.io/release/stable.txt)
  sudo curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${KVER}/bin/linux/amd64/kubectl"
  sudo chmod +x /usr/local/bin/kubectl
fi
command -v tofu >/dev/null || { curl -fsSL https://get.opentofu.org/install-opentofu.sh -o /tmp/it.sh; sudo bash /tmp/it.sh --install-method deb; }
command -v flux >/dev/null || { curl -fsSL https://fluxcd.io/install.sh | sudo bash; }
command -v cc >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y build-essential; }
command -v cargo >/dev/null || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
export PATH="$H/.cargo/bin:$PATH"
case "$SUBSTRATE" in
  kind)
    command -v kind >/dev/null || {
      KIND_VER=$(curl -s https://api.github.com/repos/kubernetes-sigs/kind/releases/latest | jq -r .tag_name)
      sudo curl -fsSLo /usr/local/bin/kind "https://kind.sigs.k8s.io/dl/${KIND_VER}/kind-linux-amd64"; sudo chmod +x /usr/local/bin/kind; } ;;
  k3d)
    command -v k3d >/dev/null || curl -s https://raw.githubusercontent.com/k3d-io/k3d/main/install.sh | bash ;;
  minikube)
    command -v minikube >/dev/null || {
      sudo curl -fsSLo /usr/local/bin/minikube https://storage.googleapis.com/minikube/releases/latest/minikube-linux-amd64; sudo chmod +x /usr/local/bin/minikube; } ;;
  *) echo "unknown substrate: $SUBSTRATE"; exit 1 ;;
esac

step "Building the dabba CLI"
cargo build --release --manifest-path "$H/dabba/Cargo.toml"
DABBA="$H/dabba/target/release/dabba"

# Pull-through image cache (kind only: wired via the kind module's registry_mirrors
# -> containerd). The CLI runs tofu as a child, so it inherits TF_VAR_registry_mirrors.
if [ "$SUBSTRATE" = "kind" ]; then
  step "Pull-through image cache"
  docker network create kind >/dev/null 2>&1 || true
  declare -A UPSTREAMS=(
    [dockerio]="https://registry-1.docker.io|docker.io" [ghcr]="https://ghcr.io|ghcr.io"
    [quay]="https://quay.io|quay.io" [forgejo]="https://code.forgejo.org|code.forgejo.org"
    [k8s]="https://registry.k8s.io|registry.k8s.io"
  )
  mirrors=""
  for name in "${!UPSTREAMS[@]}"; do
    url="${UPSTREAMS[$name]%%|*}"; host="${UPSTREAMS[$name]##*|}"; cname="dabba-cache-$name"
    if ! docker ps -a --format '{{.Names}}' | grep -qx "$cname"; then
      docker run -d --name "$cname" --restart=always --network kind \
        -v "$cname:/var/lib/registry" -e REGISTRY_PROXY_REMOTEURL="$url" registry:2 >/dev/null
    else docker start "$cname" >/dev/null 2>&1 || true; fi
    mirrors+="\"$host\":\"http://$cname:5000\","
  done
  export TF_VAR_registry_mirrors="{${mirrors%,}}"
fi

step "Writing a local DabbaConfig (one env: $ENVNAME)"
cat > "$H/dabba.yaml" <<EOF
apiVersion: dabba.spicelabs.io/v1alpha1
kind: DabbaConfig
metadata:
  name: dabba
spec:
  domain: localtest.me
  tls:
    issuer: selfsigned
  defaultEnvironment: $ENVNAME
  environments:
    - { name: $ENVNAME, substrate: $SUBSTRATE }
  useCases:
    - demo
EOF
"$DABBA" config validate "$H/dabba.yaml"

step "Cleaning any prior cluster + per-env state (repeatable; cache persists)"
case "$SUBSTRATE" in
  kind)     kind delete cluster --name "$ENVNAME" >/dev/null 2>&1 || true ;;
  k3d)      k3d cluster delete "$ENVNAME" >/dev/null 2>&1 || true ;;
  minikube) minikube delete -p "$ENVNAME" >/dev/null 2>&1 || true ;;
esac
rm -rf "$WORK"

step "dabba up (Forgejo seeded from local dabba-gitops)"
if ! "$DABBA" up -c "$H/dabba.yaml" \
  --quickstart-dir "$H/dabba/quickstart" \
  --modules-source "$H/dabba-modules" \
  --gitops-seed "$H/dabba-gitops"; then
  step "FAILED — diagnostics"
  export KUBECONFIG="$KC"
  kubectl get pods -A 2>/dev/null || true
  kubectl get kustomization,gitrepository,helmrelease -A 2>/dev/null || true
  exit 1
fi

step "Asserting the demo app serves the seeded message"
export KUBECONFIG="$KC"
kubectl -n podinfo port-forward svc/podinfo 9898:9898 >/dev/null 2>&1 &
pf=$!; sleep 5
body=$(curl -sf http://localhost:9898/ || true)
kill $pf 2>/dev/null || true
echo "$body"
if ! echo "$body" | grep -q "delivered through dabba"; then
  echo; echo "########## [$SUBSTRATE] FAIL (up) ##########"; exit 1
fi
echo "  up OK"

step "dabba status + diagram (smoke)"
"$DABBA" status -c "$H/dabba.yaml" || true
"$DABBA" diagram -c "$H/dabba.yaml" || true
"$DABBA" diagram -c "$H/dabba.yaml" --mermaid || true

step "dabba down (tearing the cluster back down)"
"$DABBA" down -c "$H/dabba.yaml"
present() {
  case "$SUBSTRATE" in
    kind)     kind get clusters 2>/dev/null | grep -qx "$ENVNAME" ;;
    k3d)      k3d cluster list 2>/dev/null | grep -qw "$ENVNAME" ;;
    minikube) minikube profile list 2>/dev/null | grep -qw "$ENVNAME" ;;
  esac
}
if present; then
  echo; echo "########## [$SUBSTRATE] FAIL (down: cluster still present) ##########"; exit 1
fi

echo; echo "########## [$SUBSTRATE] PASS (up + down) ##########"
