# Job Suspend/Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Slurm-parity job suspend/resume to Spur — `SuspendJob`/`ResumeJob` controller RPCs, SIGSTOP/SIGCONT agent dispatch, `scontrol suspend|resume`, and full suspended-time accounting (excluded from run-time and time-limit enforcement).

**Architecture:** State flips through Raft via two new timestamped WAL ops (`JobSuspend`/`JobResume`); the controller then fans out an agent RPC to SIGSTOP/SIGCONT the process on every allocated node. Allocation is retained while suspended. Suspended-time accounting lives in `spur-core` (`Job::run_time` / `Job::effective_deadline`) driven by logged timestamps for replay determinism.

**Tech Stack:** Rust, tonic/prost gRPC, openraft, nix (signals), tokio, chrono.

---

## Background the engineer needs

- **Workspace layout:** crates under `crates/`. Touch `spur-core` (types), `spur-proto` (generated gRPC), `spurctld` (controller), `spurd` (agent), `spur-cli` (CLI), `spur-tests` (integration).
- **Cargo PATH:** every shell must `source "$HOME/.cargo/env"` before `cargo`.
- **Proto regen gotcha:** after editing `proto/slurm.proto`, codegen does NOT always refresh on branch state. Force it:
  `touch proto/slurm.proto crates/spur-proto/build.rs && cargo build -p spur-proto`.
- **WAL gotcha:** adding a field/variant to `WalOperation` needs `#[serde(default)]` on new fields for old-log compat, AND every Rust construction + match site must be updated (serde default covers deserialize only). Grep all `WalOperation::` sites.
- **State machine already allows the transitions** we need (`spur-core/src/job.rs` `transition()`): `Running→Suspended`, `Suspended→Running`, `Suspended→Cancelled`. Do NOT re-add these.
- **`JobState::Suspended` and proto `JOB_SUSPENDED` already exist** and round-trip. Do NOT re-add.
- **Existing agent signal primitive:** `RunningJob::kill_signal(Signal)` in `crates/spurd/src/executor.rs:147` handles managed PIDs and container process-trees. Reuse it for SIGSTOP/SIGCONT.
- **No `unwrap()` in library code; `anyhow::Result` in app code. No test timeouts, no mocks of the unit under test, no network/DB in unit tests.**
- **Commit style:** conventional commits, e.g. `feat(spur): ...`. Include the `Co-Authored-By` trailer used elsewhere on this branch.

---

## File structure

- `proto/slurm.proto` — add 2 controller RPCs + 2 request msgs; 1 agent RPC + 1 request msg.
- `crates/spur-core/src/job.rs` — 2 new `Job` fields, `run_time()` update, new `effective_deadline()`.
- `crates/spur-core/src/wal.rs` — 2 new `WalOperation` variants.
- `crates/spurctld/src/cluster.rs` — `suspend_job`/`resume_job` methods; WAL apply arms for the 2 new ops.
- `crates/spurctld/src/server.rs` — `suspend_job`/`resume_job` RPC handlers.
- `crates/spurctld/src/scheduler_loop.rs` — `send_suspend_to_agents`; deadline uses `effective_deadline`.
- `crates/spurd/src/agent_server.rs` — `suspend_job` agent RPC handler.
- `crates/spur-cli/src/scontrol.rs` — `Suspend`/`Resume` subcommands + dispatch.
- `crates/spur-tests/src/t60_suspend.rs` (+ register in `lib.rs`) — end-to-end.

---

## Task 1: Core — `Job` fields + `run_time`/`effective_deadline`

**Files:**
- Modify: `crates/spur-core/src/job.rs` (struct ~424-458, `new()` ~477-494, `run_time()` ~515-519)
- Test: same file `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `crates/spur-core/src/job.rs` (uses existing `make_job()` helper):

```rust
#[test]
fn suspended_time_excluded_from_run_time() {
    let mut job = make_job();
    job.start_time = Some(Utc::now() - chrono::Duration::seconds(100));
    job.end_time = Some(Utc::now());
    job.suspended_secs = 30;
    // 100s wall, 30s suspended => ~70s run time
    let rt = job.run_time().unwrap().num_seconds();
    assert!((68..=72).contains(&rt), "expected ~70s, got {rt}");
}

