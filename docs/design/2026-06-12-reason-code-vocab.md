# Pending-reason vocabulary expansion (Slurm 25.11 parity)

Date: 2026-06-12
Scope: `crates/spur-core` (`PendingReason` enum), `crates/spur-core/src/qos.rs` (emission fixes), tests.
Category-4 parity: expand `PendingReason` toward Slurm's `enum job_state_reason` / `job_reason_string()`.

## Goal

Spur's `PendingReason` (in `crates/spur-core/src/job.rs`) carried 14 variants. Slurm ships ~120
reason codes. The reason string is surfaced in the `state_reason` proto field and rendered verbatim
by `squeue -r`/`scontrol show job`, so Slurm-compat clients and CI gates pattern-match on it. This
change adds a coherent, high-value set of the missing reason families with strings verified
byte-for-byte against live Slurm 25.11.6.

## Live evidence (Slurm 25.11.6, host <slurm-host>)

- `slurmctld -V` → `slurm 25.11.6`.
- Authoritative string table: `/tmp/slurm-25.11.6/src/common/job_state_reason.c`, the `jsra[]`
  array indexed by `enum job_state_reason`, consumed by `job_reason_string()`
  (`strong_alias(job_state_reason_string, slurm_job_state_reason_string)`).
- Enum constants: `/usr/local/include/slurm/slurm.h` (`enum job_state_reason`).
- Byte-exactness double-checked against the **compiled** object that defines the function:
  `strings /tmp/slurm-25.11.6/src/common/job_state_reason.o | grep -xF "<string>"` returned an
  exact, whole-line match for every string below (the table-defining object; `libslurmfull.so`
  merges some literals via string-pooling, so `.o` is the reliable witness).

Each variant's exact `.str` value was read directly from `job_state_reason.c`:

```
[WAIT_RESERVATION]            = { .str = "Reservation" }
[WAIT_PART_CONFIG]            = { .str = "PartitionConfig" }
[FAIL_SYSTEM]                 = { .str = "SystemFailure" }
[WAIT_ACCOUNT_POLICY]         = { .str = "AccountingPolicy" }
[WAIT_ASSOC_JOB_LIMIT]        = { .str = "AssociationJobLimit" }
[WAIT_ASSOC_RESOURCE_LIMIT]   = { .str = "AssociationResourceLimit" }
[WAIT_ASSOC_TIME_LIMIT]       = { .str = "AssociationTimeLimit" }
[WAIT_ASSOC_GRP_CPU]          = { .str = "AssocGrpCpuLimit" }
[WAIT_ASSOC_GRP_MEM]          = { .str = "AssocGrpMemLimit" }
[WAIT_ASSOC_GRP_NODE]         = { .str = "AssocGrpNodeLimit" }
[WAIT_ASSOC_GRP_JOB]          = { .str = "AssocGrpJobsLimit" }
[WAIT_ASSOC_GRP_SUB_JOB]      = { .str = "AssocGrpSubmitJobsLimit" }
[WAIT_ASSOC_GRP_WALL]         = { .str = "AssocGrpWallLimit" }
[WAIT_ASSOC_MAX_JOBS]         = { .str = "AssocMaxJobsLimit" }
[WAIT_ASSOC_MAX_CPU_PER_JOB]  = { .str = "AssocMaxCpuPerJobLimit" }
[WAIT_ASSOC_MAX_NODE_PER_JOB] = { .str = "AssocMaxNodePerJobLimit" }
[WAIT_ASSOC_MAX_WALL_PER_JOB] = { .str = "AssocMaxWallDurationPerJobLimit" }
[WAIT_QOS_JOB_LIMIT]          = { .str = "QOSJobLimit" }
[WAIT_QOS_RESOURCE_LIMIT]     = { .str = "QOSResourceLimit" }
[WAIT_QOS_TIME_LIMIT]         = { .str = "QOSTimeLimit" }
[WAIT_QOS_GRP_CPU]            = { .str = "QOSGrpCpuLimit" }
[WAIT_QOS_GRP_MEM]            = { .str = "QOSGrpMemLimit" }
[WAIT_QOS_GRP_NODE]           = { .str = "QOSGrpNodeLimit" }
[WAIT_QOS_GRP_JOB]            = { .str = "QOSGrpJobsLimit" }
[WAIT_QOS_GRP_SUB_JOB]        = { .str = "QOSGrpSubmitJobsLimit" }
[WAIT_QOS_GRP_WALL]           = { .str = "QOSGrpWallLimit" }
[WAIT_QOS_MAX_CPU_PER_JOB]    = { .str = "QOSMaxCpuPerJobLimit" }
[WAIT_QOS_MAX_NODE_PER_JOB]   = { .str = "QOSMaxNodePerJobLimit" }
[WAIT_QOS_MAX_WALL_PER_JOB]   = { .str = "QOSMaxWallDurationPerJobLimit" }
[WAIT_QOS_MAX_MEM_PER_JOB]    = { .str = "QOSMaxMemoryPerJob" }
[WAIT_QOS_MAX_CPU_PER_USER]   = { .str = "QOSMaxCpuPerUserLimit" }
[WAIT_QOS_MAX_SUB_JOB]        = { .str = "QOSMaxSubmitJobPerUserLimit" }
[WAIT_BURST_BUFFER_RESOURCE]  = { .str = "BurstBufferResources" }
[WAIT_BURST_BUFFER_STAGING]   = { .str = "BurstBufferStageIn" }
```

