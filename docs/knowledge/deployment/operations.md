# Spur Deployment and Operations

## 1. Deployment modes

### Native host (systemd)

Install binaries via the one-line installer (default `~/.local/bin`, no root
required) or build from source:

```bash
curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash
```

Binaries: `spur`, `spurctld`, `spurd`, `spurstepd`, `spurauthd`, plus
Slurm-compatible symlinks (`sbatch`, `squeue`, `sinfo`, `scancel`, `sacct`,
`scontrol`).  For `--mpi=pmix`, use a nightly tarball (includes
`spur_mpi_pmix.so`).

Run daemons as systemd units.  Key unit requirements:

- `spurctld.service`: `Type=simple`, `Restart=on-failure`, `LimitNOFILE=65536`.
- `spurd.service`: **`KillMode=process`** (required so that job supervisors
  survive an agent restart; the systemd default `control-group` kills them).
  `LimitMEMLOCK=infinity` for RDMA/NCCL.

Ports: **6817** (controller gRPC + accounting), **6818** (agent gRPC),
**6821** (Raft controller-to-controller).

### Ansible (recommended for production)

The `spur-toolkit` repository (`ansible/` directory) automates the full
deployment: binary install, `spur.conf` rendering, systemd unit creation,
Slurm-compat symlinks, PostgreSQL accounting, and WireGuard mesh.

```bash
ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini \
  -e spur_binary_src=/path/to/target/release
```

`deploy.yml` is idempotent.  Topology is inventory-driven:

| Shape | Inventory pattern | Transport |
|---|---|---|
| Single-node | One host in both `spur_controllers` and `spur_agents` | Loopback |
| Multi-node, direct LAN | One controller, N agents | Unencrypted LAN |
| Multi-node, WireGuard | As above + `spur_transport=wireguard` | Encrypted mesh |
| HA (multi-controller) | Odd N >= 3 in `spur_controllers` | Direct or WireGuard |

Day-2 playbooks: `add_nodes.yml`, `remove_nodes.yml`, `manage_accounts.yml`,
`healthcheck.yml`, `rolling_upgrade.yml`, `teardown.yml`.

### Kubernetes (existing cluster)

Deploy Spur inside an existing K8s cluster using the manifests in
`examples/k8s/`.

```bash
kubectl apply -f examples/k8s/namespace.yaml
kubectl apply -f examples/k8s/rbac.yaml
kubectl apply -f examples/k8s/spurjob-crd.yaml
kubectl create secret generic spur-config \
  --from-file=spur.conf=examples/k8s/spur.conf -n spur
kubectl apply -f examples/k8s/spurctld.yaml
kubectl apply -f examples/k8s/spurd.yaml
kubectl apply -f examples/k8s/operator.yaml
kubectl apply -f examples/k8s/pdb.yaml
```

Components:

- **spurctld**: StatefulSet (3 replicas for HA Raft).  Raft node ID
  auto-derived from pod hostname position in `controller.peers`.
- **spurd**: DaemonSet or Deployment on compute nodes (privileged, `hostPID`).
- **spur-k8s-operator**: Watches `SpurJob` CRDs, submits to the controller.

`spurctld` and the operator run under a restricted security context (non-root
UID 10001, read-only rootfs, no capabilities, `RuntimeDefault` seccomp).

---

## 2. Binary layout

| Binary | Role | Runs on |
|---|---|---|
| `spurctld` | Controller daemon.  Serves gRPC API (port 6817), optional REST API (port 6820), Raft consensus (port 6821), OpenMetrics (port 6822).  Accounting runs in-process backed by PostgreSQL. | Controller node(s) |
| `spurd` | Node agent.  Registers with controller, sends heartbeats, launches jobs, manages cgroups/GPUs, runs k0s components. | Every compute node |
| `spurstepd` | Per-job supervisor.  Spawned by `spurd` for each job and each `srun` step.  Survives `spurd` restarts (separate process, separate trust boundary). | Compute nodes (next to `spurd`) |
| `spurauthd` | Local credential mint.  Listens on `/run/spur/<cluster>/auth.sock`; issues short-lived tokens for CLI and step processes.  Only needed for native credentials (`[auth] plugin = "spur"`). | Login and compute nodes |
| `spur` | Multi-call CLI.  Invoked as `spur <command>` or via Slurm symlinks. Talks to spurctld on port 6817. | Any host with controller access |
| `spur-k8s-operator` | Kubernetes operator.  Watches SpurJob CRDs, projects Spur accounts into K8s quota/RBAC. | Inside K8s cluster |

