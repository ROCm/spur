#!/usr/bin/env bash
# Label nodes so the device plugin + ARC runners + serving pods land only on GPU workers.
# Idempotent (--overwrite). Run once after the cluster is Ready (kubeconfig at the example root, ../kubeconfig).
set -euo pipefail
export KUBECONFIG="${KUBECONFIG:-$(dirname "$0")/../kubeconfig}"

# GPU workers (k0s: control-plane node has no kubelet, so only workers appear).
for n in $(kubectl get nodes -o name | sed 's|node/||'); do
  # Heuristic: any node reporting amd.com/gpu is a GPU worker. Adjust if you label by hostname.
  if kubectl get node "$n" -o jsonpath='{.status.allocatable.amd\.com/gpu}' 2>/dev/null | grep -qE '^[1-9]'; then
    kubectl label node "$n" spur.amd.com/compute=true --overwrite
    echo "labelled $n spur.amd.com/compute=true"
  fi
done

# Chicken-and-egg: the device plugin needs the label before it advertises amd.com/gpu.
# First run, label GPU workers explicitly by name instead (edit the list):
#   for n in gpu-node-1 gpu-node-2 gpu-node-3 gpu-node-4; do
#     kubectl label node "$n" spur.amd.com/compute=true --overwrite
#   done
