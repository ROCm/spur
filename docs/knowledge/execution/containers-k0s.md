# Container Runtime and k0s Cluster Management

Spur ships a native container runtime and an embedded k0s Kubernetes layer.
The container runtime needs no external daemon (no Docker, no containerd).
The k0s integration is controller-driven and agent-supervised, with the k0s
process owned by systemd rather than by the agent.

## 1. Native container runtime

The runtime lives in `spurd/src/container.rs`. It is inspired by Enroot:
squashfs images, Linux namespaces, pivot_root, bind-mount GPU devices. No
container daemon runs on the node; the agent forks the workload directly and
sets up isolation in the child process between fork and exec.

### Image format

Images are squashfs files (`.sqsh`), the same format Enroot uses. OCI and
Docker images are converted to squashfs at import time via `spur image import`,
which pulls from a registry and calls `mksquashfs` (zstd compression).

### Image search path

When a job references an image by name, the agent searches these directories
in order:

1. `$SPUR_IMAGE_DIR` environment variable (if set and non-empty).
2. `/var/spool/spur/images` (system default, `DEFAULT_IMAGE_DIR`).
3. `~<job_user>/.spur/images/` (the submitting user's personal store).
4. `~<agent>/.spur/images/` (the agent process's own home).

A `docker://` URI prefix is stripped. Equivalent references
(`busybox`, `busybox:latest`, `docker://busybox`,
`docker.io/library/busybox:latest`) all normalize to the same canonical
filename stem, so an image imported under one form is found under any other.

### Rootfs setup

`setup_rootfs` prepares a writable root filesystem from the squashfs image
under `/var/spool/spur/containers/` (or `$SPUR_CONTAINER_DIR`). Two modes:

**Overlay (preferred, root only, unnamed containers).** The squashfs is
loop-mounted read-only as `lower/`, a tmpfs is mounted as `upper/`, and
overlayfs merges them into `merged/`. No disk copy; fast.

**Extraction (fallback).** `unsquashfs` extracts the image into the rootfs
directory. Used when running without root or for named (persistent) containers.

Named containers reuse an existing rootfs directory across jobs. Unnamed
containers are cleaned up after the job exits (unmount for overlay, `rm -rf`
for extracted).

## 2. Image import and pulling (OCI registry support)

`spur-net/src/oci.rs` implements a native OCI image puller. It speaks the
Docker Registry HTTP API v2 directly -- no dependency on Docker, skopeo,
umoci, or enroot.

### Pull flow

1. Parse the image reference into `(registry, repository, tag)`. Docker Hub
   official images (`ubuntu`) expand to `docker.io/library/ubuntu:latest`.
2. Authenticate. Docker Hub uses token auth; other registries use basic auth
   from credentials or anonymous access.
3. Fetch the manifest. Multi-arch manifest lists are resolved to the
   `linux/amd64` platform manifest.
4. Download layer blobs in parallel; extract them sequentially (layer order
   matters for whiteout processing).
5. Fetch the image config blob and record it inside the rootfs at
   `etc/spur/image-config.json` for environment seeding at launch.
6. Pack the assembled rootfs into squashfs via `mksquashfs -comp zstd`.

### Layer caching

Downloaded layers are cached content-addressed in a `.layers/` directory
beside the image store (overridable via `$SPUR_IMAGE_CACHE`). Cache failures
degrade to re-download; they never fail a pull.

### Registry credentials

Credentials are loaded from, in order:

1. `SPUR_REGISTRY_USER` + `SPUR_REGISTRY_PASSWORD` environment variables.
2. `~/.config/spur/credentials` (netrc format: `machine <registry> login
   <user> password <pass>`).
3. `~/.docker/config.json` (Docker-compatible base64-encoded `auths`).

### Layer formats

The puller handles uncompressed tar, gzip-compressed tar, and
zstd-compressed tar layers (detected from the OCI media type).

## 3. Container isolation

Container setup runs in the child process after `fork()`, before `exec()`,
in `container_init()`.

### Namespace setup

**Root (production).** `unshare(CLONE_NEWNS | CLONE_NEWPID)` creates mount
and PID namespaces. No user namespace -- the agent is already root and drops
privilege after pivot_root.

**Rootless (non-root agent).** `unshare(CLONE_NEWUSER | CLONE_NEWNS |
CLONE_NEWPID)` creates user, mount, and PID namespaces. The calling user
is mapped to uid 0 inside the namespace via `/proc/self/uid_map`, giving
CAP_SYS_ADMIN for mounts and pivot_root. Supplementary groups are inherited
by writing `"deny"` to `/proc/self/setgroups` (the crun/podman keep-groups
approach); the kernel still honours them for device-node permission checks.

### Mount propagation

All mounts are set to private (`MS_REC | MS_PRIVATE` on `/`) so mount/unmount
events do not propagate between the container and host.

### PID namespace entry

After unshare, the child forks once more (`fork_into_pid_namespace`). The
grandchild becomes PID 1 inside the new PID namespace. The intermediate child
waits for the grandchild and exits with its code.

### Filesystem mounts

`mount_filesystems` sets up the standard container filesystem inside the
rootfs:

- `/proc` -- procfs (noexec, nosuid, nodev).
- `/sys` -- sysfs (read-only).
- `/dev` -- tmpfs (mode 755), with essential device nodes (`null`, `zero`,
  `full`, `random`, `urandom`, `tty`) bind-mounted from the host.
- `/dev/pts` -- devpts (newinstance).
- `/dev/shm` -- tmpfs (mode 1777, 50% of RAM).
- `/dev/fd`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr` -- symlinks into
  `/proc/self/fd`.
- `/run` -- tmpfs.

### pivot_root

`pivot_into_rootfs` replaces the process's root filesystem:

1. Bind-mount the rootfs onto itself (required by `pivot_root`).
2. `pivot_root(".", ".")` swaps old and new roots.
3. `umount2(".", MNT_DETACH)` detaches the old root, preventing escape via
   open file descriptors (stronger than chroot).
4. `chdir` to the configured workdir (or `/`).

### Privilege drop

After pivot_root, if the agent is root, privileges are dropped:
`setgroups` (supplementary GIDs including those from the device registry),
then `setgid`, then `setuid`. The ordering is mandatory -- each step requires
the privilege dropped by the next.

### User identity

`setup_shadow` appends the job user to the container's `/etc/passwd` and
`/etc/group` (idempotent) and creates the home directory inside the rootfs.

### DNS

`mount_dns` injects DNS configuration into the container. Loopback
nameservers (127.x, ::1) are stripped from `resolv.conf` since local stub
resolvers (systemd-resolved, dnsmasq) are unreachable after pivot_root.
If all nameservers are loopback, it tries
`/run/systemd/resolve/resolv.conf`, then falls back to `1.1.1.1` / `8.8.8.8`.
User-mounted `/etc/resolv.conf` is left alone.

### File descriptor hygiene

`close_inherited_fds` closes all file descriptors above stderr except one
preserved sync pipe, preventing gRPC sockets and other jobs' output files
from leaking into the container process.

## 4. GPU passthrough in containers

GPU devices are injected via the `ContainerConfig.device_plan` (a
`ContainerInjectionPlan` from the `spur-devices` crate).
`apply_container_device_plan` bind-mounts each device and library path into
the container rootfs.

### AMD GPUs

- `/dev/kfd` -- KFD compute interface (bind-mounted).
- `/dev/dri/renderD<N>` -- only the allocated render nodes, not all (bind-mounted).
- ROCm userspace libraries from the host (bind-mounted via the injection plan's
  mount list).

### NVIDIA GPUs

- `/dev/nvidia*` device nodes (bind-mounted).
- Driver libraries (bind-mounted or via libnvidia-container).

### Additional GIDs

The device plan may include `additional_gids` (e.g. the `video` and `render`
groups). These are merged into the process's supplementary groups before
privilege drop so device-node permissions are satisfied.

## 5. Container environment

At import time, the OCI image config blob is fetched and recorded inside the
rootfs at `etc/spur/image-config.json`. At launch, `container_base_env`
reads this file and layers the job environment on top.

### Layering rules

- For most variables, the job's value replaces the image's (same as
  `docker run -e`).
- `PATH` and `LD_LIBRARY_PATH` are unioned rather than replaced: the image's
  entries come first, then the job's entries are appended, with duplicates
  removed. This keeps executables shipped in the image reachable when the job
  exports a host `PATH` (which it does by default under `--export ALL`).
- Images imported before the config-recording feature was added have no
  `etc/spur/image-config.json`; the job environment is passed unchanged.

### Safety

- The config file is capped at 1 MiB (`MAX_IMAGE_CONFIG_BYTES`) to limit
  memory consumption from an untrusted image.
- Symlink traversal through `etc/spur/image-config.json` is refused to
  prevent redirecting the read outside the rootfs.
- A FIFO at the config path is detected and skipped (opening a FIFO blocks
  until a writer appears, which would stall the launch).

## 6. Container hooks

`run_hooks` processes admin-configured hooks and environment files:

- `/etc/spur/container.d/hooks.d/*.sh` -- shell scripts run with
  `ENROOT_ROOTFS` and `SPUR_HOOK_ENVIRON` (also `ENROOT_ENVIRON` for
  compatibility). Hooks can inject environment variables by appending
  `KEY=VALUE` lines to `$SPUR_HOOK_ENVIRON`.
- `/etc/spur/container.d/environ.d/*.env` -- static `KEY=VALUE` files
  parsed and added to the environment.
- `/etc/spur/container.d/mounts.d/*.fstab` -- fstab-style `src dst` lines,
  bind-mounted into the rootfs.

All directories are sorted lexicographically, so ordering is deterministic.

## 7. k0s cluster management -- controller side

The controller-side reconcile loop lives in
`spurctld/src/cluster_k8s.rs`. It is leader-gated (only the Raft leader
runs it) and ticks every 30 seconds.

### Phase model

The k0s cluster state machine has four phases, persisted via Raft WAL:

| Phase | Meaning |
|---|---|
| `Down` | No cluster. All components stopped. |
| `Provisioning` | Assigning roles, starting components. |
| `Ready` | All assigned nodes are active. |
| `Degraded` | Provisioning timed out before all nodes became active. |

Ready and Provisioning both run the same assignment + converge logic so
the cluster self-heals: a node removed and re-added, or added while Ready,
gets re-assigned and re-joined on the next tick.

### Role and IP assignment (`provision_assignments`)

Each unassigned member node receives:

- **Role:** Controller (from the HA set or bootstrap node), Worker, or
  Single (1-node cluster).
- **Mesh IP:** Allocated from the WireGuard mesh CIDR (`network.wg_cidr`).
  The bootstrap controller always gets `.1`. A node's real mesh address
  (its `spur0` interface IP) is adopted rather than re-derived, preventing
  corruption when control-plane set membership does not match alphabetical
  join order.
- **Pod /24:** Carved from the pod CIDR (`cluster.pod_cidr`).

Assignments are persisted per node in the cluster state. Already-assigned
nodes are skipped (idempotent).

### Convergence (`converge_provisioning`)

For each assigned node, the controller:

1. Probes the node's component status via `GetClusterComponentStatus` RPC.
2. If the node has no active component, mints a join token (cached per-role)
   and sends `StartClusterComponentRequest` with role, token, k0s config,
   and the node's mesh IP.
3. When all members report `active`, transitions the phase to Ready.

### Provisioning timeout

A leader-local clock tracks how long the cluster stays in Provisioning.
If `cluster.k8s_provisioning_timeout_secs` elapses, the cluster is marked
Degraded.

### Mesh membership push

When the WireGuard mesh is enabled and at least two nodes are meshed, the
controller builds a full-mesh `MeshMembership` (pubkey + mesh IP + pod /24
for every node) and pushes it to every agent via `ApplyMesh` on every tick.
This is level-triggered: the agent's `reconcile_mesh` is idempotent and
prunes stale peers, so drift from reboots, failed pushes, or controller
failover self-heals.

### Teardown

On `Down`, the controller sends `StopClusterComponent` to every node.
If `reset_requested` is set, agents also run `k0s reset` (destructive
host-wide cleanup). Cached join tokens are dropped since they were minted
against the now-torn-down CA.

## 8. k0s agent side -- systemd unit and ClusterSupervisor

The agent-side k0s integration lives in `spurd/src/cluster.rs`.

### Design: systemd-owned, not agent-owned

k0s runs as its own systemd unit, not as a child process of spurd. This is
deliberate:

- k0s survives a spurd restart (its containerd/kubelet/pod cgroup is not
  orphaned when spurd exits).
- k0s stays out of the executor/monitor/time-limit job path, which would
  otherwise kill or reap it.

### ClusterSupervisor

`ClusterSupervisor` owns and reconciles a single spurd-managed k0s systemd
unit. It renders a unit file with these directives:

| Directive | Value | Rationale |
|---|---|---|
| `Type` | `simple` (default) | k0s does not call `sd_notify`; `Type=notify` would hang in `activating`. |
| `KillMode` | `process` | Leaves the delegated containerd/kubelet/pod cgroup alive on stop (unlike `mixed` which would SIGKILL it). |
| `Delegate` | `yes` | Required for k0s's containerd to manage its own cgroups. |
| `Restart` | `always` | systemd re-spawns on process crash. |
| `RestartSec` | `10` | Delay between restarts. |
| `StartLimitIntervalSec` | `300` | In `[Unit]`, because systemd ignores it in `[Service]`. 5 starts in 5 minutes before systemd stops retrying. |
| `StartLimitBurst` | `5` | systemd also refuses the reconcile loop's `systemctl start` until the 5 minutes end. A later tick starts the unit again, adding backoff without permanent lockout. |

The unit is tagged with `"(managed by spurd)"` in its `Description` field.
This marker is the only way to distinguish a spurd-owned k0s unit from an
operator-installed one (k0s's own `k0s install` uses the same unit names).

### Reconcile loop

`ClusterSupervisor::reconcile_once` runs every 30 seconds (from
`K0sAgent::supervise`). Each tick:

1. Checks if the unit file exists and matches the desired content. If not,
   writes it atomically (temp + rename) and runs `daemon-reload`.
2. Checks if the unit is enabled. If not, enables it.
3. Checks if the unit is active. If not, starts it.

This covers situations `Restart=always` does not: an out-of-band
`systemctl disable` or `systemctl stop`, a deleted unit file, or a config
drift.

### K0sAgent

`K0sAgent` is the RPC-driven owner of k0s on a node:

- **`start()`:** Auto-installs the k0s binary if missing (from a configured
  version). Writes the join token (0600) and k0s config to `/etc/k0s/`.
  Writes the CDI spec (worker/single nodes). Ships the local-path storage
  manifest (controller/single nodes). Renders the systemd unit, enables,
  starts.
- **`stop()`:** Disables the unit. Optionally runs `k0s reset` + purges the
  unit file and join token.
- **`adopt_running_unit()`:** On spurd restart, probes for an active
  spurd-owned k0s unit, parses its `ExecStart` line back into a
  `ClusterConfig`, and rebuilds the supervisor. This re-adoption ensures
  the reconcile loop continues healing the unit rather than reporting a
  false `inactive`.
- **`status()`:** Returns the component's role, systemd active-state,
  enabled flag, and whether the unit is spurd-owned.

### Roles

| Role | systemd unit | k0s subcommand |
|---|---|---|
| Controller | `k0scontroller.service` | `k0s controller` |
| Worker | `k0sworker.service` | `k0s worker` |
| Single | `k0scontroller.service` | `k0s controller --single=true` |

### k0s node operations

`K0sAgent` exposes operations that the controller invokes via RPC:

- `create_join_token` -- runs `k0s token create --role <role>` on a
  control-plane node.
- `admin_kubeconfig` -- runs `k0s kubeconfig admin`.
- `user_kubeconfig` -- creates a ServiceAccount in a namespace, mints a
  bound token, templates a namespace-scoped kubeconfig with the cluster CA.
- `drain_node` -- cordons then drains a k8s node (PDB-aware, skips
  DaemonSets).
- `delete_node` -- removes a k8s Node object after the component is stopped
  (so the kubelet cannot re-register).

## 9. WireGuard mesh interaction with k0s

The k0s cluster can ride the WireGuard mesh. When `network.wg_enabled` is
true and the CNI is `calico`, the controller sets `mesh_native` mode: pod
traffic routes natively over the WireGuard mesh rather than through a
separate overlay.

Worker kubelet `--node-ip` is pinned to the node's mesh IP (via
`--kubelet-extra-args=--node-ip=<mesh_ip>` in the systemd unit's
`ExecStart`). This ensures a native-routing CNI (Calico bird mode) peers
over the mesh rather than the underlay.

The controller pushes the full mesh membership to every agent on every
reconcile tick. Each agent runs `reconcile_mesh` to converge its WireGuard
peer set: adding new peers, updating endpoints, and pruning departed nodes.

## 10. CDI spec generation for GPU pods

Before starting a worker or single k0s node, the agent writes an AMD GPU
CDI (Container Device Interface) spec to `/etc/cdi/amd.json`. This lets
k0s's containerd inject `amd.com/gpu` devices into pods.

### Discovery

`spur_devices::cdi::discovery::discover_to_cdi` scans the KFD topology
(`/sys/class/kfd/kfd/topology/nodes/`) for AMD GPU nodes. For each GPU it
records:

- Render node device path (`/dev/dri/renderD<N>`).
- KFD device path (`/dev/kfd`).
- Topology metadata (device ID, location, VRAM, hive ID, XGMI links).
- Supplementary groups (`video`, `render`).

### Spec format

The CDI spec follows the CDI v0.6.0 schema:

- Kind: `amd.com/gpu`.
- One `CdiDevice` per GPU, annotated with a stable ID.
- Shared container edits for `/dev/kfd` (common across all GPUs).
- Annotation `auto-detected: "true"` marks it as Spur-generated.

### Ownership and staleness

`cdi_owner.rs` ensures Spur only overwrites its own specs (identified by the
`auto-detected` annotation). Foreign CDI specs are left alone. A spec is
considered stale when the set of GPU stable IDs on disk differs from a live
discovery scan; an empty live scan never makes an existing spec look stale
(guards against transient KFD loader failures).

### Spec writes

Specs are written atomically (temp file + rename) to `/etc/cdi/`. Validation
runs before the write. The spec is removed on k0s component teardown.

### Storage provisioner

k0s bundles no storage. When the configured `storage_provisioner` is
`"local-path"`, the controller node ships a local-path provisioner manifest
to k0s's manifest-deployer directory
(`/var/lib/k0s/manifests/local-path/local-path.yaml`). k0s's built-in
manifest deployer applies and reconciles it, providing PVC support out of
the box.

## Common pitfalls

**Changing the spurd unit marker.** The string `"(managed by spurd)"` in the
unit Description is the identity tag. Changing it would disown all existing
units -- they would be treated as foreign and never healed.

**ExecStart round-trip.** The adopt path parses the rendered `ExecStart`
back into a `ClusterConfig` and re-renders it. If the round-trip is not
byte-identical, reconcile rewrites the unit file every tick (30 s churn).

**k0s reset is destructive.** `k0s reset` wipes etcd, certificates, and
containerd state host-wide. It is only run after confirming the unit is
stopped and the node is drained. A join token minted against a torn-down CA
must be purged even if reset fails.

**squashfs-tools required.** Both `mksquashfs` (image import) and
`unsquashfs` (rootfs extraction fallback) come from the `squashfs-tools`
package. Missing tools produce clear error messages with install instructions.

**Loopback DNS.** Containers that pivot_root lose access to host stub
resolvers on 127.0.0.53. The runtime detects and strips loopback nameservers,
falling back to public DNS if nothing usable remains.

**Root vs rootless namespace paths.** The container runtime has two distinct
namespace paths. Root: `CLONE_NEWNS | CLONE_NEWPID` (no user namespace).
Rootless: `CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID` (user namespace maps
the calling user to uid 0 inside). Mixing them up breaks privilege drop.