`install.sh` layout (with `INSTALL_DIR=$HOME/spur/bin`):

```text
~/spur/bin/      spur, spurctld, spurd, spurstepd, spurauthd, Slurm symlinks
~/spur/lib/spur/ spur_mpi_pmix.so (when shipped in the tarball)
```

`spurstepd` must be installed next to `spurd` -- the agent resolves it beside
its own executable and does not search `$PATH`.

---

## 3. Config file structure (spur.conf)

All configuration is TOML at `/etc/spur/spur.conf`, parsed by
`spur-core/src/config.rs` (`SlurmConfig`).

### Top-level sections

| Section | Purpose |
|---|---|
| `cluster_name` | Top-level string; cluster identifier. |
| `[controller]` | Listen address, state directory, HA peers, dispatch/shutdown timeouts, agent connection tuning. |
| `[scheduler]` | Plugin (`backfill`), tick interval, max jobs per cycle, fairshare halflife, preempt type, kill wait, inactive limit. |
| `[accounting]` | PostgreSQL `database_url`, default QOS, `require_qos`, refresh intervals, transaction retention. |
| `[auth]` | Plugin (`none`/`jwt`/`spur`), mode (`disabled`/`permissive`/`required`), JWT key, `private_data`, `allow_root_jobs`. |
| `[[partitions]]` | Array of partition definitions: name, state, default flag, nodes (hostlist or label selector), time limits, preempt mode, account allow/deny, priority tier, grace time. |
| `[[nodes]]` | Array of node configs: names (hostlist or selector), CPUs, memory, reserved memory, GRES, features, weight. |
| `[network]` | WireGuard enabled/interface/CIDR, agent port, loopback rejection. |
| `[logging]` | Level, format, `audit_rpcs`. |
| `[metrics]` | OpenMetrics HTTP server: enabled, listen address (default `[::]:6822`), bind policy (`loopback`/`all`), `high_cardinality`. |
| `[rest_api]` | Slurm-compat REST API on port 6820, off by default. |
| `[hooks]` | Prolog/epilog/task hooks, job_submit shell/lua scripts. All paths must be absolute. |
| `[devices]` | CDI, GRES discovery configuration. |
| `[admission]` | Node admission mode (`open`/`token`). |
| `[rlimits]` | Per-step RLIMIT_MEMLOCK (`unlimited`/`inherit`/bytes). |
| `[cgroup]` | cgroup-v2 enforcement: enabled, required, constrain_cores/ram/swap/devices, cleanup timeout, OOM group, device filter extra paths. |
| `[mpi]` | PMIx plugin directory, tmpdir, min version, modex timeouts. |
| `[health]` | Health check programs, intervals, timeout, max unavailable nodes. |
| `[cluster]` | Spur-managed k0s: enabled, distro, pod/service CIDRs, CNI, control plane, storage provisioner. |
| `[kubernetes]` | Running Spur inside an existing K8s cluster. |
| `[update]` | Auto-update checking: `check_on_startup`, `auto_update`, channel, cache dir. |
| `[isolation]` | Job isolation configuration. |
| `[power]` | Power management. |
| `[federation]` | Federation with other clusters. |
| `[topology]` | Switch hierarchy for locality-aware scheduling. |
| `[burst_buffer]` | Burst buffer capacity pool. |
| `licenses` | Cluster-wide license pool (map). |
| `[spurd]` | Agent-side controller channel/RPC timeouts. |

Runtime config changes via `scontrol reconfigure` re-read the file on the Raft
leader.  Some parameters (tick interval, max jobs per cycle, metrics bind) are
baked at startup and require a daemon restart.

---

## 4. PostgreSQL setup for accounting

