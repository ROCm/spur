# Accounting Subsystem

Spur's accounting subsystem tracks job history, resource usage, fair-share
factors, and enforces per-user/per-account/per-QOS resource limits. It is
backed by PostgreSQL, accessed via `sqlx`, and designed so a database outage
does not block controller startup. Jobs with QOS/account constraints are held
while admission caches are cold; once loaded, subsequent outages are fail-open.

## PostgreSQL Schema

All tables are created and migrated inline via `spurctld/src/accounting/db.rs`
(`SCHEMA` constant + idempotent `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`
statements). Migrations are serialized across controllers by a PostgreSQL
advisory lock (`pg_advisory_xact_lock`), so concurrent controllers sharing one
database never overlap.

### Core Tables

**`jobs`** -- one row per job. Primary key is `job_id` (BIGINT). Columns track
submission, start, and end times, user/account/partition/QOS, exit codes
(including `exit_signal`, `derived_exit_code`), node count, task count, memory,
GPU count (`total_gpus`), and preemption metadata (`preempted_by`,
`preempt_mode`, `preempt_qos`). The `reason` and `ended_by` columns record why
and who terminated the job. `idle_fill` marks jobs that ran on spare capacity.
`usage_recorded` gates double-counting in the usage aggregation.

**`accounts`** -- hierarchical account tree. `parent_account` references another
account by name (nullable for root accounts). Each account carries a
`fairshare_weight` (default 1) and optional resource limits: `max_running_jobs`,
`grp_tres` (group TRES cap as a `cpu=N,node=N,...` string). `allowed_qos` holds
the account's QOS edit and `default_qos` its default (see
[QOS lists and defaults](#qos-lists-and-defaults)).

**`users`** -- composite PK `(name, account)`. References `accounts(name)`.
Carries `admin_level` (None/Operator/Administrator), `default_account` (at most
one per user, enforced by a partial unique index), and a `created_at` timestamp.

**`associations`** -- per-user-per-account-per-partition limits. Unique on
`(user_name, account, partition_name)`. Columns: `fairshare_weight`,
`max_running_jobs`, `max_submit_jobs`, `grp_submit_jobs`, `max_tres_per_job`,
`grp_tres`, `max_wall_min`, `default_qos`, `allowed_qos` (the user's QOS edit,
same encoding as on `accounts`).

**`qos`** -- Quality of Service definitions. Primary key is `name`. Columns:
`priority`, `preempt_mode` (off/cancel/requeue/suspend), `preempt` (list of QOS
names this one can preempt), `usage_factor` (REAL, default 1.0),
`max_jobs_per_user`, `max_submit_per_user`, `max_submit_per_account`,
`grp_submit_jobs`, `max_tres_per_job`, `max_tres_per_user`, `grp_tres`,
`max_wall_min`, `grp_wall_min`, `preempt_exempt_time`, `flags` (e.g.
`DenyOnLimit`), `idle_fill_preemptable`.

**`usage`** -- aggregated resource consumption per `(user_name, account,
period_start)`. Stores `cpu_seconds`, `gpu_seconds`, `job_count`. Used by the
fair-share computation.

**`tres_usage`** -- per-job, per-TRES-type allocated and used values.

**`txn`** -- audit log of mutating actions (see Audit Transactions below).

### Indexes

The schema includes indexes on `jobs(user_name)`, `jobs(account)`,
`jobs(state)`, `jobs(submit_time)`, `jobs(start_time)`, and a composite
covering index `jobs(end_time, qos, start_time)` for the GrpWall window
aggregate. `usage(period_start, period_end)`, `associations(user_name)`,
`associations(account)`, `txn(ts)`, `txn(actor)`, `txn(entity_type,
entity_name)`.

## TRES (Trackable RESource)

Defined in `spur-core/src/accounting.rs`. Six types: `Cpu`, `Memory` (MB),
`Energy` (Joules), `Node`, `Gpu` (`gres/gpu`), `Billing` (weighted composite).

A `TresRecord` is a `HashMap<TresType, u64>` with parse/format support for the
`cpu=N,mem=N,gres/gpu=N` string format Slurm uses. The parser rejects unknown
types and unit-suffixed values (`mem=1G`) with an error rather than silently
dropping them.

## INFINITE Sentinel

`u32::MAX` on the wire means "no limit" (stored as SQL `NULL`). A literal `0`
means "block all". A stray negative in the DB (from pre-normalization data) is
treated as unset. The three-way mapping:

- `NULL` / negative -> `None` (no limit)
- `0` -> `Some(0)` (block all)
- positive N -> `Some(N)` (limit to N)

This is implemented by `limit_from_db()` (DB -> in-memory) and `limit_to_wire()`
(DB -> proto).

## Accounts and Associations

**Account hierarchy.** Accounts form a tree via `parent_account`. Root accounts
have `parent_account = NULL`. Fair-share is split proportionally by sibling
weight at each level (BFS from roots).

**User-account membership.** A `(user, account)` pair in the `users` table
constitutes a membership. A user can belong to multiple accounts.

**Default account.** Each user has at most one default account (partial unique
index on `users(name) WHERE default_account IS NOT NULL`).

**Admin level.** Stored per user-account row but merged to the highest level
across all memberships (Administrator > Operator > None). Three synonyms
resolve to `Administrator`: `Admin`, `Administrator`, `SuperUser`.

**Association limits** (`AccountLimits`): `max_running_jobs`,
`max_submit_jobs`, `grp_submit_jobs`, `max_wall_minutes` (all `Option<u32>`),
`max_tres_per_job`, `grp_tres` (both `Option<TresRecord>`). None = no limit.

### QOS lists and defaults

Implemented in `spurctld/src/accounting/qos_level.rs`. An `allowed_qos` cell
stores an edit, not the evaluated list: NULL inherits, `a,b` replaces
(`QosLevel::Exact`), and `+a,-b` adds and removes on top of the parent
(`QosLevel::Adjust`). `evaluate_accounts` folds the edits from the root down
the `parent_account` chain, and `effective_user` applies the user's edit last.
The top of the tree is "every QOS" (`EffectiveQos::unrestricted`), so a removal
with no list above starts from every existing QOS name (`list_qos_names`) and
an add there leaves access open. An exact list left empty by `-` or by QOS
deletion inherits again. `default_qos` is the user's own (an empty string
counts as unset), else the nearest account's. A stored cell that does not
parse reads as the exact names stored (`parse_level`); `strip_name` leaves it
untouched and `apply_write` refuses a `qos+=`/`qos-=` on it. A parent missing from `accounts` ends the walk, treating the
account as top of the tree.