## Variants added (33)

Rust variant → exact Slurm 25.11 string → `enum job_state_reason` constant:

| Rust variant | Slurm string | Slurm enum |
|---|---|---|
| `Reservation` | `Reservation` | `WAIT_RESERVATION` |
| `PartitionConfig` | `PartitionConfig` | `WAIT_PART_CONFIG` |
| `SystemFailure` | `SystemFailure` | `FAIL_SYSTEM` |
| `AccountingPolicy` | `AccountingPolicy` | `WAIT_ACCOUNT_POLICY` |
| `AssociationJobLimit` | `AssociationJobLimit` | `WAIT_ASSOC_JOB_LIMIT` |
| `AssociationResourceLimit` | `AssociationResourceLimit` | `WAIT_ASSOC_RESOURCE_LIMIT` |
| `AssociationTimeLimit` | `AssociationTimeLimit` | `WAIT_ASSOC_TIME_LIMIT` |
| `AssocGrpCpuLimit` | `AssocGrpCpuLimit` | `WAIT_ASSOC_GRP_CPU` |
| `AssocGrpMemLimit` | `AssocGrpMemLimit` | `WAIT_ASSOC_GRP_MEM` |
| `AssocGrpNodeLimit` | `AssocGrpNodeLimit` | `WAIT_ASSOC_GRP_NODE` |
| `AssocGrpJobsLimit` | `AssocGrpJobsLimit` | `WAIT_ASSOC_GRP_JOB` |
| `AssocGrpSubmitJobsLimit` | `AssocGrpSubmitJobsLimit` | `WAIT_ASSOC_GRP_SUB_JOB` |
| `AssocGrpWallLimit` | `AssocGrpWallLimit` | `WAIT_ASSOC_GRP_WALL` |
| `AssocMaxJobsLimit` | `AssocMaxJobsLimit` | `WAIT_ASSOC_MAX_JOBS` |
| `AssocMaxCpuPerJobLimit` | `AssocMaxCpuPerJobLimit` | `WAIT_ASSOC_MAX_CPU_PER_JOB` |
| `AssocMaxNodePerJobLimit` | `AssocMaxNodePerJobLimit` | `WAIT_ASSOC_MAX_NODE_PER_JOB` |
| `AssocMaxWallDurationPerJobLimit` | `AssocMaxWallDurationPerJobLimit` | `WAIT_ASSOC_MAX_WALL_PER_JOB` |
| `QosJobLimit` | `QOSJobLimit` | `WAIT_QOS_JOB_LIMIT` |
| `QosResourceLimit` | `QOSResourceLimit` | `WAIT_QOS_RESOURCE_LIMIT` |
| `QosTimeLimit` | `QOSTimeLimit` | `WAIT_QOS_TIME_LIMIT` |
| `QosGrpCpuLimit` | `QOSGrpCpuLimit` | `WAIT_QOS_GRP_CPU` |
| `QosGrpMemLimit` | `QOSGrpMemLimit` | `WAIT_QOS_GRP_MEM` |
| `QosGrpNodeLimit` | `QOSGrpNodeLimit` | `WAIT_QOS_GRP_NODE` |
| `QosGrpJobsLimit` | `QOSGrpJobsLimit` | `WAIT_QOS_GRP_JOB` |
| `QosGrpSubmitJobsLimit` | `QOSGrpSubmitJobsLimit` | `WAIT_QOS_GRP_SUB_JOB` |
| `QosGrpWallLimit` | `QOSGrpWallLimit` | `WAIT_QOS_GRP_WALL` |
| `QosMaxCpuPerJobLimit` | `QOSMaxCpuPerJobLimit` | `WAIT_QOS_MAX_CPU_PER_JOB` |
| `QosMaxNodePerJobLimit` | `QOSMaxNodePerJobLimit` | `WAIT_QOS_MAX_NODE_PER_JOB` |
| `QosMaxWallDurationPerJobLimit` | `QOSMaxWallDurationPerJobLimit` | `WAIT_QOS_MAX_WALL_PER_JOB` |
| `QosMaxMemoryPerJob` | `QOSMaxMemoryPerJob` | `WAIT_QOS_MAX_MEM_PER_JOB` |
| `QosMaxCpuPerUserLimit` | `QOSMaxCpuPerUserLimit` | `WAIT_QOS_MAX_CPU_PER_USER` |
| `QosMaxSubmitJobPerUserLimit` | `QOSMaxSubmitJobPerUserLimit` | `WAIT_QOS_MAX_SUB_JOB` |
| `BurstBufferResources` | `BurstBufferResources` | `WAIT_BURST_BUFFER_RESOURCE` |
| `BurstBufferStageIn` | `BurstBufferStageIn` | `WAIT_BURST_BUFFER_STAGING` |

