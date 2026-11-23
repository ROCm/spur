# Container Runtime and k0s Cluster Management

Spur ships a native container runtime (no Docker or containerd on the node) and
a controller-driven k0s layer whose k0s process is owned by systemd, not by
`spurd`. User-facing usage is in `docs/user-guide/running-containers.rst` and
`docs/deployment/managed-kubernetes.rst`. The `container_init` step order is
`container_init` in `crates/spurd/src/container.rs`; the image search path is
`compose_image_dirs`. Container device plans are in
`docs/knowledge/devices/gpu-discovery.md`.

## Container runtime

### Rootfs

- `setup_rootfs` loop-mounts the squashfs read-only under a tmpfs overlay (no
  disk copy) only for a root agent and an unnamed container. A non-root agent,
  a named container, or an overlay failure falls back to `unsquashfs`
  extraction.
- Named containers reuse an existing rootfs directory across jobs. Unnamed
  ones are torn down after the job (`cleanup_rootfs`).
- `mksquashfs` (import) and `unsquashfs` (extraction) both come from
  `squashfs-tools`; a missing tool fails with an install hint.

### Isolation

- Root agent: `unshare(CLONE_NEWNS | CLONE_NEWPID)`, no user namespace, and
  privileges are dropped after `pivot_root`. Rootless agent: adds
  `CLONE_NEWUSER`, mapping the calling user to uid 0 for mount and
  `pivot_root` capability. Mixing up the two paths breaks privilege drop.
- Rootless keeps supplementary groups by writing `deny` to
  `/proc/self/setgroups` (the crun/podman keep-groups approach); the kernel
  still honours them for device-node permission checks.
- Device-plan `additional_gids` (e.g. `video`, `render`) are merged into the
  supplementary groups before the privilege drop so device nodes stay usable.
- `pivot_into_rootfs` detaches the old root (`MNT_DETACH`) instead of
  chrooting, so no host path stays reachable. Inherited fds are closed
  separately (next bullet).
- `close_inherited_fds` closes everything above stderr except the sync pipe,
  so gRPC sockets and other jobs' output files never leak into the container.

### The image is untrusted

The rootfs is the user's image, set up by root before `pivot_root`, so every
path into it goes through open directory handles (`scratch.rs`):

- `/dev`, `/proc`, `/sys` and `/run` must be real directories; a link at any
  of them refuses the container.
- A bind-mount destination (`--container-mounts`, the home mount, device plans,
  `mounts.d`) follows the image's own links as the container will see them: an
  absolute link restarts at the rootfs and `..` stops there
  (`open_mount_point`), so `/lib -> usr/lib` keeps working and nothing leads
  onto the host.
- Every mount targets the handle it was checked through, as
  `/proc/self/fd/<n>`, so renaming or linking along the path afterwards cannot
  redirect it. The fresh `/dev` tmpfs is reopened, checked to be a different
  filesystem from the rootfs, and filled through that handle.
- `setup_shadow` rewrites and renames `/etc/passwd` and `/etc/group` rather
  than appending, and refuses a user name or home directory containing `:` or
  a control character, since either would let the job write account lines of
  its choosing.
- `mount_dns` avoids loopback nameservers, since host stub resolvers are
  unreachable after `pivot_root`. If the host `resolv.conf` names one, it uses
  `/run/systemd/resolve/resolv.conf` when that has none; otherwise it strips
  loopback lines, falling back to public DNS only if nothing remains or the
  host file is unreadable or empty. A user-mounted `/etc/resolv.conf` is left
  alone.

### Image import (`spur-net` `oci.rs`)

- Layers download in parallel but extract sequentially in manifest order,
  because whiteout processing depends on order.
- The layer cache (`.layers/` beside the image store, or `$SPUR_IMAGE_CACHE`)
  is best-effort: a cache failure re-downloads and never fails a pull.
- The image config blob is recorded at `IMAGE_CONFIG_PATH`
  (`etc/spur/image-config.json`) inside the rootfs before it is packed with
  `mksquashfs -comp zstd`.

### Container environment (`container_base_env`)

- The image's `config.Env` is the base and job values replace it, except
  `PATH` and `LD_LIBRARY_PATH` (`UNIONED_VARS`), which are unioned image-first
  with duplicates dropped. This keeps image executables reachable when the job
  exports a host `PATH` (the default under `--export ALL`).
- Images imported before config recording have no file, so the job
  environment passes through unchanged.
- The config is user-supplied content read as root: it is capped at
  `MAX_IMAGE_CONFIG_BYTES`, a symlinked path component is refused (on the
  import write and the launch read), and only a regular file is opened, since
  opening a FIFO blocks until a writer appears and would stall the launch.

### Admin hooks (`run_hooks`)

- `/etc/spur/container.d/` holds `hooks.d/*.sh`, `environ.d/*.env` and
  `mounts.d/*.fstab`, each processed in lexicographic order.
- Hooks get `ENROOT_ROOTFS` and `SPUR_HOOK_ENVIRON` (also `ENROOT_ENVIRON`)
  and inject variables by appending `KEY=VALUE` lines to it. That file is a
  fresh root-owned file in the node's temp directory, not the rootfs (where
  the job's user could swap it for a link), and is removed once read.

## k0s controller side (`spurctld` `cluster_k8s`)