#[test]
fn in_progress_suspension_excluded_from_run_time() {
    let mut job = make_job();
    job.start_time = Some(Utc::now() - chrono::Duration::seconds(100));
    job.end_time = None;
    job.suspended_at = Some(Utc::now() - chrono::Duration::seconds(40));
    // currently suspended for ~40s of the 100s elapsed => ~60s run time
    let rt = job.run_time().unwrap().num_seconds();
    assert!((58..=62).contains(&rt), "expected ~60s, got {rt}");
}

#[test]
fn effective_deadline_extends_by_suspended_time() {
    let mut job = make_job();
    let start = Utc::now();
    job.suspended_secs = 50;
    let dl = job.effective_deadline(start, chrono::Duration::seconds(100));
    // 100s limit + 50s suspended => 150s after start
    assert_eq!((dl - start).num_seconds(), 150);
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p spur-core suspended_time_excluded -- --nocapture`
Expected: FAIL — `no field suspended_secs` / `no method effective_deadline`.

- [ ] **Step 3: Add fields to `Job` struct**

In `crates/spur-core/src/job.rs`, inside `pub struct Job`, after `node_completions` (the last field, ~457):

```rust
    /// Wall-clock instant the job entered Suspended (None unless currently suspended).
    #[serde(default)]
    pub suspended_at: Option<DateTime<Utc>>,
    /// Total seconds spent suspended across all suspend/resume cycles.
    #[serde(default)]
    pub suspended_secs: i64,
```

In `Job::new()`, after `node_completions: HashMap::new(),`:

```rust
            suspended_at: None,
            suspended_secs: 0,
```

- [ ] **Step 4: Update `run_time()` and add `effective_deadline()`**

Replace the body of `run_time()` (~515-519) with:

```rust
    pub fn run_time(&self) -> Option<chrono::Duration> {
        let start = self.start_time?;
        let end = self.end_time.unwrap_or_else(Utc::now);
        let mut suspended = self.suspended_secs;
        if let Some(since) = self.suspended_at {
            suspended += (end - since).num_seconds().max(0);
        }
        Some((end - start) - chrono::Duration::seconds(suspended))
    }

    /// Wall-clock deadline for time-limit enforcement, pushed out by time spent
    /// suspended so a job regains its full budget after resume (Slurm parity).
    pub fn effective_deadline(
        &self,
        start: DateTime<Utc>,
        time_limit: chrono::Duration,
    ) -> DateTime<Utc> {
        let mut suspended = self.suspended_secs;
        if let Some(since) = self.suspended_at {
            suspended += (Utc::now() - since).num_seconds().max(0);
        }
        start + time_limit + chrono::Duration::seconds(suspended)
    }
```

- [ ] **Step 5: Run tests, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p spur-core suspend`
Expected: PASS (3 tests). Also run `cargo build -p spur-core`.

- [ ] **Step 6: Commit**

```bash
git add crates/spur-core/src/job.rs
git commit -m "$(cat <<'EOF'
feat(spur): track suspended time on Job for run-time/deadline accounting

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: WAL — `JobSuspend` / `JobResume` variants + serde round-trip

**Files:**
- Modify: `crates/spur-core/src/wal.rs` (enum ~18-51)
- Test: same file (add a `#[cfg(test)]` round-trip test, or extend existing)

- [ ] **Step 1: Write failing test**

Append to `crates/spur-core/src/wal.rs` (create a `tests` module if none exists; check first with `grep -n "mod tests" crates/spur-core/src/wal.rs`):

```rust
#[cfg(test)]
mod suspend_wal_tests {
    use super::*;

    #[test]
    fn suspend_resume_ops_round_trip() {
        let at = chrono::Utc::now();
        for op in [
            WalOperation::JobSuspend { job_id: 7, at },
            WalOperation::JobResume { job_id: 7, at },
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let back: WalOperation = serde_json::from_str(&json).unwrap();
            match (op, back) {
                (WalOperation::JobSuspend { job_id: a, .. }, WalOperation::JobSuspend { job_id: b, .. }) => assert_eq!(a, b),
                (WalOperation::JobResume { job_id: a, .. }, WalOperation::JobResume { job_id: b, .. }) => assert_eq!(a, b),
                _ => panic!("variant mismatch after round-trip"),
            }
        }
    }
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p spur-core suspend_resume_ops_round_trip`
Expected: FAIL — `no variant JobSuspend`.

- [ ] **Step 3: Add the variants**

In `crates/spur-core/src/wal.rs`, after the `JobPriorityChange { ... },` variant (~51), inside the `// Job operations` group:

```rust
    JobSuspend {
        job_id: JobId,
        /// Controller-stamped instant of suspension (for replay-deterministic accounting).
        at: chrono::DateTime<chrono::Utc>,
    },
    JobResume {
        job_id: JobId,
        /// Controller-stamped instant of resume.
        at: chrono::DateTime<chrono::Utc>,
    },
```

(`JobId` is already imported in this file; `chrono` is a workspace dep — fully-qualify as above to avoid touching imports.)

- [ ] **Step 4: Run test, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p spur-core suspend_resume_ops_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/spur-core/src/wal.rs
git commit -m "$(cat <<'EOF'
feat(spur): add JobSuspend/JobResume WAL operations

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Cluster — WAL apply arms + `suspend_job`/`resume_job` methods

**Files:**
- Modify: `crates/spurctld/src/cluster.rs` (apply match ~1706; add methods near `cancel_job` ~363)
- Test: same file `#[cfg(test)] mod tests` (uses existing `apply_operation` test pattern ~2373+)

- [ ] **Step 1: Write failing tests**

Find the test module setup pattern first: `grep -n "fn make_cluster\|fn test_cluster\|ClusterManager::" crates/spurctld/src/cluster.rs | head`. Use the same constructor the existing apply tests use. Add a test mirroring the existing `apply_operation(&WalOperation::JobSubmit{..})` → `JobStart` pattern (see ~2410-2455):

```rust
#[test]
fn apply_suspend_then_resume_accumulates_suspended_secs() {
    let cm = /* same constructor as existing apply tests in this module */;
    let spec = JobSpec { name: "s".into(), user: "u".into(), ..Default::default() };
    cm.apply_operation(&WalOperation::JobSubmit { job_id: 1, spec: Box::new(spec) });
    cm.apply_operation(&WalOperation::JobStateChange {
        job_id: 1, old_state: JobState::Pending, new_state: JobState::Running,
    });
    let t0 = chrono::Utc::now();
    cm.apply_operation(&WalOperation::JobSuspend { job_id: 1, at: t0 });
    assert_eq!(cm.get_job(1).unwrap().state, JobState::Suspended);
    cm.apply_operation(&WalOperation::JobResume {
        job_id: 1, at: t0 + chrono::Duration::seconds(25),
    });
    let job = cm.get_job(1).unwrap();
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.suspended_secs, 25);
    assert!(job.suspended_at.is_none());
}
```

> NOTE: match the exact constructor used by sibling tests (e.g. `make_cluster()`); replace the `/* ... */` placeholder with it. Do not invent a new constructor.

- [ ] **Step 2: Run test, verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p spurctld apply_suspend_then_resume`
Expected: FAIL — non-exhaustive match / unknown variant.

- [ ] **Step 3: Add WAL apply arms**

In `crates/spurctld/src/cluster.rs`, inside the first `apply_operation` match (after the `JobStateChange` arm, ~1724), add:

```rust
            WalOperation::JobSuspend { job_id, at } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    if let Err(e) = job.transition(JobState::Suspended) {
                        warn!(job_id = *job_id, error = %e, "invalid suspend transition in WAL apply");
                    } else {
                        job.suspended_at = Some(*at);
                    }
                }
            }
            WalOperation::JobResume { job_id, at } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    if let Some(since) = job.suspended_at.take() {
                        job.suspended_secs += (*at - since).num_seconds().max(0);
                    }
                    if let Err(e) = job.transition(JobState::Running) {
                        warn!(job_id = *job_id, error = %e, "invalid resume transition in WAL apply");
                    }
                }
            }