Note on casing: Slurm is intentionally inconsistent (`AssocGrpCpuLimit` vs `QOSGrpCpuLimit`,
`Association*` for the coarse trio vs `Assoc*` for the Grp/Max family). The strings above reproduce
the source table exactly; the Rust variant names mirror the Slurm token spelling.

## Layers wired

`PendingReason` is plumbed through three layers; all were updated:

1. **Enum + Display** (`crates/spur-core/src/job.rs`): added the 33 variants and their
   `display()` arms returning the exact strings. `impl Display` delegates to `display()`, so
   `format!("{reason}")` is covered too.
2. **serde**: the enum derives `Serialize`/`Deserialize` with no rename attributes (variant name ==
   JSON token), matching the existing 14 variants; new variants inherit the same scheme and
   round-trip losslessly. `pending_reason` is persisted in the Raft `ClusterSnapshot`, so this is
   the durable path.
3. **proto + CLI**: the public API carries the reason as a plain `string state_reason` (proto
   `JobInfo`/`JobBriefInfo`), produced in `crates/spurctld/src/server.rs` via
   `job.pending_reason.display().to_string()`, and rendered verbatim by the CLI
   (`squeue` `%r`/`%R`, `scontrol show job`, `salloc`). Because the wire format is the Display
   string (no proto enum, no `FromStr` parse-back exists for `PendingReason`), the new variants are
   automatically end-to-end correct once `display()` is right. No proto change was required and the
   proto package name was untouched. There is **no exhaustive `match` on `PendingReason`** anywhere
   in the tree (verified), so adding variants is non-breaking across `spurctld`, `spur-metrics`,
   `spur-k8s`, etc.

## Emission fixes (existing detection sites that emitted wrong/generic reasons)