Every write that changes QOS grants takes one transaction-scoped advisory lock,
`lock_qos_grants` (`SCHEMA_LOCK_CLASS`, `QOS_GRANTS_LOCK_OBJ` = 2): account and
user writes that restate `qos`, `defaultqos` or `parent`, `delete qos`, and
`delete account`. Under it, `stage_qos_write` (`grpc.rs`) checks every named
QOS exists and applies a `qos+=`/`qos-=` to the stored edit. After the write,
`check_qos_grants` re-evaluates the tree in the same transaction and refuses:

- a parent that does not exist or is a sub-account of the account
  (`parent_problem`). `root` with no such account means top of the tree
  (`IMPLICIT_ROOT`);
- a `qos-=` that emptied an exact list when nothing above sets one
  (`emptied_exact_list`), since inheriting would allow every QOS;
- a default off the evaluated list for the account or a user directly in it
  (`defaults_without_access`). Like Slurm, sub-accounts are not re-checked.

`delete qos` runs `strip_qos_references`, which drops the name from every edit,
clears defaults naming it, and returns each exact list it emptied
(`EmptiedList`); the delete is refused if any of them is then unlimited.
`delete account` is refused while `child_accounts` is non-empty.

At submit, `apply_default_qos` (`cluster.rs`) resolves a job with no `--qos` to
the first of: the default when permitted, the only existing QOS on the list,
then `accounting.default_qos` when permitted. With a list and none of these, the
job is rejected; with no list, only `require_qos` rejects it.

## QOS Enforcement

QOS limits are checked at three points:

1. **Submission gate** (`check_qos_submit_limits`): submit-count limits
   (`MaxSubmitJobsPerUser`, `MaxSubmitJobsPerAccount`, `GrpSubmitJobs`) always
   deny at submission, independent of `DenyOnLimit`. Resource/wall limits deny
   at submission only when the QOS has `DenyOnLimit` (`check_qos_standalone_limits`).

2. **Scheduler gate** (`check_qos_limits`): all limits checked with real
   aggregated load. Returns `Allowed` or `Blocked(PendingReason)`.

3. **Start check** (`LimitAdmission` in `cluster/limit_admission.rs`): each
   start the backfill pass places is re-checked on its nodes against the
   running jobs, the starts before it in the pass and the room held by
   evictions in flight, then charged to the pass's `QuotaLedger` under its job
   QOS, its account and the partition QOS of the partition it starts in. A
   waiting job is charged nothing; a refused start pends with the limit's reason.

Limit dimensions checked for TRES caps: CPU, Node, Memory, GPU. A dimension
capped at 0 is treated as uncapped. Comparisons are strict `>` (at the cap =
allowed). `GrpWall` uses `>=` (at the budget = blocked).

**Idle-fill eligibility.** `grp_node_is_sole_blocker()` re-runs the full check
with the group node cap zeroed to determine if a job qualifies for idle-fill
(over its group node quota and nothing else).

## Fair-Share

Implemented in `spurctld/src/accounting/fairshare.rs`. The algorithm:

1. **Build effective shares** -- BFS from root accounts, splitting parent share
   proportionally by sibling `fairshare_weight`. Accounts with a parent cycle
   or missing parent are treated as roots (degraded to flat shares rather than
   dropped).