```

> If a second `apply_operation` exists (snapshot-restore path ~2043), check whether it delegates or duplicates the match; if it duplicates, add the same two arms there. Verify with `grep -n "fn apply_operation" crates/spurctld/src/cluster.rs`.

- [ ] **Step 4: Add `suspend_job`/`resume_job` methods**

After `cancel_job` (~388) in `crates/spurctld/src/cluster.rs`:

```rust
    /// Suspend a running job: validate state, record through Raft. Allocation is retained.
    pub fn suspend_job(&self, job_id: JobId, _user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Running {
                anyhow::bail!("job {} is not running (state {:?})", job_id, job.state);
            }
        }
        self.propose(WalOperation::JobSuspend { job_id, at: chrono::Utc::now() })?;
        info!(job_id, "job suspended");
        Ok(())
    }

    /// Resume a suspended job: validate state, record through Raft, fold suspended time.
    pub fn resume_job(&self, job_id: JobId, _user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Suspended {
                anyhow::bail!("job {} is not suspended (state {:?})", job_id, job.state);
            }
        }
        self.propose(WalOperation::JobResume { job_id, at: chrono::Utc::now() })?;
        info!(job_id, "job resumed");
        Ok(())
    }
```

> Confirm `info!` is already imported in this file (the `cancel_job` method uses it).

- [ ] **Step 5: Run tests, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p spurctld apply_suspend_then_resume`
Expected: PASS. Also `cargo build -p spurctld`.