Accounting is served in-process by `spurctld` on port 6817 (no separate
`slurmdbd`).  Only PostgreSQL is a separate service.

### Schema

Tables are created and migrated inline via `spurctld/src/accounting/db.rs`.
Migrations are serialized across controllers by a PostgreSQL advisory lock
(`pg_advisory_xact_lock`).

Core tables: `jobs`, `accounts`, `users`, `associations`, `qos`, `usage`,
`tres_usage`, `txn` (audit log).

### Ansible setup

```ini
[all:vars]
spur_accounting_enabled=true
spur_accounting_db_password=<strong-password>  # defaults to "spur" -- change for production
```

By default PostgreSQL runs on the first controller.  To separate it:

```ini
[spur_accounting_node]
acct-0 ansible_host=10.0.0.20 ansible_user=root
```

### Manual setup

```bash
sudo -u postgres createuser spur
sudo -u postgres createdb -O spur spur
# Allow remote TCP from controller hosts in pg_hba.conf
```

Configure in `spur.conf`:

```toml
[accounting]
database_url = "postgresql://spur:spur@localhost/spur"
```

### Resilience

A database outage does not block controller startup.  Accounting connects with
capped exponential backoff (1s to 30s).  Jobs naming a QOS or account are held
(`AccountingUnavailable`) while caches are cold; once loaded, subsequent
outages do not re-block scheduling.  A reconcile loop (120s interval on the
leader) diffs in-memory state against the database and re-issues stale writes.

---

## 5. Raft HA setup (multi-controller)

Spur uses openraft for durable, replicated cluster state.  Raft is always-on:
even a single-node deployment runs a 1-member Raft cluster.

### Configuration

Run 3 (or 5) `spurctld` instances with identically ordered `peers` lists:

```toml
[controller]
peers = [
  "10.44.0.1:6821",
  "10.44.0.2:6821",
  "10.44.0.3:6821",
]
raft_listen_addr = "[::]:6821"
```

Node ID is derived from position in the peers list (1-indexed) by matching the
local hostname.  A persisted `raft/node_id` file guards against identity shifts.

### Ports

| Port | Purpose |
|---|---|
| 6817 | Client-facing gRPC (SlurmController + SlurmAccounting) |
| 6820 | REST API (disabled by default) |
| 6821 | Raft-internal gRPC (heartbeats, log replication, snapshots) |
| 6822 | OpenMetrics HTTP |

The Raft port runs a separate gRPC server so heartbeats are never blocked by
client request load.  Message size limit is 32 MiB (`RAFT_MAX_MESSAGE_SIZE`).

### Raft timing

| Parameter | Value |
|---|---|
| `heartbeat_interval` | 500 ms |
| `election_timeout_min` | 1500 ms |
| `election_timeout_max` | 3000 ms |
| `snapshot_max_chunk_size` | 1 MiB |

### Leader forwarding

Write RPCs arriving at a follower are transparently forwarded to the leader.
Agents and CLIs accept a comma-separated controller list for automatic failover:

```bash
--controller http://10.44.0.1:6817,http://10.44.0.2:6817,http://10.44.0.3:6817
```

### Storage layout

```text
{state_dir}/raft/
  vote.json           # current Raft vote
  purged.json         # last purged log id
  snapshot.json       # full cluster snapshot
  node_id             # persisted Raft node id
  log/
    000...042.json    # WAL entries (JSON)
```

All critical files use atomic writes (write to `*.tmp`, fsync, rename, fsync
directory).  The snapshot needs free space for about two copies (current +
temporary).

### Membership

Raft membership is fixed after first init.  Adding, removing, or reordering a
controller requires a state wipe (`spur_wipe_state=true`).  Compute agents are
not Raft members and can be added/removed freely.

---

## 6. WireGuard mesh setup

### Initialization

On the bootstrap controller:

```bash
sudo spur net init --cidr 10.44.0.0/16 --port 51820 --interface spur0
```

On each other node:

```bash
sudo spur net join \
    --endpoint <bootstrap-underlay>:51820 \
    --server-key <bootstrap-pubkey> \
    --address 10.44.0.N
```

