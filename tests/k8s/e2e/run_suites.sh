#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Run the five k8s e2e suites concurrently, each in its own namespace so their
# operator, spurctld, ConfigMap, and RBAC are isolated. Destructive node tests
# stay skipped (SPUR_TEST_DESTRUCTIVE_NODES unset), so no suite mutates shared
# cluster-scoped Nodes and the suites can safely overlap on one cluster.
#
# Usage: run_suites.sh <k8s-e2e-assets-dir> <run-id> <results-dir>

set -uo pipefail

ASSETS="$1"
RUN_ID="$2"
RESULTS="$3"
RBAC="$ASSETS/manifests/rbac.yaml"
SUITES="core spec quota ha nodes"

mkdir -p "$RESULTS"
pids=""
rc=0

for s in $SUITES; do
  ns="spur-ci-${RUN_ID}-${s}"
  kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f -
  sed "s/namespace: spur/namespace: $ns/g" "$RBAC" | kubectl apply -f -
  (
    SPUR_TEST_NS="$ns" pytest "$ASSETS" -m "suite_k8s_${s}" -v \
      --junitxml="$RESULTS/results-${s}.xml"
  ) &
  pids="$pids $!"
done

for p in $pids; do
  wait "$p" || rc=1
done

exit $rc