- [ ] **Step 6: Commit**

```bash
git add crates/spurctld/src/cluster.rs
git commit -m "$(cat <<'EOF'
feat(spur): cluster suspend_job/resume_job with WAL apply

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Proto — controller + agent RPCs

**Files:**
- Modify: `proto/slurm.proto` (controller service ~253-256; agent service ~299-301; message blocks)

- [ ] **Step 1: Add controller RPCs**

In `proto/slurm.proto`, inside `service SlurmController`, after `rpc CancelJob(...)` (~256):

```proto
  rpc SuspendJob(SuspendJobRequest) returns (google.protobuf.Empty);
  rpc ResumeJob(ResumeJobRequest) returns (google.protobuf.Empty);
```

- [ ] **Step 2: Add agent RPC**

Inside `service SlurmAgent`, after `rpc CancelJob(AgentCancelJobRequest)...` (~300):

```proto
  rpc SuspendJob(AgentSuspendJobRequest) returns (google.protobuf.Empty);
```

- [ ] **Step 3: Add request messages**

After `message CancelJobRequest { ... }` (~367-371):

```proto
message SuspendJobRequest {
  uint32 job_id = 1;
  string user = 2;   // Advisory (parity/forward-compat), not enforced.
}

message ResumeJobRequest {
  uint32 job_id = 1;
  string user = 2;
}
```

After `message AgentCancelJobRequest { ... }` (~468-471):

```proto
message AgentSuspendJobRequest {
  uint32 job_id = 1;
  bool resume = 2;   // false = SIGSTOP (suspend), true = SIGCONT (resume)
}
```

- [ ] **Step 4: Regenerate proto and verify it compiles**

Run:
```bash
source "$HOME/.cargo/env"
touch proto/slurm.proto crates/spur-proto/build.rs
cargo build -p spur-proto
```
Expected: builds clean. Verify generated symbols exist:
`grep -rl "SuspendJobRequest" target/debug/build/spur-proto-*/out/slurm.rs`
Expected: at least one match.

- [ ] **Step 5: Commit**

```bash
git add proto/slurm.proto
git commit -m "$(cat <<'EOF'
feat(spur): add SuspendJob/ResumeJob RPCs to proto

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Agent — `suspend_job` RPC handler (SIGSTOP/SIGCONT)

