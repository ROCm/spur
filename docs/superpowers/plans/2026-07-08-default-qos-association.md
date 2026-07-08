# Plan: default QOS per user/account association

Tracks: [ROCm/spur#408](https://github.com/ROCm/spur/issues/408)
Related, not superseded: [ROCm/spur#282](https://github.com/ROCm/spur/issues/282) (explicit `--qos` enforcement — largely fixed already; this plan closes one of its remaining criteria as a side effect, see below).

## Problem

Slurm lets an admin set a default QOS on a user-account association
(`sacctmgr modify user <u> account=<a> set DefaultQOS=<q>`), so a job
submitted with no `--qos` inherits `<q>` and is subject to its limits.
Verified live against Slurm 25.11.6 (with `slurmdbd`+MariaDB): an
association with `DefaultQOS=highprio` produced jobs with `QOS=highprio`
with no `--qos` flag; an association with no default fell back to the
cluster default (`normal`).

Spur has no equivalent at any layer today:

- `associations`/`accounts`/`users` tables (`crates/spurctld/src/accounting/db.rs`)
  have no `default_qos` column.
- `AddUserRequest`/`UserInfo`/`CreateAccountRequest`/`AccountInfo`
  (`proto/slurm.proto`) carry no `default_qos` field.
- `spur sacctmgr modify user ... set DefaultQOS=...` is a no-op stub
  (`crates/spur-cli/src/sacctmgr.rs::modify`, `"user"` arm just prints a
  message).
- `ClusterManager::resolve_qos()` (`crates/spurctld/src/cluster.rs`) only
  ever reads `job.spec.qos`; a job with no `--qos` bypasses QOS checks
  entirely (`qos_block_for` early-returns via `job.spec.qos.as_ref()?`).

Confirmed live on the 2-node Spur cluster: a job submitted with
`-A qostest` and no `--qos` shows `QOS=` (blank), and there is no
`defaultqos=` parameter accepted anywhere in the CLI.

## Design decision: resolve at submit time, not at use time

The cleanest fix is to resolve the effective QOS **once, at submission**,
and write it into `spec.qos` before the job is persisted — not to thread a
"maybe defaulted" QOS through every place that reads `job.spec.qos`
(`resolve_qos`, `qos_block_for`, `pending_jobs`, display in
`scontrol`/`squeue`, accounting records, etc). This exactly mirrors the
existing `apply_default_partition()` pattern in `submit_job()`
(`cluster.rs:164`, `cluster.rs:3900`), which resolves an unset partition to
the cluster's default partition before validation, and it means:

- `resolve_qos()` and `qos_block_for()` need **zero code changes** — they
  already do the right thing once `spec.qos` is populated.
- `scontrol show job` / `squeue` / accounting records show the real,
  resolved QOS name (matches the live Slurm behavior observed: the job's
  QOS field itself is `highprio`, not merely "effectively highprio").
- Replay-deterministic for free: the resolved name is baked into the
  `JobSpec` carried by `WalOperation::JobSubmit`, so followers replay the
  already-resolved value — no clock, no cache-state dependency on apply.

Precedence chain (highest first), matching Slurm's model minus the parts
explicitly out of scope (see below):

1. `--qos` explicit on submission. If non-empty and does **not** name a
   QOS that exists, **reject the submission** with a clear error. This is
   a natural side effect of touching submit-time QOS resolution and closes
   the one remaining unmet acceptance criterion on #282 ("A job requesting
   a non-existent QOS is rejected with a clear error"). Today it silently
   falls back to `Qos::default()` (unlimited).
2. The **association's** `default_qos` for `(user, effective_account)`,
   where `effective_account = spec.account` if set, else the user's
   `default_account` (already stored in the `users` table today, just
   never consulted at submit time). If the resolved default QOS name no
   longer exists (e.g. deleted after being set as default), log a warning
   and fall through — do not fail the submission.
3. Nothing set → `spec.qos` stays `None` (today's bypass behavior,
   unchanged).

### Explicitly out of scope for this plan (v1)

- **Account-level `DefaultQOS`** (`sacctmgr modify account ... set
  DefaultQOS=`) and cascading/recursive semantics to existing child
  associations. Real Slurm's account-level default interacts with
  parent/child account hieraromanchy in ways that deserve their own design
  pass. Note it as explicit follow-up in the issue when this ships.
- **Cluster-wide default QOS** (Slurm's `sacctmgr modify cluster set
  DefaultQOS=`). Spur has no "cluster" entity/table; if wanted, the
  simplest fit is a `[accounting] default_qos` config key, but it's not
  needed to satisfy the reported gap (association-level covers the tested
  scenario) and is left as a possible phase 2.
- Persisting the *resolved default account* onto `spec.account` when `-A`
  is omitted. That's a distinct, pre-existing gap (Spur never defaults the
  account at all today) — this plan only reads `users.default_account` for
  the QOS lookup key, it does not change what `spec.account` itself
  contains after submission. Worth its own issue if wanted.

## Implementation steps

### 1. Schema (`crates/spurctld/src/accounting/db.rs`)

- `ALTER TABLE associations ADD COLUMN IF NOT EXISTS default_qos TEXT;`
  (no FK — a stale reference must degrade gracefully per the precedence
  chain above, not be enforced at the DB layer).
- Extend `add_user()` to accept and persist `default_qos: Option<&str>` in
  both the `INSERT ... ON CONFLICT DO UPDATE` for `associations` (the
  `users` table doesn't need it — only `associations` carries QOS-related
  fields today).
- Extend `list_users()`/`UserRecord`... actually `default_qos` belongs on
  the **association**, not the user-account row identity — add a
  dedicated `pub async fn list_associations(pool) -> Vec<AssociationRecord>`
  (fields: `user_name`, `account`, `default_qos: Option<String>`) for the
  new cache to consume, mirroring `list_users`'s shape. Reuse the existing
  `users.default_account` column (already there) for step 2 of the
  precedence chain — add a `default_account_by_user()` query or fold it
  into the same fetch pass.

### 2. Proto (`proto/slurm.proto`)

- `AddUserRequest`: add `string default_qos = 5;`.
- `UserInfo`: add `string default_qos = 5;` (for `sacctmgr show user`
  parity — Slurm's `show assoc` displays `Def QOS`).
- No new RPC needed: `add_user` is already an upsert
  (`ON CONFLICT ... DO UPDATE`), so `modify user` can reuse it exactly like
  `modify account`/`modify qos` already do (see step 4).

### 3. Controller: validate + resolve (`crates/spurctld/src/cluster.rs`)

- New free function `apply_default_qos(spec: &mut JobSpec, assoc_cache: &AssociationCache, qos_cache: &QosCache) -> anyhow::Result<()>`,
  mirroring `apply_default_partition`'s shape:
  - If `spec.qos` is `Some(name)`: verify `qos_cache.get(name)` exists;
    `bail!` with a clear message if not (closes the #282 criterion).
  - If `spec.qos` is `None`: compute `effective_account`, look up
    `assoc_cache.default_qos(user, effective_account)`; if found and it
    still exists in `qos_cache`, set `spec.qos = Some(name)`. If found but
    stale (deleted since), `warn!` and leave `spec.qos = None`.
- Call it in `submit_job()` right after `apply_default_partition` and
  before `validate_partition` (`cluster.rs:164-165`).
- `resolve_qos()` and `qos_block_for()`: **no changes**.

### 4. New cache: `AssociationCache` (`crates/spurctld/src/association_cache.rs`)

Mirror `FairshareCache`'s exact shape (`fairshare_cache.rs`): `RwLock<HashMap<...>>` + `spawn_refresh_loop(pool, interval)`, holding two maps built from one fetch pass:

```rust
pub struct AssociationCache {
    default_qos: RwLock<HashMap<(String, String), String>>, // (user, account) -> qos name
    default_account: RwLock<HashMap<String, String>>,        // user -> default account
}
```

- `get_default_qos(&self, user: &str, account: &str) -> Option<String>`
- `get_default_account(&self, user: &str) -> Option<String>`
- Wire into `ClusterManager` alongside `qos_cache`/`fairshare_cache`
  (`cluster.rs:122-152`) and spawn its refresh loop in `main.rs` next to
  the other two (`main.rs:168-177`), same `fairshare_refresh_secs`
  interval — no new config knob.

### 5. CLI (`crates/spur-cli/src/sacctmgr.rs`)

- `add "user"`: parse `defaultqos=` param, pass through
  `AddUserRequest.default_qos`.
- `modify "user"`: replace the no-op stub with the same
  `client.add_user(AddUserRequest { .. })` upsert call `modify "account"`
  and `modify "qos"` already use — parse `defaultqos=` (and pass through
  existing fields unchanged, matching Slurm's "modify re-sends the whole
  record" semantics already used by the other two arms).
- `show "user"` / `show "assoc"`: display a `Def QOS` column from the new
  `UserInfo.default_qos` field (`show_users`/equivalent formatting
  function).

### 6. Server (`crates/spurctld/src/accounting/grpc.rs`)

- `add_user` handler: thread `req.default_qos` through to
  `db::add_user(...)`.
- `list_users` handler: include `default_qos` in the returned `UserInfo`.

## Test coverage

### Unit tests

- `crates/spurctld/src/accounting/db.rs`: `add_user` persists and
  round-trips `default_qos` on the association row; upsert (add twice with
  different `default_qos`) updates rather than duplicating.
- `crates/spurctld/src/association_cache.rs` (new): cache
  populated from a fetch returns the right `(user, account) -> qos` and
  `user -> default_account` mappings; missing lookups return `None`
  (mirror `FairshareCache`'s existing test style).
- `crates/spurctld/src/cluster.rs` — `apply_default_qos`:
  - explicit valid `--qos` passes through unchanged.
  - explicit **invalid** `--qos` (name not in `qos_cache`) → `Err` with a
    message naming the bad QOS (closes the #282 criterion).
  - no `--qos`, association has a `default_qos`, account given via `-A` →
    `spec.qos` becomes the association's default.
  - no `--qos`, no `-A`, user has `default_account` with an association
    default set → still resolves (covers the "no -A, no --qos" case).
  - no `--qos`, association has no `default_qos` → `spec.qos` stays
    `None` (bypass unchanged — regression guard for existing behavior).
  - no `--qos`, association's `default_qos` names a QOS that was since
    deleted from `qos_cache` → `spec.qos` stays `None`, a warning is
    logged, submission still succeeds (no hard failure on stale data).
- `crates/spur-cli/src/sacctmgr.rs`: `modify user ... set
  DefaultQOS=...` builds the same `AddUserRequest` shape as `add user
  ... DefaultQOS=...` (parser-level test, no live RPC needed — mirror
  existing `parse_params` unit tests in that file if present, else add
  focused tests around the param-parsing helpers touched).

### Integration-style tests (existing `cluster.rs` test harness, in-process controller + `PgPool` test fixture — reuse the pattern in `crates/spurctld/src/accounting/db.rs`'s existing `#[tokio::test] #[ignore = "requires DATABASE_URL and PostgreSQL"]` tests, e.g. `list_qos_round_trips_all_limits`)

- End-to-end: `add_user` with `default_qos` → `submit_job` with no
  `--qos` → resulting job's `spec.qos` matches, and `qos_block_for`
  actually enforces that QOS's limits (e.g. set `max_jobs_per_user=1` on
  the default QOS, submit two jobs, confirm the second is blocked with
  `PendingReason::QoSMaxJobsPerUser` — proves the fix reaches enforcement,
  not just display).

### Live verification (required before merge, per project convention — do not claim parity without it)

Repeat exactly what was already verified manually against Slurm on the lab
testbed, this time against the Spur build from this branch:

1. Deploy branch build to the 2-node Spur cluster.
2. `spur sacctmgr add qos highprio ...`, `spur sacctmgr add account
   testacct`, `spur sacctmgr add user vm account=testacct
   defaultqos=highprio` (or `modify`, once implemented).
3. `sbatch -A testacct <script>` with **no `--qos`** → `scontrol show job`
   must show `QOS=highprio`.
4. A second association with no `default_qos` set → job's `QOS=` empty
   (bypass), confirming no regression to the current no-default path.
5. Explicit `--qos=doesnotexist` → submission rejected with a clear error
   (closes the #282 criterion) — confirm the CLI surfaces the controller's
   error message cleanly, not a raw gRPC status dump.
6. Confirm no invalid-transition/panic noise in the controller log across
   the above (standard regression check from prior live-test sessions on
   this codebase).
7. Tear down: revert any testbed config changes, remove test
   accounts/users/QOS created for the run, matching the teardown discipline
   used in every prior live-test round on this cluster.

## Open questions to resolve before/while implementing

- Precedence between an **invalid explicit `--qos`** (reject) and a
  **stale association default** (silently degrade) is intentionally
  asymmetric — reject what the user typed, don't reject what an admin
  configured and later broke by deleting a QOS. Confirm this matches
  reviewer expectations before landing; it's the one non-obvious design
  call in this plan.
- Whether `show assoc`'s column layout should exactly mirror Slurm's
  `sacctmgr show assoc format=...` spacing, or just add a `Def QOS`
  column to Spur's existing simpler table — default to the latter unless
  parity fidelity is specifically requested (Spur's `sacctmgr show`
  output is not yet byte-for-byte Slurm-compatible today either).
