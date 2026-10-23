# AGENTS.md -- spur-net

WireGuard mesh networking and OCI image pulling for the Spur scheduler.

## Overview

`spur-net` provides three capabilities:

1. **WireGuard mesh networking** -- key generation, config file management, full-mesh peering with pod-CIDR routing, and NAT traversal via persistent keepalives.
2. **Network address detection** -- resolve a node's reachable address from a WireGuard interface or hostname, rejecting loopback/link-local.
3. **OCI image pulling** -- download container images directly from registries (Docker Hub, ghcr.io, nvcr.io, private) without Docker/skopeo, extract layers, pack into squashfs.

No async runtime dependency for the WireGuard/address modules; OCI pull is async (tokio + reqwest).

## Module map

| Module | Purpose |
|---|---|
| `wireguard` | Key generation (`wg genkey/pubkey`), INI config parse/write, peer add/remove (live + durable), interface up/down, route programming, config file locking. |
| `mesh` | Full-mesh topology: compute per-node peer sets, apply/reconcile mesh membership, pod-CIDR-aware `AllowedIPs`. |
| `address` | IPv4 address pool allocation from a CIDR block. `.0` = network (reserved at init), `.1+` = allocated on demand via `allocate()` or `allocate_specific()`. |
| `comm_addr` | Validate/normalize node communication addresses. Rejects loopback, unspecified, link-local. Formats `host:port` for gRPC/HTTP (brackets IPv6). |
| `detect` | Auto-detect node address: WireGuard interface IP > hostname resolution > fallback. |
| `oci` | Registry HTTP API v2 image puller. Auth (Docker Hub token, basic, anonymous), manifest list resolution (multi-arch), parallel layer download, layer cache, squashfs packing. |
| `image_layer` | Decompress OCI/Docker image layers. Supports gzip, zstd, uncompressed. Detection by media-type suffix then magic bytes. |

## Mesh topology

Two modes:

- **Hub-and-spoke** (default `spur net join`): each worker peers only the controller. `AllowedIPs` covers the whole mesh CIDR. Sufficient for control-plane traffic.
- **Full mesh** (`spur net mesh`): every node peers every other node directly. Required when a native-routing CNI (Calico bird/BGP, no VXLAN) runs on top -- pod packets must flow worker-to-worker without hairpinning through the hub.

Key types:
- `MeshNode` -- hostname, public key, mesh IP, underlay endpoint, optional pod CIDR.
- `MeshMembership` -- JSON document of all mesh nodes.

`mesh_peers_for(self_ip, members)` computes the peer set for a node (excludes self). Each peer's `AllowedIPs` = mesh IP `/32` + pod CIDR (when set).

## Mesh application modes

