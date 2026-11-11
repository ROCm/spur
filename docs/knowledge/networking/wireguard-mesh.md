# WireGuard Mesh Networking

Spur uses WireGuard (in-kernel since Linux 5.6) to build an encrypted overlay
network between cluster nodes. The implementation lives in `spur-net` and is
driven by the controller's k0s reconcile loop (`spurctld/src/cluster_k8s.rs`).

## Topology

Two modes:

**Hub-and-spoke** (default, without k0s). Each worker peers only the
controller. `AllowedIPs` covers the whole mesh CIDR. Sufficient for control-
plane traffic (gRPC between agent and controller). Worker-to-worker traffic
hairpins through the hub.

**Full mesh.** Default is hub-and-spoke. A full mesh comes from either the
k0s reconciler's `ApplyMesh` push (`[cluster].enabled`, leader,
>=2 meshed nodes) or a manual `spur net mesh --config <membership> --self
<ip>` on every node. Every node peers every other node directly. Required
when a CNI runs in native-routing mode (no overlay) on top of the mesh,
because raw pod packets must flow worker-to-worker without hairpinning.

## Key Exchange and Keypair Management

Each node generates a WireGuard keypair locally by shelling out to `wg genkey`
and `wg pubkey` (`wireguard::generate_keypair()`). The public key is sent to the controller as part of the `spurd`
register/heartbeat RPC; `spur net join` itself makes no RPC. Private keys
never leave the node.

The keypair and interface config are stored at `/etc/wireguard/<interface>.conf`
(default interface: `spur0`). Files are written atomically (write to temp, fsync,
rename, fsync directory) and at mode `0600` from the moment of creation.

## Tunnel Management

The interface is brought up by `wg-quick up` and mutated live via `wg set`
(peer additions/removals without restart).

**Peer lifecycle:**

- `add_peer_durable()` -- adds a peer both live (`wg set`) and in the persisted
  config, so it survives a reboot. Persists first: if the live `wg set` fails,
  the config already reflects the intended state for a future reload.
- `remove_peer_durable()` -- removes a peer from both live and persisted config.
  Idempotent.
- `apply_mesh_durable()` -- applies the full peer set in one config
  read-modify-write, then live. Additive (does not prune departed peers).
- `reconcile_mesh()` -- full convergence: prunes peers no longer in the
  membership, then adds/updates desired peers.

**Concurrent mutation safety.** Config mutations that go through
`with_config_lock()` are serialized by an advisory file lock on
`<iface>.lock`. Without this, two concurrent `add-peer` calls could each read
the same base config and the second write would silently drop the first's
peer. Note: `net init` and `net join` write the config without acquiring the
lock.

**Unmodeled directives.** The config parser preserves `[Interface]` and `[Peer]`
directives it does not model (e.g. `PostUp`, `MTU`, `PresharedKey`) in an
`extra` field, so a read-modify-write never silently drops operator-maintained
configuration.

## Pod-CIDR Routing

WireGuard is a cryptokey router: it only forwards packets whose destination is
in some peer's `AllowedIPs`. For native-routing CNIs (e.g. Calico `bird`/BGP,
no VXLAN/IPIP overlay), each peer's `AllowedIPs` must include that node's pod
CIDR in addition to its `/32` mesh address.

The `mesh` module computes `AllowedIPs` per peer: `<mesh_ip>/32` plus the
optional `<pod_cidr>`. Example: `10.44.0.3/32,10.42.2.0/24`.

The `ApplyMesh` path never programs routes. `MeshMembership` (slurm.proto)
carries only `nodes`; `spurd`'s `ApplyMesh` handler hard-codes
`program_routes = false`. Only the CLI (`spur net mesh` / `add-peer
--program-routes`) can install FIB routes. When Calico is the CNI, it owns
the routes; Spur's `ApplyMesh` avoids fighting Calico's reconcile loop.

## k0s Networking Integration

The k0s cluster controller (`spurctld/src/cluster_k8s.rs`) manages the overlay
for the Spur-managed Kubernetes cluster. Key config fields in `ClusterNetworking`:

| Field | Purpose |
|-------|---------|
| `wg_enabled` | Whether WireGuard mesh is active |
| `mesh_cidr` | CIDR for node mesh IPs (e.g. `10.44.0.0/16`) |
| `mesh_interface` | WireGuard interface name (e.g. `spur0`) |
| `pod_cidr` | Pod CIDR, carved into per-node /24s |
| `service_cidr` | Kubernetes service CIDR |
| `cni_mtu` | MTU for the CNI |
| `cni` | `kuberouter` (default) or `calico` |

**Mesh-native routing** is enabled only when `wg_enabled && cni == "calico"`.
With kube-router, pods tunnel over the underlay and mesh IPs are never advertised
to the CNI.

Mesh IPs are adopted from each node's advertised `spur0` address first; pod
/24s are carved by node ordinal, not dynamically allocated from `AddressPool`.

The reconcile loop runs every 30 seconds, leader-gated via Raft. On each tick:

1. Leader check (Raft-gated).
2. Provisioning clock update for nodes that stay non-active.
3. `build_mesh_membership` -- assemble the current mesh node list.
4. `ApplyMesh` push to every node via `SlurmAgent.ApplyMesh`
   (`spawn_apply_mesh` in `cluster_k8s.rs`), if >=2 meshed nodes.
5. `reconcile_phase` -- k8s provisioning.
6. `detect_drift` -- check for configuration drift.

## NAT Traversal

WireGuard's built-in `PersistentKeepalive` (set to 25 seconds by default in
mesh peers) keeps NAT mappings alive. The `endpoint` field on each `MeshNode`
records the underlay `host:port` for the tunnel. Peers behind NAT that cannot
be reached directly connect inbound to the controller (hub-and-spoke); the
controller re-advertises their learned endpoints in the mesh membership for
direct worker-to-worker tunnels once the NAT pinhole is established.

The controller reads peer endpoints from `wg show <interface> endpoints` and
distributes them in the `MeshMembership` document so workers can discover each
other's underlay addresses without a separate signaling channel.

## CLI Commands

| Command | Purpose |
|---------|---------|
| `spur net init` | Generate keypair, write initial config, bring up interface |
| `spur net join` | Generate keypair, write WireGuard config, bring up interface (no RPC; controller learns the key from spurd register/heartbeat) |
| `spur net mesh` | Apply full-mesh peering from a membership document |
| `spur net status` | Show interface status, peers, endpoints |
| `spur net add-peer` | Add a single peer (durable) |
| `spur net remove-peer` | Remove a single peer (durable) |

## Key Source Files

| File | Purpose |
|------|---------|
| `spur-net/src/wireguard.rs` | Key generation, config parse/write, `wg`/`wg-quick` shims |
| `spur-net/src/mesh.rs` | Full-mesh peer computation, pod-CIDR routing |
| `spur-net/src/detect.rs` | Node address auto-detection |
| `spur-net/src/address.rs` | IP/CIDR address pool allocation |
| `spurctld/src/cluster_k8s.rs` | k0s reconcile loop (mesh + k8s provisioning) |
| `spur-cli/src/net.rs` | `spur net` CLI commands |