**Files:**
- Modify: `crates/spurd/src/agent_server.rs` (trait impl ~375; cancel handler ~808-822; helper near `send_explicit_signal` ~1377)
- Test: same file `#[cfg(test)]` (mirror `send_explicit_signal_kills_job` ~1819)

- [ ] **Step 1: Write failing test**

Find the existing signal test helper pattern: `grep -n "fn spawn_test_job\|async fn send_explicit_signal_kills_job\|fn make_service\|fn proc_state" crates/spurd/src/agent_server.rs`. Mirror `send_explicit_signal_kills_job` to add (read `/proc/<pid>/stat` field 3 for the process state char — `T` = stopped):

```rust
#[tokio::test]
async fn suspend_then_resume_toggles_process_state() {
    // Spawn a real long-running child via the same harness the cancel tests use.
    // (Replace spawn/lookup with the exact helper used by send_explicit_signal_kills_job.)
    let (svc, job_id, pid) = /* same setup as send_explicit_signal_kills_job, capturing pid */;

    svc.suspend_signal(job_id, false).await; // SIGSTOP
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(proc_state(pid), 'T', "process should be stopped after SIGSTOP");

    svc.suspend_signal(job_id, true).await;  // SIGCONT
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(matches!(proc_state(pid), 'R' | 'S'), "process should run after SIGCONT");

    svc.send_explicit_signal(job_id, 9).await; // cleanup
}

#[cfg(test)]
fn proc_state(pid: i32) -> char {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // field 3 is the state char, after "comm" which is parenthesized
    let after = stat.rsplit(')').next().unwrap();
    after.split_whitespace().next().unwrap().chars().next().unwrap()
}
```

> Replace the `/* ... */` with the literal harness used by the sibling test. If that harness does not expose the child pid, extend it minimally to return it. Do NOT mock — these tests must signal a real OS process.

- [ ] **Step 2: Run test, verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p spurd suspend_then_resume_toggles`
Expected: FAIL — `no method suspend_signal`.

- [ ] **Step 3: Add the `suspend_signal` helper**

After `send_explicit_signal` (~1386) in `crates/spurd/src/agent_server.rs`:

```rust
    /// Freeze (SIGSTOP) or thaw (SIGCONT) a running job's process(es).
    async fn suspend_signal(&self, job_id: u32, resume: bool) {
        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
        let sig = if resume {
            nix::sys::signal::Signal::SIGCONT
        } else {
            nix::sys::signal::Signal::SIGSTOP
        };
        info!(job_id, resume, "sending suspend/resume signal to job");
        let _ = tracked.job.kill_signal(sig);
    }
```

- [ ] **Step 4: Add the RPC handler**

In the `impl SlurmAgent for AgentService` block, after `cancel_job` (~822):

```rust
    async fn suspend_job(
        &self,
        request: Request<AgentSuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.suspend_signal(req.job_id, req.resume).await;
        Ok(Response::new(()))
    }
```

- [ ] **Step 5: Run tests, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p spurd suspend`
Expected: PASS. Also `cargo build -p spurd`.

- [ ] **Step 6: Commit**

