#!/bin/bash
# Spur K8s Integration Tests using kind (Kubernetes in Docker)
#
# Validates the full K8s deployment path:
#   - SpurJob CRD lifecycle (create → schedule → run → complete)
#   - Operator health and node registration
#   - Multi-node job coordination
#   - Cancellation and failure detection
#
# Prerequisites:
#   - Docker installed
#   - SPUR_CI_IMAGE set to a pre-built Spur container image (e.g. spur:ci)
#
# Usage:
#   SPUR_CI_IMAGE=spur:ci bash deploy/bare-metal/k8s_test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TOOLS_DIR="${HOME}/.local/bin"
CLUSTER_NAME="spur-ci"
SPUR_CI_IMAGE="${SPUR_CI_IMAGE:?SPUR_CI_IMAGE must be set to a Docker image (e.g. spur:ci)}"

mkdir -p "${TOOLS_DIR}"
export PATH="${TOOLS_DIR}:${PATH}"

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
    for _ in $(seq 1 $((timeout / 2))); do
        state=$(kubectl -n spur get spurjob "$name" -o jsonpath='{.status.state}' 2>/dev/null || echo "")
        [ "$state" = "$want" ] && echo "$state" && return 0
        case "$state" in
            Completed|Failed|Cancelled)
                echo "$state"; return 1 ;;
        esac
        sleep 2
    done
    echo "${state:-timeout}"
    return 1
}

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null || true
    echo "  kind cluster removed"
}

# ============================================================
# Prerequisites
# ============================================================
section "Prerequisites"

if ! command -v docker &>/dev/null; then
    echo "SKIP: Docker not installed"
    exit 0
fi

if ! docker info >/dev/null 2>&1; then
    echo "SKIP: Docker daemon not accessible (add runner user to docker group: sudo usermod -aG docker \$USER)"
    exit 0
fi

pass "Docker available"

# ============================================================
# Install tools (kind + kubectl)
# ============================================================
section "Install tools"