Then register each joiner on the controller **with its underlay endpoint** so
worker-to-worker tunnels can form:

```bash
sudo spur net add-peer \
    --key <joiner-pubkey> \
    --allowed-ip 10.44.0.N/32 \
    --endpoint <joiner-underlay>:51820
```

### Topology

- **Hub-and-spoke** (default without k0s): worker-to-controller only.
  Worker-to-worker packets hairpin through the hub.
- **Full mesh**: applied either by the k0s reconciler's `ApplyMesh` push
  (when `[cluster].enabled = true`) or by a manual `spur net mesh` pass.
  Required when a CNI runs in native-routing mode.

### Boot persistence

Enable the `wg-quick@spur0` systemd unit so the interface survives reboot:

```bash
sudo systemctl enable wg-quick@spur0
```

The Ansible toolkit does this when `spur_wg_persist=true` (the default).

### HA over the mesh

All controllers join the same mesh.  Use mesh IPs (not underlay) in
`controller.peers`.  The full-mesh pass must wire controller-to-controller
tunnels for Raft elections.

### Removing a node

Deregister from the cluster **first** (`spur node remove <node>`), then drop
the mesh peer (`spur net remove-peer`).  In HA, remove the peer on every
controller.

---

## 7. K8s operator deployment

### Spur inside existing K8s

The `spur-k8s-operator` watches `SpurJob` CRDs (API group
`spur.amd.com/v1alpha1`) and submits them to the controller.

```yaml
apiVersion: spur.amd.com/v1alpha1
kind: SpurJob
metadata:
  name: training-run
spec:
  script: |
    #!/bin/bash
    #SBATCH --job-name=train
    #SBATCH -N 2
    #SBATCH --gres=gpu:8
    torchrun --nnodes=2 train.py
```

The operator also:

- **Node watcher**: syncs K8s node status with Spur node state (virtual
  agents -- K8s nodes appear as Spur compute nodes without running `spurd`).
- **Quota controller**: with `--enable-quota`, projects each Spur account's
  `GrpTRES` into a per-account `ResourceQuota`/`LimitRange`.

Authentication: `--auth-mode` (`disabled`/`permissive`/`required`) with
`--jwt-key` for the cluster signing key.

The operator's RBAC is scoped to its own `spur-operator` ServiceAccount.
`spurctld` and `spurd` pods do not use a pod token.

### Spur-managed k0s (native cluster)

Enabled via `[cluster].enabled = true`.  The controller provisions and owns a
k0s cluster across its nodes.

```bash
spur k8s up --controller http://localhost:6817
spur k8s status
spur k8s kubeconfig > mine.conf
spur k8s add-nodes --nodes gpu[09-12]
spur k8s remove-nodes --nodes gpu12
spur k8s down [--reset]
```

CNI options: `kuberouter` (default, IPIP overlay) or `calico` (`bird` native
routing over WireGuard mesh when `wg_enabled = true`, else VXLAN).

---

## 8. Upgrade procedures

### Single-host self-update

```bash
spur version --check     # check for updates
spur self-update          # download + replace binaries (stable)
spur self-update --nightly
```

The updater downloads the tarball, verifies SHA-256, and replaces binaries
atomically (rename to `.spur-old`, copy new, delete backups; rollback on
failure).  **Does not restart daemons.**

The `[update]` config block controls daemon startup checks:

| Field | Default | Effect |
|---|---|---|
| `check_on_startup` | `true` | Check GitHub releases on daemon start |
| `auto_update` | `false` | Auto-download (still never auto-restarts) |
| `channel` | `"stable"` | `"stable"` or `"nightly"` |
| `cache_dir` | `"/var/cache/spur"` | Update-check cache (1h TTL) |

### Rolling upgrade (Ansible)

The drain-aware, jobs-preserving cluster upgrade path:

```bash
ansible-playbook playbooks/rolling_upgrade.yml -i inventory/hosts.ini \
  -e spur_binary_src=/path/to/target/release
```

Steps:

1. **Guard rails**: abort if `spur_wipe_state=true`, if WireGuard transport, or
   if cluster is unhealthy.
