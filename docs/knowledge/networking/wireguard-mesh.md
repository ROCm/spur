# WireGuard Mesh Networking

The encrypted overlay between cluster nodes. `spur-net` owns the WireGuard
primitives; the controller's k0s reconcile loop (`spurctld` `cluster_k8s`) and
`spurd`'s `ApplyMesh` handler drive it. Operator bring-up, removal, and HA
steps are in `docs/deployment/wireguard.rst`. The k0s side (mesh IP and pod /24
assignment, level-triggered membership push) is in
`docs/knowledge/execution/containers-k0s.md` ("k0s controller side").

## Topology

- **Hub-and-spoke** is the default without k0s. `spur net join` peers each
  worker only with the controller, with `AllowedIPs` covering the whole mesh
  CIDR. Enough for control-plane gRPC; worker-to-worker traffic hairpins
  through the hub.
- **Full mesh** (every node peers every other node directly) comes from either
  the k0s reconciler's `ApplyMesh` push (`[cluster].enabled`, leader only, at
  least two meshed nodes) or a manual `spur net mesh --config <membership>
  --self <ip>` on every node. It is required when a CNI runs in native-routing
  mode on top of the mesh, because raw pod packets must flow worker-to-worker
  without hairpinning.

## Keys

- Each node generates its keypair locally (`wireguard::generate_keypair`,
  shelling out to `wg genkey` / `wg pubkey`). Private keys never leave the node.
- The controller learns a node's public key from the `spurd` register and
  heartbeat RPCs (`wg_pubkey`). `spur net join` makes no RPC.

## Persistence and locking

- Config files (`/etc/wireguard/<interface>.conf`) are written by
  `WgConfig::write_to`: temp file, fsync, rename, fsync directory, and mode
  `0600` from creation, because the file holds the private key.
- `add_peer_durable` and `remove_peer_durable` change both the live interface
  (`wg set`) and the persisted config, persisting first: if the live `wg set`
  fails, the config already reflects the intended state for a later reload or
  retry. Removal is idempotent.
- Additive vs pruning mesh application (`apply_mesh_durable` vs
  `reconcile_mesh`) and the `protected` peer exemption are described in
  `crates/spur-net/AGENTS.md`.
- Config read-modify-writes inside `with_config_lock` are serialized by an
  advisory lock on the sibling `<iface>.lock`. Without it, two concurrent
  `add-peer` calls read the same base config and the second write silently
  drops the first's peer. `spur net init` and `spur net join` write the config
  without taking the lock.
- `WgConfig::parse` keeps `[Interface]` and `[Peer]` directives it does not
  model (`PostUp`, `MTU`, `PresharedKey`, ...) in `extra`, so a read-modify-write
  never drops operator-maintained configuration.

## Pod-CIDR routing and route ownership

- WireGuard is a cryptokey router: it only forwards packets whose destination
  is in some peer's `AllowedIPs`. For a native-routing CNI (Calico `bird`/BGP,
  no VXLAN/IPIP), `mesh::peer_allowed_ips` sets each peer to its `/32` mesh
  address plus its optional pod CIDR.
- The `ApplyMesh` path never programs kernel routes: `MeshMembership` carries
  only `nodes`, and `spurd`'s `apply_mesh` handler passes
  `program_routes = false`. Calico owns the FIB routes, and Spur programming
  them would fight its reconcile loop. Only the CLI (`spur net mesh` /
  `add-peer --program-routes`) installs routes.
- Mesh-native routing is on only when `ClusterNetworking::mesh_native`
  (`wg_enabled && cni == "calico"`). kube-router tunnels pods over the underlay,
  so a mesh IP must never be advertised to it.

## NAT traversal and endpoints

- Mesh peers carry `PersistentKeepalive` (`mesh::mesh_peers_for`) to keep NAT
  mappings alive.
- `MeshNode.endpoint` is the peer's underlay `host:port`. The controller fills
  it from its own `wg show <interface> endpoints` (`wireguard::peer_endpoints`
  in `build_mesh_membership`), so workers learn each other's underlay addresses
  without a separate signaling channel. A peer behind NAT that connects inbound
  to the controller gets its learned endpoint re-advertised for direct
  worker-to-worker tunnels.
