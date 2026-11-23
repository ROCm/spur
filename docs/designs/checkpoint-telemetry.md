---
orphan: true
---

# Workload-reported checkpoint telemetry

## Goal

Give training jobs an opt-in API to tell Spur when they checkpoint, so operators
can see — through the controller's metrics endpoint — whether jobs checkpoint,
how many checkpoints succeed, and how much work is lost when preemption, QOS,
time limits, or cancellation end a run.

Spur observes only. It does not integrate with training frameworks, infer
checkpoints, or change scheduling based on these events. A job that never calls
the API behaves exactly as today.

## Prior art

- Adjacent work: ROCm/spur#280 (`--signal` pre-timeout warning).
- **Slurm has no equivalent.** BLCR `scontrol checkpoint` was scheduler-driven
  and removed in 20.11; `--signal` is controller→job; `acct_gather_profile`
  (HDF5/InfluxDB) samples only plugin-collected system data; `Comment`/`Extra`
  are unstructured strings. This is a Spur-native surface.
- Public practice this aligns with:
  - Google ML Goodput Measurement: step-keyed `save`/`restore` events with
    UTC timestamps; checkpoint save/restore time counted as badput.
  - NVIDIA Resiliency Extension (NVRx): ranks report section boundaries
    (setup / step / checkpointing) to a node-local monitor over a Unix socket.
  - Framework hooks (Lightning `on_save_checkpoint`, HF Trainer `on_save`,
    Megatron save timer) are the natural call sites.
  - Frontier labs' internal checkpoint tracking is not publicly documented.

## Decisions

### D0. Phase 1 requirements and scope

- **R1 — no dependencies:** a job reports an event with the language runtime it
  already has — a few lines of the Python standard library (`socket`, `json`,
  `os`). Jobs cannot reach `spur`/`spurd` tooling (the scheduler layer is
  isolated from them, e.g. PyTorch in a container), so no CLI, package, or
  client library is required. Plain shell `echo` is not supported: a redirect
  cannot open a Unix socket (`ENXIO`, verified on 6.8), and the file-based
  alternative failed R2 (see Spike results).
- **R2 — kernel-verified attribution:** every accepted event is attributed via
  kernel-supplied peer credentials, checked against the writer's cgroup
  (`spur/job_<id>_<attempt>/step_<N>`) and uid.
- **R3 — never harms the job:** sending an event never blocks, hangs, or kills
  the job, whatever state spurd is in (alive, overloaded, hung, dead). Every
  client-side operation is bounded by a short timeout and every error is
  swallowed.
- **Phase 1 scope:** each step's spurstepd collects its events, so collection
  continues while spurd restarts or is upgraded; spurd picks the records up
  when it re-adopts the step. Events are lost only if the step's spurstepd
  itself is gone. Steps without a spurstepd (`stepd_enabled = false`) and k8s
  virtual-agent nodes get no checkpoint socket; accounting (`sacct`) is a later
  phase.

### D1. Observability only

Events feed metrics and (optionally) accounting. The scheduler never acts on
them, so user-supplied data needs no trust beyond job attribution.

### D2. Fixed event vocabulary, no free-form metrics

Spur defines the event kinds; no user-named metrics or labels. Keeps series
count bounded and avoids turning spurd into a general metrics relay. The wire
message carries a `kind` so kinds can be added later without breaking v1.

### D3. Transport: a per-step Unix socket served by spurstepd

- Each step's spurstepd (one per step, root, outside the step cgroup, survives
  spurd restarts) binds `/run/spur/<cluster>/checkpoint/<job>.<attempt>.<step>.sock`
  (`<cluster>` = `SlurmConfig.cluster_name`)
  before it launches the step, `chown`s it to the job uid with mode `0600`
  (other users cannot connect), and spurd injects
  `SPUR_CHECKPOINT_SOCK=<path>` into the step's task env. The directory is
  owned by the user spurd runs as (root in a standard install), mode `0711`,
  so jobs cannot list other steps' sockets. The socket is removed at step end.
- The listener runs on the existing tokio runtime in spurstepd. One listener per
  step spreads a synchronized checkpoint across steps instead of funnelling
  every rank on the node into one accept loop.