2. **Upgrade controllers one at a time** (`serial: 1`).  On multi-controller
   clusters, the Raft **leader goes last** to minimize the mixed-version
   window (a new WAL variant emitted by a new leader would stall old followers).
3. **Drain and upgrade agents in batches**: drain, wait for `DRAINED`, swap
   binary, restart `spurd`, wait for re-registration, resume.
4. **Verify**: submit a test job.

Key flags:

| Flag | Default | Effect |
|---|---|---|
| `spur_rolling_batch_size` | `1` | Agents upgraded per batch |
| `spur_in_place_upgrade` | `false` | Never drain; trust `spurstepd` to carry jobs across |
| `spur_skip_busy_agents` | `false` | Skip busy nodes instead of aborting |
| `spur_force_upgrade_busy_agents` | `false` | Kill running jobs and upgrade anyway |

### Safe upgrade order

1. Rebuild all binaries from the same source tree (shared WAL schema).
2. Upgrade controllers before agents.
3. Drain agents before swapping binaries (unless `spur_in_place_upgrade`).
4. Never wipe state during an upgrade.
5. Only roll forward -- the Raft log gains entries newer binaries write that
   older binaries cannot parse.
6. Only roll forward across accounting schema migrations (take a DB backup
   first).

### Full convergence (deploy.yml)

Re-installs binaries and restarts every daemon on every host.  Causes a brief
cluster-wide outage.  Non-destructive by default (state preserved).

```bash
ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini \
  -e spur_binary_src=/path/to/target/release
```

---

## 9. Monitoring (OpenMetrics endpoints, Prometheus scraping)

### Metrics server

`spurctld` exports cluster-wide metrics over HTTP in Prometheus/OpenMetrics
text format on port 6822.  Controlled by the `[metrics]` section:

```toml
[metrics]
enabled = true
listen_addr = "[::]:6822"
bind = "loopback"          # "loopback" (default) or "all"
high_cardinality = false   # enables /metrics/jobs-users-accts
```

Only the Raft **leader** serves data; followers return `503`.  Point the
scraper at all controllers and let it follow the leader.

### Endpoints

| Path | Contents |
|---|---|
| `/metrics` | Alias for `/metrics/jobs`. |
| `/metrics/jobs` | Job counts by state, aggregate allocated CPUs/memory/GPUs. |
| `/metrics/nodes` | Node counts by state, cluster resource totals, per-node gauges (labeled `node=<name>`). |
| `/metrics/partitions` | Planned (currently empty). |
| `/metrics/scheduler` | Planned (currently empty). |
| `/metrics/k8s` | Spur-managed k0s lifecycle: cluster phase, control-plane replicas, node counts, drift, provisioning. |
| `/metrics/jobs-users-accts` | Per-user/per-account job gauges and karma counters (requires `high_cardinality = true`; 404 otherwise). |

### Key metrics

**Job metrics** (`/metrics/jobs`): `spur_jobs`, `spur_jobs_<state>`,
`spur_jobs_cpus_alloc`, `spur_jobs_memory_alloc_bytes`, `spur_jobs_gpus_alloc`.

**Node metrics** (`/metrics/nodes`): `spur_nodes`, `spur_nodes_<state>`,
`spur_nodes_cpus`/`spur_nodes_cpus_alloc`,
`spur_nodes_memory_bytes`/`spur_nodes_memory_alloc_bytes`,
`spur_nodes_gpus`/`spur_nodes_gpus_alloc`, `spur_nodes_not_responding`,
`spur_nodes_liveness_breaker_tripped`.  Per-node: `spur_node_cpus`,
`spur_node_gpus`, `spur_node_cpu_load`, `spur_node_free_memory_bytes`.

**k0s metrics** (`/metrics/k8s`): `spur_k8s_cluster_up`,
`spur_k8s_cluster_phase{phase}`, `spur_k8s_nodes_total`,
`spur_k8s_drift_detected_total{kind}`, `spur_k8s_reconcile_duration_seconds`.

### Prometheus scrape config

```yaml
scrape_configs:
  - job_name: spur
    static_configs:
      - targets:
          - controller-1:6822
          - controller-2:6822
          - controller-3:6822
    # Only the leader returns 200; followers return 503 and are skipped.
```

