# Convenience targets. Everything is idempotent-ish; read each script before running on real hardware.
export KUBECONFIG ?= $(CURDIR)/kubeconfig

.PHONY: up cluster gpu arc serve down capture nodes

up:                    ## full ordered bring-up (network is manual — see 00-network/README.md)
	./bootstrap.sh

cluster:               ## k0s only
	k0sctl apply --config 10-cluster/k0sctl.yaml
	k0sctl kubeconfig --config 10-cluster/k0sctl.yaml > kubeconfig

gpu:                   ## device plugin + node labels
	kubectl apply -f 20-gpu/amd-device-plugin.yaml && ./20-gpu/node-labels.sh

arc:                   ## reminder: follow 30-arc/install.md (needs secrets + pinned chart versions)
	@echo "See 30-arc/install.md — controller, secrets, then both scale sets (pinned)."

serve:                 ## namespace + deployer RBAC (workload is deployed by CI)
	kubectl create namespace sglang --dry-run=client -o yaml | kubectl apply -f -
	kubectl apply -f 40-serving/rbac-deployer.yaml

nodes:                 ## quick health view
	kubectl get nodes -o wide

capture:               ## snapshot live cluster state for drift-checking against this tree
	@d=state/$$(date +%Y%m%d-%H%M%S); mkdir -p $$d; \
	 kubectl get all -A -o yaml > $$d/all.yaml; \
	 kubectl get nodes -o yaml > $$d/nodes.yaml; \
	 helm list -A > $$d/helm.txt 2>/dev/null || true; \
	 kubectl get crd -o name > $$d/crds.txt; \
	 echo "captured -> $$d"

down:                  ## reverse-order teardown
	./teardown.sh