- One event per connection: the client connects, sends one JSON line
  (≤ 512 bytes), waits for a fixed 1-byte reply, and closes. The client bounds
  connect, send, and reply with a single ~50 ms timeout and ignores every error
  (R3): spurstepd gone → `ECONNREFUSED`/`ENOENT` immediately; hung or backlog
  full → timeout.
- spurstepd bounds its side too: a capped number of in-flight connections, a
  short read deadline per connection (no slow-loris), and connections over
  the cap are closed immediately.
- The reply is sent only after spurstepd has read the peer's identity (D4),
  which keeps the client alive during the lookup. It is the same byte whether
  the event is accepted or rejected (D5).
- Rejected alternative: one node-wide socket on spurd. Events are lost for the
  whole node while spurd restarts, and a single accept loop is the burst
  bottleneck the spike hit (see Spike results).

### D4. Access control: peer credentials, job cross-check, step fallback

- **Kernel identity:** on accept spurstepd reads `SO_PEERCRED` (PID, uid, gid —
  captured by the kernel at `connect`, unforgeable, translated into its PID
  namespace for container callers) and resolves `/proc/<pid>/cgroup` while the
  client is still waiting for its reply. Where available (Linux ≥ 6.5),
  `SO_PEERPIDFD` pins the process so PID reuse cannot misdirect the lookup.
- **Own step only:** the peer's cgroup must be exactly the step cgroup this
  spurstepd supervises (`job_<id>_<attempt>/step_<N>`), and the peer uid must
  be the job's uid (uid 0 only for root jobs). Anything else is rejected —
  login-node users, system daemons, and other steps or jobs.
- **Job info as a cross-check, never the source of truth:** the message carries
  the `job_id` from the task env (`SPUR_JOB_ID`); a mismatch with the
  supervised step is rejected. `step_id` (`SPUR_STEP_ID`) is optional — a
  plain `sbatch` batch step has none — but a present value is cross-checked
  the same way.
- **Step fallback:** if the cgroup can no longer be read (the client gave up
  and exited), spurstepd accepts only if the connect-time uid is the step's
  owner and the step is still active, recording the event as
  `verification=uid` rather than `verification=cgroup`. Cross-user spoofing
  stays impossible; the socket's `0600` mode already limits callers to the job
  user.
- **Rejected alternatives** (see Spike results): a per-step file written with
  `echo >>` and verified by async fanotify notifications (writers exit before
  their cgroup can be resolved — 164–168 of 200 foreign writers undetected);
  FIFO + `FAN_OPEN_PERM` or FUSE (the writer waits on Spur, violating R3);
  tokens in the environment (leak via `/proc/<pid>/environ`; spurd strips
  credentials from job env).

### D5. The channel carries checkpoint events and nothing else

The step's socket is the job's only surface toward Spur for this feature, and it can
express only the checkpoint vocabulary.

- **Data, never commands:** spurstepd only parses messages into a fixed per-step
  record that feeds metrics (D1). A message can never change job, node, or
  scheduler state, reach Raft/WAL, trigger RPCs, or be forwarded anywhere
  except as that numeric record.
- **Strict allowlist parser:** `v` must be a known version; `kind` must be one
  of the fixed v1 kinds; `step` is a `u64`; `status` is an enum; `job_id` is
  required and `step_id` is optional (absent when the sender has no
  `SPUR_STEP_ID`), and both are only compared against kernel identity (D4).
  Unknown keys, extra fields, strings outside enums, and messages over 512
  bytes are rejected whole. Per-step rate limit; excess is dropped and
  counted. The parser is a pure function with unit and fuzz tests.
- **Every drop is accounted:** each event spurstepd receives is either applied,
  dropped, or rejected as invalid, with a reason in either case
  (`not_job_process`, `uid_mismatch`, `read_timeout`, `rate_limited`, `busy`
  for delivery/attribution drops; `job_mismatch`, `unknown_kind`,
  `unsupported_version`, `invalid_status`, `unknown_field`, `invalid_field`,
  `malformed_json`, `oversize` for an invalid event). spurstepd keeps
  per-reason counts in the step's record and logs each drop (rate-limited,
  job/step/uid/reason only, never message contents). spurd sums them per
  node; spurctld exposes `spur_checkpoint_events_dropped_total{reason}` and
  `spur_job_run_checkpoints_failed_total{reason}`, plus the per-run counts
  next to the accepted count, so accepted + dropped + invalid equals
  everything the step's listener saw. Only events a client could not deliver
  at all (spurstepd gone or hung) are unaccounted.
