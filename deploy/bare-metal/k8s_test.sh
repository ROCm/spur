#!/bin/bash
# Spur K8s Integration Tests
#
# Validates the full K8s deployment path on a 2-node MI300X cluster:
#   - K3s cluster setup (server + agent)
#   - SpurJob CRD lifecycle (create → schedule → run → complete)
#   - Operator health and node registration
#   - Multi-node job coordination
#   - Cancellation and failure detection
#
# Prerequisites:
#   - Spur binaries at ~/spur/bin/ (from cluster job)
#   - SSH access to mi300-2
#   - Docker installed (for container image build)
#   - sudo access (for K3s install)
#
# Usage: bash deploy/bare-metal/k8s_test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SPUR_HOME="${HOME}/spur"
SPUR_BIN="${SPUR_HOME}/bin"

PASS=0
FAIL=0
TOTAL=0

pass() { TOTAL=$((TOTAL + 1)); PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { TOTAL=$((TOTAL + 1)); FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }
section() { echo ""; echo "=== $1 ==="; }

wait_spurjob() {
    local name="$1"
    local want="$2"
    local timeout="${3:-60}"
    local state=""
    for i in $(seq 1 $((timeout / 2))); do
        state=$(kubectl -n spur get spurjob "$name" -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        [ "$state" = "$want" ] && echo "$state" && return 0
        # Early exit on terminal states if we're not waiting for them
        case "$state" in
            Completed|Failed|Cancelled)
                [ "$state" = "$want" ] && echo "$state" && return 0
                echo "$state" && return 1 ;;
        esac
        sleep 2
    done
    echo "${state:-timeout}"
    return 1
}

# ============================================================
# Prerequisites
# ============================================================
section "Prerequisites"

if ! command -v docker &>/dev/null; then
    echo "SKIP: Docker not installed (required for container image build)"
    exit 0
fi

if [ ! -x "${SPUR_BIN}/spurctld" ]; then
    echo "ERROR: Spur binaries not found at ${SPUR_BIN}"
    exit 1
fi

pass "Docker and Spur binaries available"

# ============================================================
# Cleanup previous K3s (idempotent)
# ============================================================
section "Cleanup previous K3s"
ssh mi300-2 'sudo /usr/local/bin/k3s-agent-uninstall.sh 2>/dev/null; true'
sudo /usr/local/bin/k3s-uninstall.sh 2>/dev/null || true
echo "  Previous K3s state cleared"

# ============================================================
# Install K3s cluster
# ============================================================
section "Install K3s cluster"

# Server on this node
curl -sfL https://get.k3s.io | sudo INSTALL_K3S_EXEC="--disable=traefik --write-kubeconfig-mode=644" sh -
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml

kubectl wait --for=condition=Ready "node/$(hostname)" --timeout=60s \
    && pass "K3s server node ready" \
    || fail "K3s server not ready"

# Agent on mi300-2
K3S_TOKEN=$(sudo cat /var/lib/rancher/k3s/server/node-token)
SERVER_IP=$(hostname -I | awk '{print $1}')
ssh mi300-2 "curl -sfL https://get.k3s.io | sudo K3S_URL=https://${SERVER_IP}:6443 K3S_TOKEN=${K3S_TOKEN} sh -"

# Wait for both nodes (agent takes a few seconds to register)
sleep 10
kubectl wait --for=condition=Ready node --all --timeout=120s \
    && pass "Both K3s nodes ready" \
    || fail "K3s nodes not ready"

echo "  Nodes:"
kubectl get nodes -o wide

# Label nodes for operator node-watcher
for node in $(kubectl get nodes -o jsonpath='{.items[*].metadata.name}'); do
    kubectl label node "$node" spur.ai/managed=true --overwrite
done
pass "Nodes labeled for Spur operator"

# ============================================================
# Build and import container image
# ============================================================
section "Build Spur container image"

BUILD_DIR=$(mktemp -d)

cat > "${BUILD_DIR}/Dockerfile" <<'DOCKERFILE'
FROM ubuntu:22.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates util-linux curl && rm -rf /var/lib/apt/lists/*
COPY bin/ /usr/local/bin/
DOCKERFILE

mkdir -p "${BUILD_DIR}/bin"
for b in spur spurctld spurd spurdbd spurrestd spur-k8s-operator; do
    cp "${SPUR_BIN}/${b}" "${BUILD_DIR}/bin/"
done

docker build -t spur:ci "${BUILD_DIR}" \
    && pass "Container image built" \
    || fail "Container image build failed"

# Import into K3s containerd on both nodes
docker save spur:ci | sudo k3s ctr images import - \
    && pass "Image imported (local)" \
    || fail "Image import failed (local)"

docker save spur:ci | ssh mi300-2 'sudo k3s ctr images import -' \
    && pass "Image imported (mi300-2)" \
    || fail "Image import failed (mi300-2)"

rm -rf "${BUILD_DIR}"

# ============================================================
# Deploy Spur to K8s
# ============================================================
section "Deploy Spur to K8s"

# Register SpurJob CRD
"${SPUR_BIN}/spur-k8s-operator" generate-crd | kubectl apply -f - \
    && pass "SpurJob CRD registered" \
    || fail "CRD registration failed"

# Namespace + RBAC
kubectl apply -f "${REPO_ROOT}/deploy/k8s/namespace.yaml"
kubectl apply -f "${REPO_ROOT}/deploy/k8s/rbac.yaml"

# CI-specific config (no accounting, fast scheduler)
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: spur-config
  namespace: spur
data:
  spur.conf: |
    cluster_name = "k8s-ci"

    [scheduler]
    interval_secs = 1
    plugin = "backfill"

    [[partitions]]
    name = "default"
    state = "UP"
    default = true
    nodes = "ALL"
    max_time = "1h"
    default_time = "10m"
EOF

# Deploy controller + operator (image: spur:ci)
for f in spurctld.yaml operator.yaml; do
    sed 's|spur:latest|spur:ci|g' "${REPO_ROOT}/deploy/k8s/${f}" \
        | kubectl apply -f -
done

# Wait for pods
kubectl -n spur wait --for=condition=Available deployment/spur-k8s-operator --timeout=120s \
    && pass "Operator deployment ready" \
    || fail "Operator not ready"

kubectl -n spur wait --for=condition=Ready pod -l app=spurctld --timeout=120s \
    && pass "Controller pod ready" \
    || fail "Controller not ready"

# Health check
OPERATOR_POD=$(kubectl -n spur get pod -l app=spur-k8s-operator -o jsonpath='{.items[0].metadata.name}')
kubectl -n spur exec "$OPERATOR_POD" -- curl -sf http://localhost:8080/healthz >/dev/null 2>&1 \
    && pass "Operator /healthz OK" \
    || fail "Operator /healthz failed"

# ============================================================
# TEST 1: Simple SpurJob
# ============================================================
section "TEST 1: Simple SpurJob"

kubectl apply -f - <<'EOF'
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-simple
  namespace: spur
spec:
  name: test-simple
  image: busybox:latest
  command: ["sh", "-c", "echo SPUR_K8S_OK && sleep 1"]
  numNodes: 1
EOF

STATE=$(wait_spurjob test-simple Completed 60)
[ "$STATE" = "Completed" ] \
    && pass "Simple SpurJob completed" \
    || fail "Simple SpurJob state: $STATE"

JOB_ID=$(kubectl -n spur get spurjob test-simple -o jsonpath='{.status.spurJobId}' 2>/dev/null)
[ -n "$JOB_ID" ] \
    && pass "Spur assigned job ID: $JOB_ID" \
    || fail "No Spur job ID assigned"

kubectl delete spurjob test-simple -n spur --timeout=30s 2>/dev/null || true

# ============================================================
# TEST 2: SpurJob with environment variables
# ============================================================
section "TEST 2: SpurJob with environment variables"

kubectl apply -f - <<'EOF'
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-env
  namespace: spur
spec:
  name: test-env
  image: busybox:latest
  command: ["sh", "-c", "echo job=$SPUR_JOB_ID custom=$CUSTOM_VAR"]
  numNodes: 1
  env:
    CUSTOM_VAR: "spur-ci-test"
EOF

STATE=$(wait_spurjob test-env Completed 60)
[ "$STATE" = "Completed" ] \
    && pass "Env var SpurJob completed" \
    || fail "Env var SpurJob state: $STATE"

kubectl delete spurjob test-env -n spur --timeout=30s 2>/dev/null || true

# ============================================================
# TEST 3: Multi-node SpurJob (2 nodes)
# ============================================================
section "TEST 3: Multi-node SpurJob"

kubectl apply -f - <<'EOF'
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-multinode
  namespace: spur
spec:
  name: test-multinode
  image: busybox:latest
  command: ["sh", "-c", "echo rank=$SPUR_NODE_RANK nodes=$SPUR_NUM_NODES host=$(hostname)"]
  numNodes: 2
EOF

STATE=$(wait_spurjob test-multinode Completed 90)
[ "$STATE" = "Completed" ] \
    && pass "Multi-node SpurJob completed" \
    || fail "Multi-node SpurJob state: $STATE"

# Verify pods were tracked in status
PODS=$(kubectl -n spur get spurjob test-multinode -o jsonpath='{.status.pods}' 2>/dev/null || echo "")
[ -n "$PODS" ] && [ "$PODS" != "[]" ] \
    && pass "Multi-node job tracked pods: $PODS" \
    || fail "No pods tracked in SpurJob status"

kubectl delete spurjob test-multinode -n spur --timeout=30s 2>/dev/null || true

# ============================================================
# TEST 4: SpurJob cancellation
# ============================================================
section "TEST 4: SpurJob cancellation"

kubectl apply -f - <<'EOF'
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-cancel
  namespace: spur
spec:
  name: test-cancel
  image: busybox:latest
  command: ["sleep", "600"]
  numNodes: 1
EOF

# Wait for it to start (or at least be pending)
sleep 8

# Delete the SpurJob
kubectl delete spurjob test-cancel -n spur --timeout=30s

# Verify pods are cleaned up
sleep 5
REMAINING=$(kubectl -n spur get pods 2>/dev/null | grep -c "test-cancel" || echo 0)
[ "$REMAINING" -eq 0 ] \
    && pass "Cancelled SpurJob pods cleaned up" \
    || fail "Pods still present after cancel ($REMAINING remaining)"

# ============================================================
# TEST 5: SpurJob failure detection
# ============================================================
section "TEST 5: SpurJob failure detection"

kubectl apply -f - <<'EOF'
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-fail
  namespace: spur
spec:
  name: test-fail
  image: busybox:latest
  command: ["sh", "-c", "exit 42"]
  numNodes: 1
EOF

STATE=$(wait_spurjob test-fail Failed 60)
[ "$STATE" = "Failed" ] \
    && pass "Failed SpurJob detected" \
    || fail "Failed SpurJob state: $STATE"

kubectl delete spurjob test-fail -n spur --timeout=30s 2>/dev/null || true

# ============================================================
# TEST 6: Sequential SpurJobs (scheduler handles queue)
# ============================================================
section "TEST 6: Sequential SpurJobs"

for i in 1 2 3; do
    kubectl apply -f - <<EOF
apiVersion: spur.ai/v1alpha1
kind: SpurJob
metadata:
  name: test-seq-${i}
  namespace: spur
spec:
  name: test-seq-${i}
  image: busybox:latest
  command: ["sh", "-c", "echo seq=${i}"]
  numNodes: 1
EOF
done

ALL_DONE=true
for i in 1 2 3; do
    STATE=$(wait_spurjob "test-seq-${i}" Completed 60)
    if [ "$STATE" != "Completed" ]; then
        ALL_DONE=false
        fail "Sequential job ${i} state: $STATE"
    fi
done
$ALL_DONE && pass "All 3 sequential SpurJobs completed"

for i in 1 2 3; do
    kubectl delete spurjob "test-seq-${i}" -n spur --timeout=10s 2>/dev/null || true
done

# ============================================================
# Summary
# ============================================================
echo ""
echo "========================================"
echo "K8s Integration: ${PASS} passed, ${FAIL} failed (${TOTAL} total)"
echo "========================================"

[ "$FAIL" -eq 0 ] || exit 1
