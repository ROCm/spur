# 15-addons — the standard upstream pieces k0s does NOT bundle

k0s ships a bare cluster (no ingress / LB / storage). We add the *documented upstream* components,
so nothing here is distro-specific magic. Pin every version in `../versions.lock.md`.

## Storage — local-path-provisioner (RWO, node-local)

k8s has no default StorageClass; sglang's HF cache PVC needs one.

```bash
kubectl apply -f https://raw.githubusercontent.com/rancher/local-path-provisioner/VERSION/deploy/local-path-storage.yaml
# make it the cluster default so PVCs without a class bind:
kubectl patch storageclass local-path \
  -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}'
```
Result: `storageClassName: local-path` (RWO). The serving PVC in `../40-serving` uses it.

## Ingress — ingress-nginx (NodePort, mesh-reachable)

No cloud LB here, so expose the ingress controller as a NodePort reachable over the mesh/underlay.

```bash
helm install ingress-nginx ingress-nginx \
  --repo https://kubernetes.github.io/ingress-nginx --version VERSION \
  -n ingress-nginx --create-namespace \
  --set controller.service.type=NodePort \
  --set controller.service.nodePorts.http=30080 \
  --set controller.service.nodePorts.https=30443 \
  --set controller.hostNetwork=false
```
Reach the served model at `http://<any-node-mesh-or-underlay-ip>:30080` via the Ingress in
`../40-serving/ingress.yaml`.

## Optional — MetalLB (only if you want real `LoadBalancer` IPs)

Skip unless you have a spare IP range on the node subnet to hand out. If you do:
```bash
helm install metallb metallb --repo https://metallb.github.io/metallb -n metallb-system --create-namespace
# then apply an IPAddressPool + L2Advertisement over your free range.
```
With MetalLB present, switch the serving Service to `type: LoadBalancer`. Without it, keep
ingress-nginx NodePort (the default in this example).

## Optional — metrics-server (kubectl top, HPA)

```bash
helm install metrics-server metrics-server \
  --repo https://kubernetes-sigs.github.io/metrics-server -n kube-system --version VERSION
```