```bash
git add crates/spurd/src/agent_server.rs
git commit -m "$(cat <<'EOF'
feat(spur): agent SuspendJob handler issues SIGSTOP/SIGCONT

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Controller — dispatch helper + RPC handlers

**Files:**
- Modify: `crates/spurctld/src/scheduler_loop.rs` (near `send_cancel_to_agents` ~933; imports ~13)
- Modify: `crates/spurctld/src/server.rs` (after `cancel_job` ~256)

- [ ] **Step 1: Add `send_suspend_to_agents`**

In `crates/spurctld/src/scheduler_loop.rs`, ensure `AgentSuspendJobRequest` is imported (add to the existing `use spur_proto::proto::{...}` list at ~13). After `send_cancel_to_agents` (ends ~982), add:

```rust
/// Dispatch suspend (SIGSTOP) or resume (SIGCONT) to every allocated node.
pub async fn send_suspend_to_agents(
    cluster: &Arc<ClusterManager>,
    job: &spur_core::job::Job,
    resume: bool,
) {
    for node_name in &job.allocated_nodes {
        let node_info = cluster.get_node(node_name);
        let (addr, port) = match node_info {
            Some(ref n) if n.address.is_some() => (n.address.clone().unwrap(), n.port),
            _ => {
                warn!(job_id = job.job_id, node = %node_name,
                    "no agent address — cannot suspend/resume job on node");
                continue;
            }
        };
        let agent_addr = format!("http://{}:{}", addr, port);
        let job_id = job.job_id;
        tokio::spawn(async move {
            match SlurmAgentClient::connect(agent_addr.clone()).await {
                Ok(mut client) => {
                    if let Err(e) = client
                        .suspend_job(AgentSuspendJobRequest { job_id, resume })
                        .await
                    {
                        warn!(job_id, resume, agent = %agent_addr, error = %e, "SuspendJob RPC failed");
                    } else {
                        info!(job_id, resume, agent = %agent_addr, "sent SuspendJob");
                    }
                }
                Err(e) => {
                    warn!(job_id, agent = %agent_addr, error = %e,
                        "failed to connect to agent for suspend/resume");
                }
            }
        });
    }
}
```

- [ ] **Step 2: Add controller RPC handlers**

In `crates/spurctld/src/server.rs`, after `cancel_job` (~256), add (mirrors cancel's leader-forward + spawn-dispatch shape):

```rust
    async fn suspend_job(
        &self,
        request: Request<SuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.suspend_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward suspend_job to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let job_id = req.job_id;
        let job = self.cluster.get_job(job_id);
        self.cluster
            .suspend_job(job_id, &req.user)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        if let Some(job) = job {
            let cluster = self.cluster.clone();
            tokio::spawn(async move {
                crate::scheduler_loop::send_suspend_to_agents(&cluster, &job, false).await;
            });
        }
        Ok(Response::new(()))
    }

    async fn resume_job(
        &self,
        request: Request<ResumeJobRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.resume_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward resume_job to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let job_id = req.job_id;
        // Re-snapshot AFTER the state flip so allocated_nodes reflect the resumed job.
        self.cluster
            .resume_job(job_id, &req.user)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        if let Some(job) = self.cluster.get_job(job_id) {
            let cluster = self.cluster.clone();
            tokio::spawn(async move {
                crate::scheduler_loop::send_suspend_to_agents(&cluster, &job, true).await;
            });
        }
        Ok(Response::new(()))
    }
```

- [ ] **Step 3: Build, verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p spurctld`
Expected: clean build (trait now fully implemented).

- [ ] **Step 4: Commit**

```bash
git add crates/spurctld/src/scheduler_loop.rs crates/spurctld/src/server.rs
git commit -m "$(cat <<'EOF'
feat(spur): controller SuspendJob/ResumeJob handlers + agent dispatch

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Timeout enforcer uses `effective_deadline`

**Files:**
- Modify: `crates/spurctld/src/scheduler_loop.rs` (~726)

- [ ] **Step 1: Update the deadline calculation**

In `enforce_time_limits`, replace (~726):

```rust
            let deadline = start_time + time_limit;
```

with:

```rust
            let deadline = job.effective_deadline(start_time, time_limit);
