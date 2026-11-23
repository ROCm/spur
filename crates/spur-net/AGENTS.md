# AGENTS.md -- spur-net

WireGuard mesh, node address detection, and the native OCI image puller. Topology, peer lifecycle,
config locking, pod-CIDR routing, and NAT traversal are in `docs/knowledge/networking/wireguard-mesh.md`.
The image import pipeline and `container_base_env` are in `docs/knowledge/execution/containers-k0s.md`.

## Invariants

- `WgConfig::upsert_peer` keeps the existing endpoint, keepalive, and unmodeled directives when the
  caller leaves them unset, matching `wg set` semantics. Overwriting with `None` would drop, on the
  next reload, an endpoint the live interface still has.
- `apply_mesh` and `apply_mesh_durable` are additive and never prune. Only `reconcile_mesh` prunes
  (via `peers_to_prune`), and keys in `protected` (the node's own persisted config) are never pruned,
  whatever membership was pushed.
- Derive on-disk image names from `image_file_stem`, never from the raw input string. Equivalent
  references (`busybox`, `busybox:latest`, `docker://busybox`, `docker.io/library/busybox:latest`)
  must map to one stored image.

## Pitfalls

- `detect_node_address` never rejects loopback. A loopback hostname resolution only warns, and a
  failed resolution falls back to `127.0.0.1`. Rejection is the caller's job via
  `validate_comm_address(.., reject_loopback)` (driven by `reject_loopback_comm_addr`, default off).
- Manifest lists resolve to `linux/amd64` only; other platforms fail the pull.