The reconcile loop runs on the Raft leader only. Phase transitions are
replicated; the provisioning clock is leader-local (see
`crates/spurctld/AGENTS.md`).

- Provisioning and Ready both run `provision_assignments` and
  `converge_provisioning`, so a node removed and re-added, or added while
  Ready, is re-assigned and re-joined on the next tick. Assigned nodes are
  skipped: the persisted `k0s_role` / `k0s_mesh_ip` / `k0s_pod_cidr` is the
  allocation record.
- Mesh IPs come from `network.wg_cidr`, with `.1` reserved for the bootstrap
  controller. A meshed node's real `spur0` address is adopted rather than
  re-derived; re-deriving corrupts the mesh when the control-plane set does
  not match alphabetical join order. Each node also gets a pod /24 carved from
  `cluster.pod_cidr`.
- A node with no active component gets `StartClusterComponent` with its role,
  k0s config, mesh IP and join token. Join tokens are cached per node until it
  joins.
- Ready is reached once a quorum of the control-plane set is active; workers
  keep converging afterwards. At `cluster.k8s_provisioning_timeout_secs` a
  cluster with that quorum still goes Ready; without it the cluster goes
  Degraded and half-started units are stopped without reset.
- With WireGuard enabled and at least two meshed nodes, the full
  `MeshMembership` is pushed via `ApplyMesh` to every meshed node on every
  tick. This is level-triggered on purpose: the agent's `reconcile_mesh` is
  idempotent and prunes, so reboots, failed pushes and controller failover
  self-heal. Mesh topology and the mesh-native routing rule are in
  `docs/knowledge/networking/wireguard-mesh.md`.
- Under mesh-native routing, a joining node's kubelet `--node-ip` is pinned to
  its mesh IP in the unit's `ExecStart`, so a native-routing CNI (Calico bird
  mode) peers over the mesh rather than the underlay.
- On Down, cached join tokens are dropped, since they were minted against the
  CA a rebuild regenerates. Nodes whose component still runs get
  `StopClusterComponent` (with `k0s reset` when `reset_requested` is set).
- Worker removal drains, then stops and resets, then deletes the k8s Node
  object. Deleting the Node while the kubelet still runs lets it re-register.

## k0s agent side (`spurd` `cluster.rs`)

### Why systemd owns k0s

- k0s runs as its own systemd unit, not a `spurd` child, so it survives a
  `spurd` restart (its delegated containerd/kubelet/pod cgroup is not
  orphaned) and stays out of the executor, monitor and time-limit job path,
  which would otherwise reap or kill it.
- `render_unit` mirrors k0s's own installer: `Type=simple` (k0s does not
  `sd_notify`, so `Type=notify` would hang in `activating`), `KillMode=process`
  (stop leaves the delegated cgroup alive; `mixed` would SIGKILL it),
  `Delegate=yes` and `Restart=always`. A finite `StartLimitBurst` bounds a
  crash loop; the reconcile loop restarts the unit on its next tick, so the
  limit adds backoff without a permanent lockout.
- Controller and Single share `k0scontroller.service`; `--single=true` in
  `ExecStart` tells them apart.

### Supervision

- `ClusterSupervisor::reconcile_once` (each `K0sAgent::supervise` tick)
  rewrites the unit file (temp + rename, then `daemon-reload`) when its content
  differs, then enables and starts the unit. This covers what `Restart=always`
  does not: an out-of-band `systemctl disable` or `stop`, a deleted unit file,
  or config drift.
- `adopt_running_unit` runs on `spurd` start: it parses an active spurd-owned
  unit's `ExecStart` back into a `ClusterConfig`, so supervision resumes
  instead of reporting a false `inactive`.

### Pitfalls

- `SPURD_UNIT_MARKER` (`"(managed by spurd)"` in the unit `Description`) is
  the only way to tell a spurd unit from one written by `k0s install`, which
  uses the same unit names. Changing it disowns every existing unit: they are
  treated as foreign and never healed.
- `parse_execstart` must round-trip `render_unit` byte for byte. Change both
  together, or reconcile rewrites the unit every tick.
- `k0s reset` wipes etcd, certificates and containerd state host-wide. It runs
  only after the unit is confirmed stopped. The unit file and join token are
  purged even if the reset fails, since a token minted against the torn-down
  CA fails the node's next join.

## GPU CDI spec and storage for k0s pods

- Before a worker or single node starts, the agent writes `/etc/cdi/amd.json`
  from `spur_devices::discovery::discover_gpu_cdi` so k0s's containerd can inject `amd.com/gpu` devices.
  The spec is removed on component teardown.
- `cdi_owner` only overwrites specs carrying Spur's `auto-detected`
  annotation; foreign specs are left alone. A spec is stale (`is_stale`) when
  a live scan differs in GPU stable ids or, if `/sys/class/drm` is readable, in
  the device nodes each GPU grants (so an older build's guessed card node gets
  rewritten). With DRM unreadable only ids are compared, and a rewrite keeps
  unchanged GPUs' on-disk card nodes (`keep_card_nodes`). An empty live scan
  never makes a spec stale (guards against transient KFD loader failures).
- k0s bundles no storage. With `storage_provisioner = "local-path"`,
  control-plane nodes write the provisioner manifest into k0s's
  manifest-deployer directory (`K0S_MANIFESTS_DIR`), which k0s applies and
  reconciles, giving PVC support.