- `apply_mesh` -- additive: adds/updates peers via `wg set`, never prunes departed nodes.
- `apply_mesh_durable` -- same + persists peers to the config file (survives interface reload/reboot). Uses `with_config_lock` for concurrent-safe read-modify-write.
- `reconcile_mesh` -- convergent: prunes peers no longer in membership (via `peers_to_prune`), then adds desired peers. Protected peers (from the node's own persisted config) are exempt from pruning.

`program_routes` flag: when true, installs kernel routes (`ip route replace <cidr> dev <iface>`) for pod CIDRs. Leave false when a CNI owns the FIB routes.

## Key exchange and tunnel management

`wireguard` module shells out to `wg` and `wg-quick` (standard on Linux 5.6+).

- `generate_keypair()` -- `wg genkey` | `wg pubkey`, returns base64 keypair.
- `WgConfig` -- parse/write wg-quick INI format. Preserves unmodeled directives (`PostUp`, `MTU`, `PresharedKey`, etc.) through read-modify-write cycles.
- `upsert_peer` -- add or update by public key. Keeps existing endpoint/keepalive/preshared-key when caller leaves them unset (matches `wg set` semantics).
- `add_peer_durable` / `remove_peer_durable` -- persist-then-apply under advisory file lock (`<config>.lock`). Persist-first so a failed live apply still converges on next reload.
- Config writes are crash-safe: write to temp file (mode 0600), fsync, rename, fsync directory.

## NAT traversal

Persistent keepalive (default 25 seconds) on every mesh peer. This keeps NAT mappings alive so peers behind NAT/firewall remain reachable. Set via `WgPeer.persistent_keepalive`.

Endpoint discovery: `peer_endpoints(interface)` parses `wg show <iface> endpoints` to map pubkeys to observed underlay endpoints. The controller re-advertises these in mesh membership so workers can reach each other even when their public endpoints change.

## Network address detection

`detect_node_address(hostname, port, wg_interface)`:

1. Check WireGuard interface (`ip addr show <iface>`, parse `inet` line) -- returns `AddressSource::WireGuardDetected`.
2. Fall back to hostname resolution via `normalize_comm_address` -- returns `AddressSource::Static`.
3. Last resort: `127.0.0.1`.

`AddressPool` manages CIDR-based allocation:
- `new("10.44.0.0/16")` -- prefix 1-30, reserves `.0`.
- `allocate()` -- next available IP starting from `.1`.
- `allocate_specific(ip)` -- claim a specific address (e.g., controller claiming `.1`).
- `mark_allocated(ip)` -- restore state from persistence.

## Comm address validation

`comm_addr` module ensures inter-node addresses are routable:
- `normalize_comm_address` -- resolve hostname to IP (blocking DNS via `ToSocketAddrs`), prefer non-loopback results.
- `validate_comm_address(input, reject_loopback)` -- normalize + optionally reject loopback/unspecified/link-local.
- `format_comm_socket` / `format_comm_http_url` -- produce `host:port` / `http://host:port` strings, bracketing IPv6 literals.

## OCI image support

`pull_image(image, output_dir)` end-to-end:

1. Parse image ref: `ubuntu:22.04` -> `docker.io/library/ubuntu:22.04`. Handles `docker://` prefix, bare names, custom registries, `localhost:port`.
2. Authenticate: Docker Hub token auth, basic auth (env `SPUR_REGISTRY_USER`/`SPUR_REGISTRY_PASSWORD`, netrc file `~/.config/spur/credentials`, Docker `~/.docker/config.json`), or anonymous.
3. Fetch manifest (accepts OCI + Docker v2 media types). Resolve manifest list/image index to `linux/amd64`.
4. Download layers in parallel (tokio tasks). Content-addressed layer cache at `<output>/.layers/` (override via `SPUR_IMAGE_CACHE`).
5. Extract layers sequentially (order matters for whiteout files). Supports gzip, zstd, uncompressed tar. Handles `.wh.*` whiteout entries.
6. Record OCI config (`config.Env`) inside rootfs at `etc/spur/image-config.json`. Symlink-safe path traversal, capped read (1 MiB), FIFO-safe.
7. Pack rootfs into squashfs via `mksquashfs -comp zstd`.

`container_base_env(rootfs, job_env)` at launch: image env seeded first, job env layered on top. `PATH` and `LD_LIBRARY_PATH` are unioned (image entries first) rather than replaced.

Equivalent image references (`busybox`, `busybox:latest`, `docker://busybox`, `docker.io/library/busybox:latest`) all normalize to the same canonical name and filename stem.

## Dependencies

Runtime (shells out): `wg`, `wg-quick`, `ip`, `mksquashfs`.
Rust: tokio, reqwest, serde/serde_json, flate2, tar, zstd, bytes, base64, anyhow, thiserror, tracing.

## Testing

```bash
cargo test -p spur-net
```

All WireGuard config parsing, mesh peer computation, address allocation, comm address validation, image ref parsing, layer decompression, and config persistence are covered by unit tests that do not require `wg`/`ip` binaries. Tests that shell out are integration-level and need a WireGuard-capable host.
