# spur-tests

Integration and unit test suite for the Spur scheduler. No running daemons
needed -- all tests exercise core types and scheduler logic in-process. CLI
parsers are not exercised from this crate (t17 asserts test_script() output
and checks JobSpec/Partition field defaults; t55 tests `config::format_time`
and display-string conformance).

## Test numbering

Test modules mirror Slurm's testsuite numbering for 1-1 mapping where a
Slurm equivalent exists. Spur-specific groups start at 50.

| Module | Area | Slurm equiv |
|---|---|---|
| `t01_run` | Task distribution, CPU/GPU binding, cons_tres allocation, job steps | srun (test1.x) |
| `t05_queue` | Job state classification, filtering, sort, queue visibility | squeue (test5.x) |
| `t06_cancel` | Hold, release, cancel, state transitions | scancel (test6.x) |
| `t07_sched` | Backfill scheduler, priority ordering, timeline, resource matching | scheduling (test7.x) |
| `t17_submit` | Asserts test_script() output contains directive lines; checks JobSpec/Partition field defaults (no #SBATCH parser runs) | sbatch (test17.x) |
| `t21_acctmgr` | TRES records, QOS limits, accounting data models | sacctmgr (test21.x) |
| `t24_priority` | Priority calculation, fair-share | priority (test24.x) |
| `t28_arrays` | Array spec parsing, task ID assignment, path resolution | job arrays (test28.x) |
| `t28b_array_expand` | Array spec expansion (complement to t28) | -- |
| `t39_gpu` | GRES parsing, GPU type matching, constraint scheduling | GPU/GRES (test39.x) |
| `t50_core` | Job, Node, ResourceSet, Partition types | Spur-only |
| `t51_hostlist` | Hostlist expansion and compression (`node[001-010]`) | Spur-only |
| `t52_config` | TOML config loading, time parsing, partition building | Spur-only |
| `t55_format` | config::format_time and JobState/NodeState/PendingReason display strings (no %X format engine) | Spur-only |
| `t56_deps` | Dependency parsing and satisfaction checking | Spur-only |
| `t57_auth` | JWT token generation/verification (native SPURTKN1 auth not covered here), authorization checks | Spur-only |
| `t58_spank` | SPANK plugin loading, hook invocation, plugstack parsing | Spur-only |
| `t60_suspend` | Suspend/resume guards, run-time accounting | Spur-only |

## Test harness (`harness.rs`)

Fixture builders -- no daemon processes, no network.

- `make_job(name)` -- job with auto-incrementing ID, default partition/user.
- `make_job_with_resources(name, nodes, tasks, cpus_per_task, time_limit)` -- job with specific resource requirements.
- `make_nodes(count, cpus, memory_mb)` -- batch of idle compute nodes.
- `make_partition(name, node_count)` -- partition with hostlist.
- `assert_job_state(job, expected)` -- state assertion.
- `assert_transition_ok(job, to)` / `assert_transition_err(job, to)` -- state machine transition assertions.
- `test_script(directives, body)` -- builds a `#!/bin/bash` script with `#SBATCH` directives.

Job IDs are globally unique within a test run (atomic counter).

## Dependencies

The crate depends on `spur-core`, `spur-sched`, `spur-proto`, `spur-spank`,
`chrono`, and `jsonwebtoken`. No async runtime needed. 12 of 18 test files
do not use `crate::harness` (they exercise types directly).

## Running tests

```bash
# All tests in this crate
cargo test -p spur-tests

# Single module
cargo test -p spur-tests t39_gpu

# Single test
cargo test -p spur-tests t39_1_parse_gpu_full

# With output
cargo test -p spur-tests -- --nocapture
```

E2E tests (Python, require running daemons) live in `tests/{native_host,k8s}/e2e/`,
outside this crate.

## Test patterns

6 of 18 test files use the harness (t05, t06, t07, t17, t50, t60); the other
12 exercise types directly. Files that use the harness follow:

```rust
#[cfg(test)]
mod tests {
    use crate::harness::*;
    // ...

    #[test]
    fn tNN_M_description() { ... }
}
```

- Tests are numbered `tNN_M` where NN is the module group and M is the
  sub-test number. This keeps output greppable and maps to Slurm test IDs.
- Fixtures are built inline or via `harness::make_*`. No shared mutable
  state across tests (the atomic job ID counter is the only exception).
- Scheduler tests build a `ClusterState` struct with nodes, partitions,
  and pending jobs, then call `sched.schedule()` and assert on assignments.
- Auth tests generate/verify JWT tokens and check authorization rules
  (own-job, admin, expired).
- Resource tests use `ResourceSet::can_satisfy()` and
  `NodeAllocation::allocate_for_job()` to verify TRES accounting.
