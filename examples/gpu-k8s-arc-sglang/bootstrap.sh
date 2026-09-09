#!/usr/bin/env bash
# Ordered, mostly-idempotent bring-up of the SPUR-hosted GPU k8s cluster + ARC + sglang.
# This is the human-runnable seed of `spur k8s up`. Each step logs to RUNBOOK.md by hand as you go.
# Steps that need secrets/pins are marked MANUAL — do them, then re-run to continue.
set -euo pipefail
cd "$(dirname "$0")"
export KUBECONFIG="${KUBECONFIG:-$PWD/kubeconfig}"

echo "== 00-network =="            # MANUAL: bring up the WireGuard mesh + firewall (see 00-network/README.md)
echo "   ensure spur0 mesh is up and M0 gates pass (record in RUNBOOK.md), then continue."

echo "== 10-cluster (k0s via k0sctl) =="
# MANUAL: fill real ssh addresses/pins in 10-cluster/k0sctl.yaml first.
k0sctl apply --config 10-cluster/k0sctl.yaml
k0sctl kubeconfig --config 10-cluster/k0sctl.yaml > kubeconfig
kubectl get nodes -o wide

echo "== 15-addons (local-path + ingress-nginx) =="   # see 15-addons/README.md for the pinned commands
#  (run the helm/kubectl commands from 15-addons/README.md, pinned via versions.lock.md)

echo "== 20-gpu (device plugin + labels) =="
# First run: label GPU workers BY NAME before the plugin (nodeSelector chicken-and-egg).
for n in gpu-node-1 gpu-node-2 gpu-node-3 gpu-node-4; do   # <-- edit to your real worker node names
  kubectl label node "$n" spur.amd.com/compute=true --overwrite || true
done
kubectl apply -f 20-gpu/amd-device-plugin.yaml
bash 20-gpu/node-labels.sh    # subsequent runs: relabel any node already reporting amd.com/gpu
kubectl get nodes -o json | jq -r '.items[]|.metadata.name+" gpu="+(.status.allocatable["amd.com/gpu"]//"0")'

echo "== 30-arc (controller + secrets + scale sets) =="   # MANUAL secrets — see 30-arc/install.md
#  helm install arc ...controller ; create sglang-arc-app + ghcr-pull ; helm install both scale sets

echo "== 40-serving (namespace + deployer RBAC; workload deployed by CI) =="
kubectl create namespace sglang      --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace arc-runners --dry-run=client -o yaml | kubectl apply -f -   # deployer SA lives here
kubectl apply -f 40-serving/rbac-deployer.yaml
# Optional: kubectl -n sglang create secret generic hf-token --from-literal=token=<HF_TOKEN>   # gated models only
echo "   serving workload is applied by the fork CI deploy job (40-serving/fork-ci.yml) on green CI."

echo "== done: run 'make capture' to snapshot state =="