2. **Compute per-user shares** -- within each account, the account's effective
   share is split by each user's `fairshare_weight` from the association.

3. **Decay-weighted usage** -- billable usage = `(cpu_seconds + gpu_seconds) *
   exp(-decay_rate * age)`. Decay rate = `ln(2) / halflife_seconds`. Usage
   older than `4 * halflife_days` is not queried.

4. **Factor** -- `user_share / actual_share_of_total_usage`. Zero-usage users
   get `user_share / epsilon` (capped at 10.0). The cap matches the scheduler's
   priority calculation.

The fair-share cache is refreshed periodically (`fairshare_refresh_secs`) from
the database in a background loop.

## Best-Effort Accounting (DB Down Doesn't Block Scheduling)

The accounting subsystem is designed so database unavailability does not block
controller startup. Scheduling is mostly unaffected, with one exception: jobs
that name a QOS or account are held (`AccountingUnavailable`) while their
admission caches are cold (`accounting_block`). Once caches have loaded at
least once, subsequent DB outages do not re-block scheduling:

- **Startup.** `accounting::start()` spawns a background task that connects with
  capped exponential backoff (1s to 30s). The gRPC service reports
  `UNAVAILABLE` with the current failure cause until connected.

- **Job notifications.** `AccountingNotifier` writes job start/end records with
  retries. If retries exhaust, the write is deferred to the reconcile loop.

- **GrpWall.** When consumption is unknown (accounting unreachable), the
  `grp_wall_minutes` cap is simply not applied -- jobs are admitted rather than
  the cluster halting.

## Reconciliation

`reconcile.rs` runs every 120 seconds on the Raft leader. It diffs in-memory
job state against the accounting database and re-issues writes for jobs whose
records are missing or stale:

1. Query known accounting states for all started jobs.
2. Compare expected state (from in-memory job store) vs actual DB state.
3. Re-issue `record_job_start` and/or `record_job_end` in a single transaction
   for each stale job.

The transaction wrapping prevents the pre-fix corruption where `record_job_start`
(which unconditionally sets `state='RUNNING'` and wipes `end_time`) could commit
independently, clobbering a correct terminal record if the subsequent
`record_job_end` failed.

Jobs with unknown read status (DB query failed) are skipped that cycle, not
treated as missing -- confirmed-absent vs failed-read are distinct.

Accounting maps only two states: `RUNNING` and finalized. Non-finalized states
like `SUSPENDED` and `COMPLETING` map to `RUNNING` for comparison, preventing
perpetual stale-flagging.

**Transaction purge loop.** Runs hourly on the leader, deleting `txn` rows older
than `txn_retention_days`. Both loops use `raft.ensure_leader()` (consensus-
backed, not cached) to prevent a partitioned former leader from writing against a
stale job store.

## Audit Transactions (txn table)

Every mutating `sacctmgr` operation (add/modify/remove account, user, QOS) writes
a `txn` row recording: timestamp, actor, actor UID, verified status, source
(api/system), action, entity type/name, outcome (success/failure), details (JSON),
and peer address. Written in-band on the caller's transaction so the audit row
commits or rolls back atomically with the mutation.

Viewable via `sacctmgr show txn`. Purged by `txn_retention_days` (configurable).

## Cache Architecture

Four in-memory caches are refreshed from the database on a periodic loop:

- **FairshareCache** -- `HashMap<(user, account), f64>` factor.
- **QosCache** -- `HashMap<name, Qos>` definitions.
- **AssociationCache** -- default QOS, default account, memberships, per-
  association limits, allowed QOS sets, admin levels.
- **GrpWallCache** -- per-QOS consumed wall-clock minutes over the configured
  trailing window.

All four use the same `refresh_secs` interval (from `[accounting]` config).
The caches are populated by the background bringup task after a successful
database connection, so a controller that boots without a database retries until
connected rather than running with empty caches.

## Key Source Files

| File | Purpose |
|------|---------|
| `spurctld/src/accounting/mod.rs` | Startup, activation, fairshare entry point |
| `spurctld/src/accounting/db.rs` | Schema DDL, all SQL queries |
| `spurctld/src/accounting/fairshare.rs` | Fair-share factor computation |
| `spurctld/src/accounting/reconcile.rs` | Periodic resync of stale DB records |
| `spurctld/src/accounting/grpc.rs` | gRPC service (sacctmgr RPCs) |
| `spurctld/src/accounting/qos_level.rs` | QOS edit encoding, inheritance, default access check |
| `spurctld/src/accounting/notifier.rs` | Job start/end notification writer |
| `spurctld/src/accounting/txn.rs` | Audit transaction types |
| `spur-core/src/accounting.rs` | Data models: Account, Qos, TresRecord, limits |
| `spur-core/src/qos.rs` | QOS enforcement logic |
