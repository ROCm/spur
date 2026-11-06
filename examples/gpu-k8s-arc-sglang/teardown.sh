#!/usr/bin/env bash
# Reverse-order teardown. ARC runner scale sets MUST go before the controller or finalizers hang.
set -uo pipefail
cd "$(dirname "$0")"
export KUBECONFIG="${KUBECONFIG:-$PWD/kubeconfig}"

echo "== serving =="
kubectl delete -f 40-serving/sglang-service-amd.yaml --ignore-not-found
kubectl delete -f 40-serving/ingress.yaml --ignore-not-found
kubectl delete namespace sglang --ignore-not-found

echo "== ARC (scale sets BEFORE controller) =="
helm uninstall linux-mi35x-gpu-1 -n arc-runners || true
helm uninstall linux-cpu-deploy  -n arc-runners || true
helm uninstall arc -n arc-systems || true          # controller last
kubectl delete namespace arc-runners arc-systems --ignore-not-found

echo "== gpu + addons =="
kubectl delete -f 20-gpu/amd-device-plugin.yaml --ignore-not-found
# helm uninstall ingress-nginx -n ingress-nginx ; kubectl delete -f <local-path-storage.yaml>

echo "== cluster (k0s reset on every node) =="
k0sctl reset --config 10-cluster/k0sctl.yaml || true

echo "== mesh + SPUR =="
echo "   MANUAL: reverse the SPUR scheduler drain (re-admit GPU nodes), then tear down the mesh:"
echo "     on each node: sudo spur net leave   (or wg-quick down spur0)"
echo "   reconcile the mesh back to hub-and-spoke if you keep SPUR running."