```

- [ ] **Step 2: Build**

Run: `source "$HOME/.cargo/env" && cargo build -p spurctld`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/spurctld/src/scheduler_loop.rs
git commit -m "$(cat <<'EOF'
feat(spur): exclude suspended time from time-limit enforcement

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: CLI — `scontrol suspend|resume`

**Files:**
- Modify: `crates/spur-cli/src/scontrol.rs` (enum ~50-54; dispatch ~155-170)

- [ ] **Step 1: Add clap variants**

In `enum ScontrolCommand`, after `Requeue { job_id: u32 }` (~54):

```rust
    /// Suspend a running job (SIGSTOP, retains allocation)
    Suspend {
        /// Job ID
        job_id: u32,
    },
    /// Resume a suspended job (SIGCONT)
    Resume {
        /// Job ID
        job_id: u32,
    },
```

- [ ] **Step 2: Add dispatch arms**

In the `match args.command` block, after the `Requeue { .. }` arm (~170):

```rust
        ScontrolCommand::Suspend { job_id } => {
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .suspend_job(spur_proto::proto::SuspendJobRequest {
                    job_id,
                    user: String::new(),
                })
                .await
                .context("suspend failed")?;
            println!("job {} suspended", job_id);
            Ok(())
        }
        ScontrolCommand::Resume { job_id } => {
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .resume_job(spur_proto::proto::ResumeJobRequest {
                    job_id,
                    user: String::new(),
                })
                .await
                .context("resume failed")?;
            println!("job {} resumed", job_id);
            Ok(())
        }
```

- [ ] **Step 3: Build + smoke-test arg parsing**

Run: `source "$HOME/.cargo/env" && cargo build -p spur-cli`
Expected: clean. Then:
`cargo run -p spur-cli --bin spur -- scontrol suspend --help 2>&1 | head -3`
Expected: help text mentioning the job ID arg (no connection attempt for `--help`).

- [ ] **Step 4: Commit**

```bash
git add crates/spur-cli/src/scontrol.rs
git commit -m "$(cat <<'EOF'
feat(spur): scontrol suspend|resume subcommands

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Integration test (spur-tests)

**Files:**
- Create: `crates/spur-tests/src/t60_suspend.rs`
- Modify: `crates/spur-tests/src/lib.rs` (register module)

- [ ] **Step 1: Inspect an existing integration test for the harness API**

Run: `sed -n '1,60p' crates/spur-tests/src/t06_cancel.rs` and note how it boots the harness, submits a job, and asserts state. Reuse that exact harness (`crates/spur-tests/src/harness.rs`).

- [ ] **Step 2: Write the test**

Create `crates/spur-tests/src/t60_suspend.rs` following the t06_cancel pattern. Skeleton (adapt the harness calls to match t06 exactly — submit a sleep job, wait for Running, suspend, assert SUSPENDED, resume, assert RUNNING, cancel):

```rust
// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! t60: job suspend/resume round-trip.

// Use the same imports/harness entrypoints as t06_cancel.rs.

pub async fn run() -> anyhow::Result<()> {
    // 1. Boot harness (single controller + agent), same as t06_cancel.
    // 2. Submit a long-running job; poll until JobState::Running.
    // 3. Call controller SuspendJob; poll until JobState::Suspended.
    // 4. Call controller ResumeJob; poll until JobState::Running.
    // 5. Cancel; assert terminal.
    // Assert no panic and the two transitions are observed.
    Ok(())
}
```

> Fill the body with the concrete harness calls copied from `t06_cancel.rs` (same client construction, same poll helper). Do not introduce a new polling abstraction.

- [ ] **Step 3: Register the module**

In `crates/spur-tests/src/lib.rs`, add `pub mod t60_suspend;` alongside the other `pub mod tNN_*;` lines, and wire it into the test runner the same way t06 is wired (check how `lib.rs` dispatches — `grep -n "t06_cancel" crates/spur-tests/src/lib.rs`).

