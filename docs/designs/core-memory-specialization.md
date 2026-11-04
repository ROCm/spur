---
orphan: true
---

# Core and memory specialization — design

Reserve CPUs and memory on a node for services that run alongside jobs (e.g. a
WEKA client), and lay the groundwork for reserving cores per GPU. Operator
documentation lives in the admin guide's configuration reference; this note
records how the feature is built and the guarantees it relies on.

## Goal

Operators configure, per node:

- `core_spec_count` **or** `cpu_spec_list` — CPUs reserved for node services.
- `reserved_memory_mb` (alias `mem_spec_limit`) — memory reserved for them.
- `daemons_off_spec` — keep `spurd`'s threads off the reserved CPUs (on by
  default, unlike Slurm's `SlurmdOffSpec`); supervisors inherit the mask when
  they start.

No job may then land on the reserved CPUs or grow into the reserved memory, and
the schedulable inventory the controller advertises excludes them.

## Architecture

<!-- spellcheck-disable -->

```text
  controller                                     agent (spurd)
  ──────────                                     ────────────
  topology (from agent) ─┐
  [[nodes]] settings  ───┤── compute CoreLayout ── deliver ──▶ adopt + hold
                         │   (spec / general /      (reg reply,   │
                         │    gpu-reserved CPUs)     launch)       ▼
                         │                                   enforce on the
  schedulable count  ◀───┘   fingerprint, ◀── heartbeat ──┐  spur cgroup root:
  excludes spec CPUs         enforcement error            │  - cpuset.cpus =
                                                          │    non-spec CPUs
  layout change ─▶ reregister ◀─────────────────────────┘  - memory.high→max
```

<!-- spellcheck-enable -->

- **Controller** reads the agent-reported CPU topology at registration, computes
  the node's `CoreLayout` (which CPUs are spec / general / GPU-reserved), and
  delivers it in the registration reply and in every launch request. The
  schedulable CPU count (`CPUEfctv`) excludes spec cores; `CPUTot` keeps them.
- **Agent** discovers real online CPU IDs grouped by core and NUMA from sysfs,
  adopts the delivered layout, and applies it to the `spur` cgroup root: a
  reconciled `cpuset.cpus` and the memory reservation. Each heartbeat reports the
  held layout's fingerprint and, if it is not enforcing, why.
- **Policy core** (`spec.rs`) is pure and unit-tested without a cgroup file
  system; the cgroup writes are a thin wrapper.

## Guarantees

- **Fail closed, never clear on error.** A layout the controller cannot compute
  (offline spec CPU, missing topology, spec covering every core, or reserved
  memory covering all memory) keeps the last good layout and marks the node
  unschedulable with a `core-layout:` reason. The registration reply carries the
  error, so the agent keeps what it holds even after a controller restart has
  lost the last good layout. Only an error-free registration reply clears.
- **Ordered delivery.** Each delivery carries a revision ordered by (Raft term,
  controller boot epoch, per-node sequence). The sequence advances on every
  layout change, a clear included; the boot epoch is a durable per-controller
  start counter, because openraft restores a restarted leader at its old term.
  A launch older than the newest revision the agent has seen is ignored, and a
  launch never clears a layout. A registration reply is dropped if a later
  registration was sent or a newer delivery landed while it was in flight;
  otherwise it may move the watermark down, which is how a restarted per-node
  sequence is recovered.
- **Unknown policy changes nothing.** A restarted agent leaves the root as found
  until a delivery states the node's policy; if the controller reports it cannot
  compute one and the agent holds nothing, the root keeps its limits and the
  agent refuses launches. A missing root (after a reboot) counts as released.
- **Enforced before launch.** A newly adopted layout is written to the root
  before a launch or standalone `srun` allocation is accepted; until then, or
  while root writes fail or the node's own `[cgroup]` is disabled, the node
  refuses workloads and reports the cause, which the controller turns into a
  `core-layout: not enforced:` reason. Re-registration is requested only when it
  can help, at most once per interval for the same layout.
- **cpuset widening.** cgroup v2 widens a child whose cpuset no longer intersects
  its parent's, so a CPU a live job would otherwise lose entirely stays in the
  root until that job exits.
- **Memory only when reserved.** Without `reserved_memory_mb` the root's memory
  limits stay at `max`, so a node never depends on reading its memory total; with
  a reservation, an unreadable total is an enforcement error and nothing is
  written.
- **Memory tightening.** `memory.high` drops to the target immediately (throttle
  only, never OOM); `memory.max` follows only while no process runs under the
  root (`cgroup.events` `populated 0`), so a hard limit never lands under a
  running job.
  `memory.max` is never loosened except by a full reset, and a restarted agent
  that finds it already at or below the target treats the reservation as active.
- **Only reset what spurd wrote.** A marker file records that spurd holds
  controls on the root; a node without specialization never touches operator-set
  limits.
- **Ledger exclusions.** The allocation ledger excludes exactly the current
  layout's spec CPUs on every allocation path and after capacity rebuilds; a CPU
  that stops being excluded stays taken while a running job holds it.
- **Cheap steady state.** Root writes happen only when a value changes, on a
  blocking thread, and the spec CPU set is read without waiting on root I/O.

## Testing

- Unit: the policy core (root cpuset, reconciliation, memory sequencing,
  adoption and watermark rules, marker and reset), the controller layout
  compute, error retention, heartbeat decision, state projection and reconfigure
  rejection, and config validation.
- e2e (`tests/native_host/e2e/test_core_spec.py`): reserve a CPU and memory on a
  rootful node; assert the `scontrol` fields, the root's `cpuset.cpus` and
  `memory.high`, a job's own CPU affinity, and that spurd resets the root when
  specialization is removed.

## Out of scope

- Per-GPU reserved cores and `--cpus-per-gpu` (follow-up that builds on the
  layout machinery here).
- `spur-ffi` gains `cpus_per_gpu` only with a future revision of its C interface.
- A per-job `-S` / `--core-spec` request; reservations are per node only.
- Re-pinning already running `spurstepd` supervisors when `daemons_off_spec`
  changes.