`crates/spur-core/src/qos.rs` `check_qos_limits()` already *detects* several QOS limit conditions
but reported imprecise reasons. Per the task ("fix wrong/`None` where a new variant is clearly
correct"), these were corrected to the verified Slurm strings — no new detection logic was invented:

| Condition (already detected) | Was | Now |
|---|---|---|
| `max_wall_minutes` exceeded | `PartitionTimeLimit` | `QosMaxWallDurationPerJobLimit` |
| `max_tres_per_job` CPU exceeded | `Resources` | `QosMaxCpuPerJobLimit` |
| `max_tres_per_job` Memory exceeded | `Resources` | `QosMaxMemoryPerJob` |
| `max_tres_per_user` CPU exceeded | `Resources` | `QosMaxCpuPerUserLimit` |
| `max_submit_jobs_per_user` exceeded | `QoSMaxJobsPerUser` (running-job cap) | `QosMaxSubmitJobPerUserLimit` |

The `max_jobs_per_user` path is unchanged (`QoSMaxJobsPerUser` → `QOSMaxJobsPerUserLimit` is already
the correct Slurm string for `WAIT_QOS_MAX_JOB_PER_USER`).

No new emission sites were added for the Assoc-*/Grp-* families: Spur does not yet implement
association-limit or Grp-TRES detection, so those variants are vocabulary-only (intentional — this
task is vocabulary completeness, not new limit enforcement).

## Intentionally skipped

- **The 11 variants already on open PR #274** (`parity/exit-code-signal`), to avoid a duplicate
  conflict: `NonZeroExitCode`, `RaisedSignal`, `JobLaunchFailure`, `JobHeldAdmin`, `BadConstraints`,
  `PartitionInactive`, `DependencyNeverSatisfied`, `InvalidAccount`, `InvalidQOS`, `BootFail`,
  `OutOfMemory`.
- **`FrontEndDown` / `WAIT_FRONT_END`**: OMITTED. Front-end mode was removed from Slurm; there is
  **no** `WAIT_FRONT_END` constant in 25.11.6's `slurm.h` and no `FrontEnd` string in
  `job_state_reason.c`. Adding it would not match any real Slurm output.
- The long tail of TRES sub-families verified-present but lower value for Spur today (e.g.
  `Assoc*Energy*`, `Assoc*GRES*`, `Assoc*License*`, `Assoc*BB*`, `*Minutes*`/`*RunMinutes*`
  variants, `QOS*Unknown*`, `QOSUsageThreshold`, `Prolog`, `Cleaning`, `SchedDefer`, `TimeLimit`,
  `InactiveLimit`, `JobHoldMaxRequeue`, `JobArrayTaskLimit`, `BurstBufferOperation`, fed/account/qos
  "NotAllowed" variants). These are correct in Slurm but Spur has no detection for them and they add
  little immediate parity value; deferred to keep the set coherent. None were omitted due to an
  uncertain string — every string in the table above was confirmed byte-exact.

## Test coverage added

- `crates/spur-core/src/job.rs`:
  - `reason_vocab_display_matches_slurm_25_11` — asserts `display()` and `format!()` equal the exact
    Slurm string for all 33 new variants (table `REASON_VOCAB`).
  - `reason_vocab_serde_roundtrips` — JSON serialize/deserialize round-trip for all 33.
- `crates/spur-core/src/qos.rs`: updated `test_blocked_by_max_wall` and
  `test_blocked_by_max_tres_per_job` to the corrected reasons; added
  `test_blocked_by_max_mem_per_job`, `test_blocked_by_max_cpu_per_user`,
  `test_blocked_by_max_submit_jobs_per_user` (each drives the real `check_qos_limits`).
- `crates/spur-tests/src/t21_acctmgr.rs`: updated `t21_12_qos_max_wall_blocked` and
  `t21_13_qos_tres_per_job_blocked` to the corrected reasons.
- `crates/spur-tests/src/t55_format.rs`: extended `t55_10_pending_reasons_match_slurm` with 10 of
  the new parity strings (CLI-layer rendering check).

## Verification

`cargo build`, `cargo test` (full workspace: 29 test binaries ok, 1229 tests passed, 0 failed),
`cargo clippy --all-targets` (0 warnings), `cargo fmt --all --check` (clean).
</content>

## Update — emission wiring (follow-up commit)

Review + live testbed comparison (Slurm 25.11.6 on .145 vs Spur on .147) found the
vocabulary above was almost entirely **decorative**: Spur emitted none of it, and the
`qos.rs` emission fix was inert because the scheduler discarded the reason. Corrected in a
follow-up commit on this branch:

- **Reservation / Licenses / QoS now surface.** `pending_jobs()` drops jobs blocked by these
  limits *before* `update_pending_reasons()` runs, so their reason was never set. Added
  `tag_blocked_pending_reasons()` (write-locked scheduler pass, mirrors
  `cancel_unsatisfiable_dependency_jobs()`) that sets `Reservation`/`Licenses`/QoS reasons, and
  extracted the eligibility checks into shared helpers (`reservation_block`, `license_block`,
  `qos_block_for`) so the drop decision and the displayed reason cannot diverge.
- **QoS caveat (corrected over-claim).** QoS limits are still not sourced from the accounting DB
  (`Qos::default()` is limitless), so the QoS reasons remain wired-but-inert until QoS loading is
  implemented — the path now carries the specific `QOS*` reason the moment real configs exist.
  Earlier text in this note implied user-visible QoS parity today; that was inaccurate.
- **NodeDown/Resources parity bug fixed.** `update_pending_reasons()` flagged a fully-allocated
  (busy-but-up) cluster as `NodeDown` (it used `is_available()` = Idle|Mixed only). Added
  `NodeState::is_up()` (Idle|Mixed|Allocated); a saturated cluster now reports `Resources`,
  matching Slurm (live-confirmed: Slurm reports `Resources`, old Spur reported `NodeDown`).

Tests added in `crates/spurctld/src/cluster.rs`:
`fully_allocated_cluster_reports_resources_not_nodedown`, `tag_blocked_sets_reservation_reason`,
`tag_blocked_sets_licenses_reason`, `tag_blocked_preserves_held_reason`.
