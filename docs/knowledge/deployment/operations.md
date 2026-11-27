# Spur Deployment and Operations

Operational invariants and gotchas that span daemons. Procedures live in the
user docs:

- Install and systemd units: `docs/deployment/native-host.rst`; Ansible
  toolkit and playbooks: `docs/deployment/ansible.rst`.
- Kubernetes: `docs/deployment/kubernetes.rst` (existing cluster),
  `docs/deployment/managed-kubernetes.rst` (Spur-managed k0s).
- WireGuard: `docs/deployment/wireguard.rst`. Upgrades:
  `docs/deployment/upgrading.rst`. Uninstall: `docs/deployment/uninstalling.rst`.
- Config fields and their reload class: `docs/admin-guide/configuration.rst`.
  PostgreSQL setup: `docs/admin-guide/accounting.rst`.
- Raft config, storage layout, and recovery modes: `docs/knowledge/consensus/raft.md`.
  Accounting outage behaviour: `docs/knowledge/accounting/accounting.md`. Mesh
  topology: `docs/knowledge/networking/wireguard-mesh.md`.

## Host install invariants

- The `spurd` unit needs `KillMode=process`. The systemd default
  (`control-group`) kills the detached `spurstepd` supervisors on agent
  restart, so jobs would not survive it.
- `spurstepd` must be installed next to `spurd`. `resolve_stepd_executable`
  looks beside `spurd`'s own executable and otherwise execs the bare name
  (CWD-relative, no `$PATH` search).

## Upgrades

### Upgrade order

Upgrade order (controllers before agents, Raft leader last, roll forward only,
never wipe state mid-upgrade, drain unless `spur_in_place_upgrade`) and its
rationale are in `docs/deployment/upgrading.rst` ("Safe Upgrade Order" and the
`upgrading-leader-last` section). Build every binary from the same source tree;
they share the WAL schema.

### Single-host self-update

- `spur update` and `spur self-update` are the same binary-upgrade command
  (`spur control update` changes jobs/nodes).
- SHA-256 verification is optional: it runs only when the release has a
  `.tar.gz.sha256` sidecar asset.
- Replacement is not atomic despite the name `install::atomic_replace`; see
  `crates/spur-update/AGENTS.md`.
- `[update] auto_update` downloads and installs the update (`do_self_update`),
  but neither it nor `spur self-update` restarts daemons. `[update]` applies to
  `spurctld` only (`spurd` passes `auto_update = false`).

## Reconfigure and node inventory

- `scontrol reconfigure` re-reads the file on the Raft leader only. Followers
  keep their startup config until restarted. Restart-only fields are marked
  in `docs/admin-guide/configuration.rst`; `spurd` reads `[cgroup]` only at
  startup.
- `admission.node_names` is saved through Raft only by `scontrol reconfigure`,
  and only when the list differs from the saved one; a removed field equals a
  cleared list. A restart never saves it or deletes nodes. Its Raft entry
  (`NodeInventoryUpdate`) is new, so set the field only after every controller
  is upgraded.
- Removing a node: see `docs/deployment/wireguard.rst` (`wireguard-remove-node`)
  and, with `admission.node_names`, `docs/admin-guide/configuration.rst`
  (`configured-node-list`).

## Raft state on disk

- Atomic writes go through `*.tmp` plus rename, so a snapshot write needs
  free space for about two copies of `snapshot.json`.

## Node health checks

- `[health]` checks run as exclusive whole-node jobs pinned to each node. A
  failing check drains the node.
- Only responding nodes in `Idle`, `Allocated`, or `Mixed` are checked
  (`node_off_limits_for_health`); down, drained, and not-responding nodes are
  skipped.

## K8s operator deployment

- `spur-k8s-operator` watches `SpurJob` CRDs (API group
  `spur.amd.com/v1alpha1`) and submits them to `spurctld`.
- K8s nodes appear as Spur compute nodes without running `spurd` (virtual
  agents served by the operator).
- With `--enable-quota`, the operator projects each Spur account's
  `grp_tres` into per-account K8s quota objects (`ResourceQuota`,
  `LimitRange`).
- RBAC is scoped to the operator's own `spur-operator` ServiceAccount.
  `spurctld` and `spurd` pods set `automountServiceAccountToken: false`.
- Spur-managed k0s (`[cluster].enabled`) is covered in
  `docs/knowledge/execution/containers-k0s.md`.

## Monitoring

Endpoints and metric names are listed in `docs/user-guide/monitoring-jobs.rst`.

- `/metrics` is an alias for `/metrics/jobs`.
- Every endpoint except `/metrics/auth` and `/metrics/audit` returns `503` on
  a follower, so point the scraper at all controllers. `/metrics/auth` and
  `/metrics/audit` are per-process and served by every controller.
- `/metrics/jobs-users-accts` returns `404` unless `metrics.high_cardinality`
  is set, checked before the leader check.
- The listener binds loopback by default; a remote scraper needs
  `bind = "all"`.
