# 00-network — WireGuard mesh + host prerequisites (plan M0)

The k0s cluster addresses nodes over the SPUR `spur0` mesh (10.44.0.0/16). Bring the mesh up before
`k0sctl apply`. On this cluster all M0 gates must pass (gfx950 workers, flat routed underlay,
direct egress) — record the results in `../RUNBOOK.md`.

## Mesh (SPUR)

```bash
# Head:
sudo spur net init --cidr 10.44.0.0/16 --port 51820        # head gets spur0 = 10.44.0.1
# Each worker (map: gpu-node-1..4 -> 10.44.0.2..5):
sudo spur net join --endpoint <head-underlay-ip>:51820 --server-key <pub> --address 10.44.0.<N>
# Head registers each worker (full mesh: repeat so every worker peers every other):
sudo spur net add-peer --key <node-pub> --allowed-ip 10.44.0.<N>/32 --endpoint <node-underlay-ip>:51820
```

## Pod routing over the mesh (Calico `bird`, no second overlay)

We run Calico in BGP native-routing mode (`../10-cluster/k0sctl.yaml`), so pod traffic rides the
WireGuard mesh directly — no VXLAN-on-WireGuard. For that, the mesh must **route pod CIDRs**, not
just node IPs:

1. Give each node a **deterministic pod /24** — kube-controller-manager `--allocate-node-cidrs`
   populates `node.spec.podCIDR` (e.g. gpu-node-1 → 10.42.1.0/24 … gpu-node-4 → 10.42.4.0/24).
2. Program that pod /24 into the node's WireGuard peer **AllowedIPs** (alongside its `/32` mesh IP),
   on every peer (full mesh), so WireGuard forwards the raw pod packets Calico routes:
   ```bash
   sudo spur net add-peer --key <node-pub> \
     --allowed-ip 10.44.0.<N>/32,10.42.<N>.0/24 --endpoint <node-underlay-ip>:51820
   ```
   Multi-CIDR AllowedIPs is the **spur-net enhancement** on the plan's gap list. Until `spur net`
   emits it, add the pod-CIDR AllowedIPs + route by hand (`wg set … allowed-ips …` + `ip route add
   10.42.<N>.0/24 dev spur0`), or switch to the VXLAN fallback in `k0sctl.yaml` (no mesh change).

Calico BGP distributes the pod-CIDR *routes*; WireGuard AllowedIPs is what *permits* the traffic on
the wire — both are required.

## Host prerequisites (every node)

```bash
# Firewall: allow intra-mesh k8s ports on spur0 (firewalld/ufw block by default)
#   6443/tcp (apiserver) · 10250/tcp (kubelet) · 179/tcp (Calico BGP, bird mode) ·
#   8132/tcp (k0s konnectivity) · 9443/tcp (k0s API) · 2379-2380/tcp (etcd, HA controllers only)
#   [VXLAN fallback only: also 4789/udp (Calico VXLAN)]
# MTU: with bird (no overlay) calico mtu ~= spur0 MTU; with the VXLAN fallback use spur0-50.
#   Set it in ../10-cluster/k0sctl.yaml.
ip link show spur0
# Pod egress (only if a node lacks its own egress — here all nodes have direct egress, so usually skip):
#   sudo iptables -t nat -A POSTROUTING -s 10.42.0.0/16 -o <uplink> -j MASQUERADE
```

Log each command actually run into `../RUNBOOK.md`.
