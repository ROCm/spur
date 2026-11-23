# Spur Architecture

Daemon roles and trust boundaries are in the root `AGENTS.md`; components and
ports for users are in `docs/user-guide/architecture.rst`. This file holds the
cross-daemon flows. Per-area detail:

- Raft write path, leadership, snapshots, WAL compatibility:
  `docs/knowledge/consensus/raft.md`
- Backfill, ordering, cons_tres, preemption: `docs/knowledge/scheduler/scheduling.md`
- spurd/spurstepd launch, start gate, teardown, recovery:
  `docs/knowledge/execution/job-execution.md`
- Containers and embedded k0s: `docs/knowledge/execution/containers-k0s.md`
- WireGuard topology (hub-and-spoke vs full mesh): `docs/knowledge/networking/wireguard-mesh.md`
- Accounting and admission caches: `docs/knowledge/accounting/accounting.md`
- Config reload vs restart-only fields: `crates/spurctld/AGENTS.md`
- Job and node state machines: `Job::transition` / `NodeState::transition` in
  spur-core, pitfalls in `crates/spur-core/AGENTS.md`

## 1. Job submission through completion

1. The CLI calls `SubmitJob`. spurctld validates the spec and proposes
   `WalOperation::JobSubmit`; the job is Pending once it commits.
2. The scheduler loop runs on the Raft leader only and produces assignments.
3. The leader reserves the placement with `WalOperation::JobStart`. The job
   stays Pending but occupies capacity.
4. Multi-node PMIx jobs first get `PreparePmix` on every node (start PMIx
   servers, exchange peer info). Any failure aborts dispatch before
   `LaunchJob` is sent.
5. `LaunchJob` fans out to the assigned spurd agents. Each spawns a spurstepd,
   which holds the workload at its start gate.
6. Only after every node confirms does the leader propose the
   Pending -> Running `JobStateChange` (gated on `run_attempt`).
7. `StartJob` then releases the gate on every node, so nothing the job launches
   can reach the controller while it still looks Pending. If any node fails to
   release, the leader cancels the run on all nodes and evicts that attempt.
8. On exit, spurstepd tells spurd over its Unix socket, and spurd reports via
   `ReportJobStatus`. The leader proposes one `JobNodeComplete` per node; its
   apply frees that node's slice, and the last node's report finalizes the job
   in that `JobNodeComplete` apply. Paths that finish without per-node reports
   (srun and PTY exit, completing-timeout force-finish, node-less timeout)
   propose `JobComplete` via `ClusterManager::complete_job`. A multi-node job
   whose script runs on every node reports through `JobCopyExited` instead (see
   [job execution](../execution/job-execution.md#per-node-runs-allnodes)). A node's
   first report is its exit: `ClusterManager::node_complete` proposes nothing for
   a repeat, but answers it with where the run stands, so a reporter retrying the
   follow-up (stopping the other nodes) still gets it. Reports for an older
   `run_attempt` or a non-active job are dropped in the apply.

## 2. Node registration and liveness

- `RegisterAgent` is refused for a node outside a saved `admission.node_names`.
  `NodeInventoryUpdate` (proposed only by `ClusterManager::reconfigure`) saves
  that list and deletes unlisted nodes in one entry. Semantics are in
  `docs/knowledge/deployment/operations.md`.
- Resource inventory and node address are set at registration, not by
  heartbeats. spurd's periodic inventory refresh acts only on a change seen on
  two consecutive ticks, and converges by re-registering.
- A >0 to 0 GPU discovery is treated as a suspect scan (likely a KFD/CDI loader
  error): spurd keeps the old GPU capacity and marks fabric observations
  unavailable instead of converging to zero (`is_suspect_gpu_dropout`).
- The leader's health checker flags nodes silent past `heartbeat_timeout_secs`
  as not-responding (no new work) and probes their agent directly
  (`node_liveness`). A node is marked Down only after failing two probes a
  health pass apart, and not at all when so many nodes go silent at once that
  the leader suspects itself (`max_stale_node_fraction`). Heartbeats often
  arrive through a follower, so silence alone cannot distinguish a dead node
  from a broken path.
- Marking a node Down evicts its running jobs: the apply force-completes their
  steps, and the leader sends cancels to the agents. A leadership grace period
  defers Down right after an election (`crates/spurctld/AGENTS.md`).
- A returning Down or Error node is held down while its heartbeats report runs
  the controller already ended, or cannot list its jobs. The leader cancels
  those runs. If the node is still not clean at `recovery_hold_cap` (max of
  `heartbeat_timeout_secs` and the kill + cleanup + epilog budget plus one
  heartbeat interval), it is drained admin-locked so a job that will not die
  cannot keep it Down forever.
- `DeregisterAgent` (spurd on SIGTERM, except while it holds stepds; see
  `crates/spurd/AGENTS.md`) marks the node Down and keeps its record.

## 3. Kubernetes integration

There are two independent modes.

- **Embedded k0s** (`[cluster].enabled`): a leader-gated reconcile loop in
  `spurctld` `cluster_k8s` drives k0s on Spur nodes through spurd, with k0s as a
  spurd-managed systemd unit that survives spurd restarts. Details in
  `docs/knowledge/execution/containers-k0s.md`.
- **Operator** (`crates/spur-k8s`): runs inside an existing K8s cluster. It
  reconciles `SpurJob` CRDs into `SubmitJob`, registers K8s nodes with spurctld
  as compute nodes that run no spurd, and serves a virtual `SlurmAgent` that
  turns `LaunchJob`/`CancelJob` into Pod create/delete. The quota controller
  projects Spur accounts into per-account Namespace, ResourceQuota,
  LimitRange, and RBAC objects. Deployment is in
  `docs/knowledge/deployment/operations.md` (K8s operator deployment).

## 4. Controller HA

Multiple spurctld instances form a Raft cluster; the leader handles writes and
followers forward client RPCs, so the controllers act as one logical endpoint.
Clients and agents take a comma-separated controller list and fail over at dial
time (`crates/spur-client/AGENTS.md`). Raft details are in
`docs/knowledge/consensus/raft.md`.

## 5. gRPC services

RPC definitions live only in `proto/slurm.proto`. `SlurmController` and
`SlurmAccounting` share spurctld's gRPC port (accounting is in-process).
`SlurmAgent` is served by spurd and also by the spur-k8s operator's virtual
agent, so a change to agent RPC semantics must be implemented on both.