- [ ] **Step 4: Run it**

Run: `source "$HOME/.cargo/env" && cargo test -p spur-tests t60 -- --nocapture` (or the suite's runner entrypoint if t06 is invoked via a single `#[tokio::test]` dispatcher — match that mechanism).
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/spur-tests/src/t60_suspend.rs crates/spur-tests/src/lib.rs
git commit -m "$(cat <<'EOF'
test(spur): end-to-end suspend/resume integration test

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Full build, fmt, clippy, whole-suite gate

- [ ] **Step 1: Format**

Run: `source "$HOME/.cargo/env" && cargo fmt --all && git diff --stat`
If fmt changed files, review then `git commit -am "style(spur): cargo fmt"`.

- [ ] **Step 2: Clippy (workspace, deny warnings)**

Run: `source "$HOME/.cargo/env" && cargo clippy --all-targets --all-features 2>&1 | tail -30`
Expected: no warnings. Fix any, then commit.

- [ ] **Step 3: Full test suite**

Run: `source "$HOME/.cargo/env" && cargo test 2>&1 | grep -E "test result|error\["`
Expected: every `test result:` line shows `0 failed`. (Remember: a trailing `grep -c` with 0 matches returns exit 1 — read the actual lines.)

- [ ] **Step 4: Final verification commit (if anything changed)**

```bash
git add -A && git commit -m "$(cat <<'EOF'
chore(spur): fmt/clippy clean for suspend/resume

Co-Authored-By: Claude Opus 4 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Live verification on testbed (147 vs 145)

> Manual / agent-driven. Deploy per `lab-testbed` memory: build release for the affected crates, kill daemons by PID, scp binaries to `~/.local/bin/` on 147, restart spurdbd→spurctld→spurd in order. SSH: `sshpass -p vm ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null vm@10.11.98.147`.

- [ ] **Step 1: Build release**

Run: `source "$HOME/.cargo/env" && cargo build --release -p spurctld -p spurd -p spur-cli`

- [ ] **Step 2: Deploy to 147** (follow lab-testbed deploy procedure exactly).

- [ ] **Step 3: Verify suspend/resume basics**

On 147: submit a sleep job (script file — no `--wrap`; job id via `grep -oE '[0-9]+' | tail -1`). Then:
```
spur scontrol suspend <id>   # expect: JobState=SUSPENDED
# on the node: ps -o pid,stat -p <pid>  => state contains 'T'
spur scontrol resume <id>    # expect: JobState=RUNNING; stat back to S/R
```

- [ ] **Step 4: Verify accounting parity vs Slurm 25.11.6 on 145**

For both schedulers: submit a job with a short `--time`, suspend it across a span longer than would normally trip the limit, resume, and confirm it does NOT time out while suspended and that `scontrol show job` run-time excludes the suspended window. Record a small parity table (State / run-time behavior) for the PR description.

- [ ] **Step 5: Capture results** into the PR description (parity table + the two documented behaviors: allocation retained, StartTime stable).

---

## Self-review notes (coverage map)

- Spec §1 Proto → Task 4. §2 Core fields/run_time/effective_deadline → Task 1. §3 WAL ops → Task 2 + apply in Task 3. §4 Controller methods+handlers → Task 3 (cluster) + Task 6 (server/dispatch). §5 Timeout enforcer → Task 7. §6 Agent → Task 5. §7 CLI → Task 8. §8 Tests → Tasks 1,2,3,5,9 + Task 10 gate. Live verification → Task 11.
- All three scope decisions honored: full accounting (Tasks 1,7), advisory `user` (Task 3 `_user`), retain allocation (Task 3 — no dealloc; uses JobSuspend not JobComplete).
- Ordering note: Tasks 1–3 (core/wal/cluster) precede Task 4 (proto) deliberately so each compiles independently; Task 6 (handlers referencing proto types) comes after Task 4. Agent (Task 5) also after Task 4.
