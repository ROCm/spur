# Accounting Subsystem

Accounting runs inside `spurctld` (`crates/spurctld/src/accounting/`), backed
by PostgreSQL; shared models and QOS checks are in `spur-core`
(`accounting.rs`, `qos.rs`). Operator-facing behaviour (sacctmgr, limit
semantics, GrpWall, audit log, fair-share differences from Slurm) is documented
in `docs/admin-guide/accounting.rst`; this page covers what that page does not.

## Bring-up and outage semantics

- **Startup never blocks on the database.** `accounting::start()` returns the
  gRPC service immediately (reporting `UNAVAILABLE` with the current failure
  cause) and spawns `connect_with_retry`, which retries connect + migrate with
  capped exponential backoff. Only after success does `activate()` install the
  pool and spawn the notifier, reconcile, txn purge, and the four cache-refresh
  loops. Without the retry, a cold start with the database down would leave
  every cache unloaded for the life of the process.
- **Migrations are serialized across controllers.** `db::migrate` takes a
  transaction-scoped `pg_advisory_xact_lock` because it rewrites data and builds
  a unique index, so concurrent controllers sharing one database must not
  overlap. The lock releases even if the apply fails. Its two-int4 key space is
  distinct from the per-user bigint lock `add_user` takes.
- **Fail closed before the first load.** `ClusterManager::accounting_block()`
  holds jobs that name a QOS or account in `Pending` (`AccountingUnavailable`)
  until their caches have loaded once.
- **Fail open afterwards.** The QOS, association, fair-share, and GrpWall caches
  retain stale data when a refresh fails, so a later outage does not re-block
  scheduling. All four refresh on `fairshare_refresh_secs`.
- **GrpWall with no figure is unapplied.** `check_qos_limits` takes
  `consumed_wall_minutes: None` when consumption is unknown and skips the
  `grp_wall_minutes` cap, admitting jobs rather than halting the cluster.
- **Job records.** `AccountingNotifier` writes job start/end with retries; a
  write that exhausts them is left for the reconcile loop to repair.

## Limit semantics

- **`INFINITE` vs `0`.** `spur_core::accounting::INFINITE` (`u32::MAX`) on the
  wire means "no limit" and is stored as SQL `NULL`. A literal `0` means "block
  all". A stray negative in the DB (pre-sentinel data) is treated as unset.
  `limit_from_db()` (DB to in-memory) and `limit_to_wire()` (DB to proto) own
  this mapping.
- **A TRES dimension of `0` is uncapped**, the opposite of count/wall limits.
  `grp_node_is_sole_blocker()` (idle-fill eligibility) relies on this: it
  re-runs the full check with the group node cap zeroed. Never apply that trick
  to wall time, where `0` blocks everything.
- TRES caps compare with strict `>` (at the cap is allowed); `GrpWall` uses `>=`
  (at the budget is blocked).
- `TresRecord::parse` rejects unknown types and unit-suffixed values
  (`mem=1G`) instead of silently dropping them.

## QOS enforcement points

1. **Submit gate** (`check_qos_submit_limits`): submit-count limits always deny,
   independent of `DenyOnLimit`. Resource/wall limits deny at submit only under
   `DenyOnLimit` (`check_qos_standalone_limits`).
2. **Scheduler gate** (`check_qos_limits`): every limit, against real aggregated
   load.
3. **Start check** (`LimitAdmission`, `spurctld/src/cluster/limit_admission.rs`):
   each start a pass places is re-checked on its nodes against running jobs,
   earlier starts in the same pass, and room held by in-flight evictions, then
   charged to the pass's `QuotaLedger` under its job QOS, account, and the
   partition QOS it starts in. A waiting job is charged nothing.

## Fair-share

The algorithm is `compute_fairshare` in `accounting/fairshare.rs`. Non-obvious
points:

- Billable usage is CPU and GPU seconds combined through `BillingWeights`
  (`billing_weights.bill`), decayed by half-life; usage older than
  `4 * halflife_days` is not queried.
- Accounts with a missing parent or in a parent cycle are re-rooted (flat
  shares) rather than dropped.
- `FAIRSHARE_CAP` in `compute_fairshare` applies to every factor, not only
  zero-usage users. The cap is duplicated in other crates; see
  [scheduling.md](../scheduler/scheduling.md#pass-structure-and-ordering).

## Reconciliation

`reconcile.rs` runs every `RECONCILE_INTERVAL_SECS` and re-issues writes for
jobs whose accounting rows are missing or stale.

- **Leader-only via `raft.ensure_leader()`** (consensus-backed, not the cached
  `is_leader()`), because these writes go straight to Postgres and bypass Raft:
  a partitioned former leader must not resync against a stale job store. The txn
  purge loop uses the same check. Leadership is only checked at the start of a
  pass.
- **`record_job_start` and `record_job_end` run in one transaction.**
  `record_job_start` unconditionally sets `state='RUNNING'` and wipes
  `end_time`, so committing it alone could clobber a correct terminal record if
  the following `record_job_end` failed.
- **A failed read is not a missing row.** Jobs whose accounting read failed are
  skipped that cycle, not resynced.
- **Accounting models only `RUNNING` and finalized states.** Non-finalized
  states such as `SUSPENDED` and `COMPLETING` compare as `RUNNING`; otherwise
  they would be flagged stale forever.

## Audit rows

Accounting-entity mutations (account, user, QOS) write their `txn` row on the
caller's transaction, so the audit row commits or rolls back with the change.
Coverage, retention, and the best-effort path for Raft-backed entities are in
`docs/admin-guide/accounting.rst` ("Auditing administrative actions").