- **No information flows back:** the reply is one fixed byte whether the event
  was accepted or not. Rejections are visible only to operators through
  `spur_checkpoint_events_dropped_total{reason}` /
  `spur_job_run_checkpoints_failed_total{reason}` and rate-limited logs that
  never echo message contents.

### D6. Container jobs get the socket by default

Spur-managed containers bind-mount the step's socket at the same path with no
flag, reusing the `bind_mount` path in `crates/spurd/src/container.rs` (the
same shape as kubelet sockets exposed to pods via `hostPath`). Unlike the
spurauthd mint socket (opt-in via `--container-auth-socket`), it grants no
privilege. Only the step's own socket is mounted, never the directory. Native
jobs see the host path directly. Containers a job starts itself
(docker/podman/apptainer) need a user-supplied bind; docker moves processes out
of the step cgroup, so D4 rejects them.

### D7. spurstepd → spurd → spurctld: a dedicated batched RPC, not the heartbeat

The heartbeat stays small and unchanged; checkpoint records travel on their own
request.

- **spurstepd → spurd:** the step's record (counts, last steps, per-reason
  drops; numeric, fixed size) lives in `checkpoint.json` in the
  root-only spurstepd session directory, rewritten atomically at most every 5 s while it
  changes and once more, marked final, at step end. spurd `stat`s the files
  every 30 s and reads changed ones, and reads the final file on completion or
  re-adoption, so it collects records even after being down. No stepd request
  or reply changes: `PROTOCOL_VERSION` stays 1 and live steps are still
  re-adopted across an upgrade (the handshake hard-fails on any version
  mismatch). Polling `QueryState` was rejected: it is used today only for
  launch readiness, so a 30 s poll of every live spurstepd would be new
  per-step socket load. A new `obligations.jsonl` variant is avoided because
  an older spurd would fail to parse it after a downgrade.
- **spurd → spurctld:** a new `ReportCheckpointTelemetry` RPC on
  `SlurmController` (additive; authenticated like the heartbeat with the node
  token; bounded timeout). spurd batches only records that changed since the
  last acknowledged send, every 30 s, and immediately for steps that just
  ended (the final flush that captures the preemption-grace checkpoint). A
  failed send keeps the records for the next batch; nothing is resent once
  acknowledged.
- **spurctld:** leader in-memory state, not Raft/WAL (non-leaders forward). It
  caps records per job and per request, accepts a record only if the run
  attempt has, or just had, an allocation on the reporting node, and finalizes
  run-end metrics when the job is terminal and every allocated node has sent
  its final record, or after a short grace timeout.
- Old agents never call the RPC; old controllers reject it as unimplemented and
  spurd backs off.

### D8. Scale design

Invariant: **state is O(1) per job per node regardless of rank count or event
rate.**

| Stage | Bound |
|---|---|
| Rank → spurstepd | O(1) in-memory record update in the step's own listener; per-step token-bucket rate limit; numeric fields only (no paths) |
| Node merge | spurd merges its steps' records by training `step` (min begin, max end, ranks reported) |
| Node → spurctld | One `ReportCheckpointTelemetry` batch per node per 30 s, only changed records (~64 B each); the heartbeat is unchanged |
| Controller ingest | Map `job → node → record` under its own lock (never `self.jobs` write lock); job aggregate recomputed per node report; step complete only when all job nodes report it; entries dropped at job end / node removal |
| Scrape | O(jobs), reads precomputed aggregates |

Cardinality:

- Default `/metrics/jobs`: cluster aggregates only (constant series count).
- Per-job series only on `/metrics/jobs-users-accts` behind
  `metrics.high_cardinality = true` (~6 series/job; labels `job_id`, `user`,
  `account`, `job_name` -- the same identity label set on every per-job series).