Set `bind = "all"` in `[metrics]` to expose the endpoint to a remote scraper.

---

## 10. Common operational tasks

### Drain a node

```bash
scontrol update NodeName=gpu-1 State=drain Reason="maintenance"
# Node stays DRAINING until running jobs complete, then becomes DRAIN.
# Resume:
scontrol update NodeName=gpu-1 State=RESUME
```

### Reconfigure

```bash
# Edit /etc/spur/spur.conf on the controller, then:
scontrol reconfigure
```

`reconfigure` runs on the Raft leader only.  Followers keep their startup
config until restarted.  Some fields are live (partitions, scheduler parameters,
`private_data`); others require a daemon restart (metrics bind, tick interval,
max jobs per cycle).  `spurd` reads `[cgroup]` only at startup.

### Snapshot recovery

Raft state lives in `{state_dir}/raft/`.  Default recovery mode is **strict**:
any unreadable log entry or snapshot is a hard error.

- If the snapshot is from an incompatible binary, start the matching binary
  version to replay forward, then upgrade.
- `--allow-partial-wal-recovery` enables lenient mode: unreadable entries are
  skipped (committed state is lost silently).  An unrestorable snapshot is
  quarantined to `snapshot.json.unrestorable`.
- Stale `.tmp` files from interrupted writes are cleaned on startup.

### Node health checks

```toml
[health]
max_unavailable = "10%"

[[health.checks]]
program = "/etc/spur/node-health.sh"
interval_secs = 300
timeout_secs = 60
```

The controller submits each check as a real job.  Non-zero exit drains the
node.  Resume to re-validate.  Only idle, allocated, and mixed nodes are
checked: down, drained, and not-responding (`*` in `sinfo`) nodes are skipped.

### Add/remove agents

```bash
# Ansible:
ansible-playbook playbooks/add_nodes.yml -i inventory/hosts.ini \
  -e new_nodes=gpu-3,gpu-4

ansible-playbook playbooks/remove_nodes.yml -i inventory/hosts.ini \
  -e nodes_to_remove=gpu-3,gpu-4
# remove_nodes.yml: drains, waits for DRAINED, stops spurd, deregisters.
```

### Uninstall

```bash
# Ansible teardown (stops daemons, preserves state):
ansible-playbook playbooks/teardown.yml -i inventory/hosts.ini
# With state wipe:
ansible-playbook playbooks/teardown.yml -i inventory/hosts.ini -e wipe=true

# Manual:
sudo systemctl disable --now spurctld spurd
curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- uninstall
sudo rm -rf /var/spool/spur /etc/spur
# Drop accounting DB:
sudo -u postgres dropdb spur && sudo -u postgres dropuser spur
```

### Key diagnostic commands

```bash
scontrol ping                    # controller reachable
spur nodes                       # registered nodes and state
spur queue                       # job queue
spur history                     # accounting history (sacct)
scontrol show config             # effective configuration
spur net status                  # WireGuard peers + handshakes
spur k8s status                  # k0s cluster phase + per-node state
journalctl -u spurctld           # controller logs
journalctl -u spurd              # agent logs
```

---

## Key source files

| File | Purpose |
|---|---|
| `spur-core/src/config.rs` | Configuration parsing (`SlurmConfig` and all sub-structs) |
| `spurctld/src/raft.rs` | Raft type config, startup, recovery |
| `spurctld/src/accounting/db.rs` | Schema DDL, SQL queries, migrations |
| `spurctld/src/cluster_k8s.rs` | k0s reconcile loop, mesh membership |
| `spur-net/src/wireguard.rs` | Key generation, WireGuard config management |
| `spur-update/src/lib.rs` | Self-update: check, download, install |
| `crates/spur-metrics/` | OpenMetrics exposition |
| `install.sh` | One-line installer/uninstaller |
| `Dockerfile` | Multi-stage build (AlmaLinux 8 builder, Ubuntu 24.04 runtime) |
| `examples/spur.conf` | Annotated config reference |
| `examples/k8s/` | K8s deployment manifests |