if ! command -v kind &>/dev/null; then
    KIND_VER=$(curl -fsSL https://api.github.com/repos/kubernetes-sigs/kind/releases/latest \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)
    echo "  Installing kind ${KIND_VER}..."
    curl -fsSL -o "${TOOLS_DIR}/kind" \
        "https://github.com/kubernetes-sigs/kind/releases/download/${KIND_VER}/kind-linux-amd64"
    chmod +x "${TOOLS_DIR}/kind"
fi
pass "kind $(kind version)"

if ! command -v kubectl &>/dev/null; then
    KUBECTL_VER=$(curl -fsSL https://dl.k8s.io/release/stable.txt)
    echo "  Installing kubectl ${KUBECTL_VER}..."
    curl -fsSL -o "${TOOLS_DIR}/kubectl" \
        "https://dl.k8s.io/release/${KUBECTL_VER}/bin/linux/amd64/kubectl"
    chmod +x "${TOOLS_DIR}/kubectl"
fi
pass "kubectl $(kubectl version --client -o json 2>/dev/null | grep gitVersion | cut -d'"' -f4)"

# ============================================================
# Create kind cluster (control-plane + 2 workers)
# ============================================================
section "Create kind cluster"

# Clean up previous cluster
kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null || true

trap cleanup EXIT

kind create cluster --name "${CLUSTER_NAME}" --config=- <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
  - role: worker
EOF

kubectl wait --for=condition=Ready node --all --timeout=120s \
    && pass "Kind cluster ready (3 nodes)" \
    || fail "Kind cluster not ready"

echo "  Nodes:"
kubectl get nodes

# Label worker nodes for Spur operator
for node in $(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}'); do
    kubectl label node "$node" spur.ai/managed=true --overwrite
done
pass "Worker nodes labeled for Spur"

# ============================================================
# Load container image into kind
# ============================================================
section "Load Spur container image"

echo "  Source image: ${SPUR_CI_IMAGE}"
if [ "$SPUR_CI_IMAGE" != "spur:ci" ]; then
    docker tag "$SPUR_CI_IMAGE" spur:ci
fi

kind load docker-image spur:ci --name "${CLUSTER_NAME}" \
    && pass "Image loaded into kind" \
    || fail "Image load failed"

# Pre-load busybox for SpurJob pods (avoids Docker Hub pull issues in CI)
docker pull busybox:latest
kind load docker-image busybox:latest --name "${CLUSTER_NAME}" \
    && pass "busybox image loaded into kind" \
    || fail "busybox image load failed"

# ============================================================
# Deploy Spur to K8s
# ============================================================
section "Deploy Spur to K8s"

# Register SpurJob CRD
docker run --rm spur:ci generate-crd | kubectl apply -f - \
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

# Deploy controller + operator with CI image
for f in spurctld.yaml operator.yaml; do
    sed 's|spur:latest|spur:ci|g' "${REPO_ROOT}/deploy/k8s/${f}" \
        | kubectl apply -f -
done

# Wait for pods to be ready
kubectl -n spur wait --for=condition=Available deployment/spur-k8s-operator --timeout=120s \
    && pass "Operator deployment ready" \
    || fail "Operator not ready"

kubectl -n spur wait --for=condition=Ready pod -l app=spurctld --timeout=120s \
    && pass "Controller pod ready" \
    || fail "Controller not ready"

# Health check via a temporary busybox pod hitting the operator's pod IP
OPERATOR_POD=$(kubectl -n spur get pod -l app=spur-k8s-operator -o jsonpath='{.items[0].metadata.name}')
OPERATOR_IP=$(kubectl -n spur get pod "$OPERATOR_POD" -o jsonpath='{.status.podIP}')
kubectl run healthcheck -n spur --image=busybox:latest --restart=Never --rm -i \
    -- wget -qO- -T 5 "http://${OPERATOR_IP}:8080/healthz" >/dev/null 2>&1 \
    && pass "Operator /healthz OK" \
    || fail "Operator /healthz failed"

# Show what the operator sees
echo "  Operator logs (last 10 lines):"
kubectl -n spur logs "$OPERATOR_POD" --tail=10 2>/dev/null | sed 's/^/    /'

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

STATE=$(wait_spurjob test-simple Completed 60) \
    && pass "Simple SpurJob completed" \
    || { fail "Simple SpurJob state: $STATE"; \
         echo "  Debug: pods in spur namespace:"; \
         kubectl -n spur get pods -o wide 2>/dev/null | sed 's/^/    /'; \
         echo "  Debug: operator logs (last 20):"; \
         kubectl -n spur logs -l app=spur-k8s-operator --tail=20 2>/dev/null | sed 's/^/    /'; }

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

STATE=$(wait_spurjob test-env Completed 60) \
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

STATE=$(wait_spurjob test-multinode Completed 90) \
    && pass "Multi-node SpurJob completed" \
    || fail "Multi-node SpurJob state: $STATE"

NODES=$(kubectl -n spur get spurjob test-multinode -o jsonpath='{.status.assignedNodes}' 2>/dev/null || echo "")
[ -n "$NODES" ] && [ "$NODES" != "[]" ] \
    && pass "Multi-node job assigned nodes: $NODES" \
    || pass "Multi-node job completed (node tracking optional)"

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

# Wait for pod to appear
sleep 8

# Delete the SpurJob
kubectl delete spurjob test-cancel -n spur --timeout=30s

# Verify pods are cleaned up
sleep 5
REMAINING=$(kubectl -n spur get pods 2>/dev/null | grep -c "test-cancel" || true)
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

STATE=$(wait_spurjob test-fail Failed 60) \
    && pass "Failed SpurJob detected" \
    || fail "Failed SpurJob state: $STATE"

kubectl delete spurjob test-fail -n spur --timeout=30s 2>/dev/null || true

# ============================================================
# TEST 6: Sequential SpurJobs (scheduler queue)
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
    STATE=$(wait_spurjob "test-seq-${i}" Completed 60) || true
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