- Never per-node or per-rank series; `step` is a value, never a label.

### D9. Primary metrics: checkpoint state correlated with run end reason

Recorded once per run (`run_attempt`) at termination, from the final flush --
but only for a run that reported at least one checkpoint event; a run with
none is skipped entirely rather than counted as `checkpoint="none"`:

- `spur_job_runs_ended_total{reason, checkpoint="none|completed|in_progress"}`
- `spur_job_runs_restored_total{reason}` (runs that reported a restore)
- `spur_job_run_end_seconds_since_last_checkpoint{reason}` (histogram; lost work)
- `spur_job_run_checkpoints_completed{reason}` (histogram)
- `spur_preempt_grace_checkpoint_total{outcome="completed|failed|none"}`
- `spur_job_checkpoints_failed_total{job_id, user, account, job_name, reason}`
  (per-job, per-reason invalid-event count for the job's current or last run; `high_cardinality` only)

`reason` comes from the terminal `JobState` (PREEMPTED, TIMEOUT, CANCELLED,
NODE_FAIL, REQUEUED, COMPLETED, …) and whether a preemption grace was active.

## Open questions

### For team review (design / PR review)

The v1 vocabulary below is the complete set of events a job can send
(D5). Proposed, to be confirmed by the team, along with the rate-limit
values, the socket connection and read caps, and whether the documented helper
should emit from every rank (spurd merges) or only local rank 0 per node
(8× fewer connections under a synchronized checkpoint):

| Kind | Fields | Purpose |
|---|---|---|
| `checkpoint-begin` | `step` | Detect kill mid-checkpoint |
| `checkpoint-end` | `step`, `status=ok\|failed` | Successful count, last good checkpoint |
| `restore` | `step` | Resume point → work lost across requeue |

- `step` is a required, monotonic `u64` (training step) — the merge key across
  ranks and nodes; an end below the last completed step is ignored, since
  ranks finish the same save at different times.
- The client is fire-and-forget: one bounded connection per event, every error
  swallowed (D3).

### Later

- Job-end summary into accounting (`RecordJobEnd`) for `sacct`.
- k8s-operator nodes (virtual agent) support.
- Docs: user guide page + metrics reference.

## Example (proposed)

Batch script — no new flags:

<!-- spellcheck-disable -->
```bash
#!/bin/bash
#SBATCH --nodes=4
#SBATCH --gpus-per-node=8
#SBATCH --qos=preemptible          # GraceTime=120
#SBATCH --requeue
srun torchrun --nnodes=$SLURM_NNODES --nproc-per-node=8 \
  --rdzv-backend=c10d --rdzv-endpoint=$(hostname):29500 \
  train.py --ckpt-dir /shared/ckpt/$SLURM_JOB_ID
```
<!-- spellcheck-enable -->

Client helper: the standard-library `emit` in the user guide
(`docs/user-guide/checkpoint-telemetry.rst`), which is the tested copy.

Training loop (every rank calls `emit`; spurd merges by step):

<!-- spellcheck-disable -->
```python
stop_requested = False
def on_sigterm(signum, frame):            # preemption grace: SIGCONT then SIGTERM to srun steps
    global stop_requested
    stop_requested = True
signal.signal(signal.SIGTERM, on_sigterm)

def save(step):
    emit("checkpoint-begin", step)
    try:
        dcp.save(state_dict(), checkpoint_id=f"{ckpt_dir}/step_{step}")
        emit("checkpoint-end", step, status="ok")
    except Exception:
        emit("checkpoint-end", step, status="failed")
        raise

step = load_latest(ckpt_dir)
if step > 0:
    emit("restore", step)
while step < max_steps:
    train_step(batch())
    step += 1
    if step % 1000 == 0 or stop_requested:
        save(step)
    if stop_requested:
        break
```
<!-- spellcheck-enable -->

From a shell script, use the same standard library (no Spur tooling):

<!-- spellcheck-disable -->
```bash
python3 -c 'import json,os,socket
s=socket.socket(socket.AF_UNIX);s.settimeout(0.05)
try:
  s.connect(os.environ["SPUR_CHECKPOINT_SOCK"]);s.sendall(json.dumps({"v":1,"kind":"checkpoint-end","step":4000,"status":"ok","job_id":os.environ["SPUR_JOB_ID"]}).encode()+b"\n");s.recv(1)
except Exception: pass' || true
```
<!-- spellcheck-enable -->

Preempted at step 4420 with a successful grace save → final flush reports
completed=5, last=4420 → `spur_job_runs_ended_total{reason="preempted",
checkpoint="completed"}` +1; the requeued run's `restore step=4420` shows zero
lost steps. If the grace save was cut off, the run ends `in_progress` with last
good step 4000 (~420 steps lost) — the signal for tuning GraceTime.

## Spike results (2026-11-21)

Throwaway spike on two 8× MI355X nodes (kernel 6.8.12, Ubuntu 24.04), 64 steps ×
8 ranks = 512 concurrent writers. It evaluated the earlier file-based design
(per-step file on a capped tmpfs + async fanotify `FAN_MODIFY`).

| Check | Result |
|---|---|
| Capped tmpfs, `nosuid,nodev,noexec`, ACL write-only, punch-hole | Pass |
| `echo >> file` is one `write(2)`; 256 000 concurrent appends, 0 corrupt lines | Pass |
| Writer never blocks with listener live, stalled 30 s, or killed by SIGKILL | Pass (median 8–12 µs, max < 23 ms) |
| Full tmpfs fails fast (`ENOSPC`) | Pass (~20 µs) |
| Writer attributed to its cgroup after the fact | **Fail** — writers exit before lookup (30–72 of ~520 events in a realistic burst; all 512 with a stalled listener) |
| Foreign writer detected | **Fail** — 164–168 of 200 missed |
| `FAN_REPORT_PIDFD` closes the race | **Fail** — pidfd is created at read time; exited writers yield errors |
| Mount mark sees writes via a job's copied mount namespace | **Fail** — 0 events (filesystem marks do see them) |
| `echo > unix-socket` | `ENXIO` in bash and sh; Python stdlib connect+send works |

Conclusion: identity must be captured while the writer is alive, at
`connect` time (D3, D4). R3 behaviour of the file path informed the bounded
client and server timeouts in D3.

### Socket path (D3/D4), same nodes

A Python stand-in for the socket handler (thread per connection; it ran the
node-wide variant, so one listener served all steps) and the
documented stdlib client (50 ms budget), clients running as the job user
inside per-step cgroups.

| Check | Result |
|---|---|
| `SO_PEERCRED` → cgroup at accept; `SO_PEERPIDFD` on 6.8 | Pass — 100% of accepted events cgroup-verified, 0 uid fallbacks |
| Client in its own PID + mount namespace | Pass — verified by cgroup |
| Claiming another job, non-job process, other uid | Pass — all rejected (`cgroup_mismatch`, `uid`) |
| spurd absent / dead (stale socket) | Pass — client returns in ≤ 0.03 ms |
| spurd hung (never accepts) / never replies, 512 clients | Pass — client returns in ≤ 54 ms / 50 ms |
| 100 idle connections (slow-loris) | Pass — cut at the read deadline; real clients served |
| One 8-rank job per node, synchronized checkpoint | Pass — 24/24 accepted, p99 ≤ 4 ms |
| 8 jobs × 8 ranks per node, synchronized | Pass — 192/192 accepted, p99 ≤ 24 ms |
| 16 jobs × 8 ranks per node, synchronized | Pass — 384/384 accepted, p99 ≤ 40 ms |
| 64 jobs × 8 ranks, synchronized (stress) | Events dropped (15–21% accepted); clients still bounded at ~50 ms |

The stress loss came from the stand-in's throughput (~4 000 connections/s in
Python), not the listen backlog: `listen(4096)` and client retries within the
budget did not raise acceptance. This is one reason the listener moved into
each step's spurstepd (D3), which splits such a burst across steps.
Implementation requirements that follow: the
listener stays allocation-light and off any shared lock, the listen
backlog is raised (≥ 1024), and a connection-rate benchmark is part of the
test plan. The job is never blocked in any case; overload only drops events.
