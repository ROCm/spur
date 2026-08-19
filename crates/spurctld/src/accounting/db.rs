// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{PgPool, QueryBuilder, Row};

use spur_core::job::JobId;

/// Apply the database schema, serialized across controllers by a fixed advisory
/// lock: migrate now also rewrites data (default-account dedup) and builds a
/// unique index, so concurrent runs against a shared database must not overlap.
/// The lock is transaction-scoped, so it releases even if the apply fails.
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(SCHEMA_LOCK_CLASS)
        .bind(SCHEMA_LOCK_OBJ)
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(SCHEMA).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// Advisory-lock keys that serialize `migrate` across controllers. The two-int4
// form is a distinct lock space from the single-bigint per-user locks
// `add_user` takes, so the keys can never collide.
const SCHEMA_LOCK_CLASS: i32 = 0x5350_5552; // 'SPUR'
const SCHEMA_LOCK_OBJ: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    job_id          INTEGER PRIMARY KEY,
    name            TEXT NOT NULL DEFAULT '',
    user_name       TEXT NOT NULL,
    uid             INTEGER NOT NULL DEFAULT 0,
    account         TEXT NOT NULL DEFAULT '',
    partition_name  TEXT NOT NULL DEFAULT '',
    qos             TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL DEFAULT 'PENDING',
    exit_code       INTEGER NOT NULL DEFAULT 0,
    exit_signal     INTEGER NOT NULL DEFAULT 0,
    derived_exit_code INTEGER NOT NULL DEFAULT 0,
    num_nodes       INTEGER NOT NULL DEFAULT 1,
    num_tasks       INTEGER NOT NULL DEFAULT 1,
    cpus_per_task   INTEGER NOT NULL DEFAULT 1,
    memory_mb       BIGINT NOT NULL DEFAULT 0,
    nodelist        TEXT NOT NULL DEFAULT '',
    submit_time     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    start_time      TIMESTAMPTZ,
    end_time        TIMESTAMPTZ,
    time_limit_min  INTEGER,
    work_dir        TEXT NOT NULL DEFAULT '',
    script_hash     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS accounts (
    name            TEXT PRIMARY KEY,
    description     TEXT NOT NULL DEFAULT '',
    organization    TEXT NOT NULL DEFAULT '',
    parent_account  TEXT,
    fairshare_weight INTEGER NOT NULL DEFAULT 1,
    max_running_jobs INTEGER,
    grp_tres        TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS users (
    name            TEXT NOT NULL,
    account         TEXT NOT NULL REFERENCES accounts(name),
    admin_level     TEXT NOT NULL DEFAULT 'none',
    default_account TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (name, account)
);

CREATE TABLE IF NOT EXISTS usage (
    user_name       TEXT NOT NULL,
    account         TEXT NOT NULL,
    period_start    TIMESTAMPTZ NOT NULL,
    period_end      TIMESTAMPTZ NOT NULL,
    cpu_seconds     BIGINT NOT NULL DEFAULT 0,
    gpu_seconds     BIGINT NOT NULL DEFAULT 0,
    job_count       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_name, account, period_start)
);

CREATE TABLE IF NOT EXISTS qos (
    name            TEXT PRIMARY KEY,
    description     TEXT NOT NULL DEFAULT '',
    priority        INTEGER NOT NULL DEFAULT 0,
    preempt_mode    TEXT NOT NULL DEFAULT 'off',
    preempt         TEXT NOT NULL DEFAULT '',
    usage_factor    REAL NOT NULL DEFAULT 1.0,
    max_jobs_per_user INTEGER,
    max_submit_per_user INTEGER,
    max_submit_per_account INTEGER,
    grp_submit_jobs INTEGER,
    max_tres_per_job TEXT,
    max_tres_per_user TEXT,
    grp_tres        TEXT,
    max_wall_min    INTEGER,
    grp_wall_min    INTEGER,
    preempt_exempt_time INTEGER,
    flags           TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS associations (
    id              SERIAL PRIMARY KEY,
    user_name       TEXT NOT NULL,
    account         TEXT NOT NULL REFERENCES accounts(name),
    partition_name  TEXT,
    fairshare_weight INTEGER NOT NULL DEFAULT 1,
    max_running_jobs INTEGER,
    max_submit_jobs INTEGER,
    grp_submit_jobs INTEGER,
    max_tres_per_job TEXT,
    grp_tres        TEXT,
    max_wall_min    INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_name, account, partition_name)
);

CREATE TABLE IF NOT EXISTS tres_usage (
    job_id          INTEGER NOT NULL,
    tres_type       TEXT NOT NULL,
    alloc_value     BIGINT NOT NULL DEFAULT 0,
    used_value      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (job_id, tres_type)
);

CREATE INDEX IF NOT EXISTS idx_jobs_user ON jobs(user_name);
CREATE INDEX IF NOT EXISTS idx_jobs_account ON jobs(account);
CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
CREATE INDEX IF NOT EXISTS idx_jobs_submit_time ON jobs(submit_time);
CREATE INDEX IF NOT EXISTS idx_jobs_start_time ON jobs(start_time);
CREATE INDEX IF NOT EXISTS idx_usage_period ON usage(period_start, period_end);
CREATE INDEX IF NOT EXISTS idx_assoc_user ON associations(user_name);
CREATE INDEX IF NOT EXISTS idx_assoc_account ON associations(account);

ALTER TABLE jobs ADD COLUMN IF NOT EXISTS exit_signal INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS derived_exit_code INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS reservation TEXT NOT NULL DEFAULT '';
-- No FK to qos(name): a stale reference (QOS deleted after being set as a
-- default) must degrade gracefully at read time, not be blocked here.
ALTER TABLE associations ADD COLUMN IF NOT EXISTS default_qos TEXT;
-- Comma-separated QOS names, same degrade-gracefully rationale as default_qos.
ALTER TABLE associations ADD COLUMN IF NOT EXISTS allowed_qos TEXT;
ALTER TABLE qos ADD COLUMN IF NOT EXISTS grp_wall_min INTEGER;
ALTER TABLE qos ADD COLUMN IF NOT EXISTS max_submit_per_account INTEGER;
ALTER TABLE qos ADD COLUMN IF NOT EXISTS grp_submit_jobs INTEGER;
ALTER TABLE qos ADD COLUMN IF NOT EXISTS flags TEXT NOT NULL DEFAULT '';
ALTER TABLE associations ADD COLUMN IF NOT EXISTS grp_submit_jobs INTEGER;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS grp_tres TEXT;
ALTER TABLE qos ADD COLUMN IF NOT EXISTS preempt TEXT NOT NULL DEFAULT '';
ALTER TABLE qos ADD COLUMN IF NOT EXISTS preempt_exempt_time INTEGER;

-- users.default_account is the single source of truth for a user's default
-- account (the scheduler reads it via the association cache). associations
-- once carried a redundant is_default flag that nothing read; drop it so the
-- two representations can't drift.
ALTER TABLE associations DROP COLUMN IF EXISTS is_default;

-- One default account per user. Pre-fix rows could mark several accounts
-- default; collapse each user to one (the lowest account name) before the index
-- below, which would otherwise fail to build. Idempotent: a no-op once clean.
UPDATE users u SET default_account = NULL
WHERE default_account IS NOT NULL
  AND EXISTS (
    SELECT 1 FROM users o
    WHERE o.name = u.name
      AND o.default_account IS NOT NULL
      AND o.account < u.account
  );
CREATE UNIQUE INDEX IF NOT EXISTS one_default_account_per_user
    ON users (name) WHERE default_account IS NOT NULL;

-- Administrative action / audit log. Records who ran reservation admin commands
-- (create/update/delete) and their outcome. Entity-agnostic so other admin ops
-- can reuse it later. `details` is a JSON string (sqlx has no json feature).
CREATE TABLE IF NOT EXISTS txn (
    id           BIGSERIAL PRIMARY KEY,
    ts           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor        TEXT NOT NULL DEFAULT '',
    actor_uid    BIGINT,
    verified     BOOLEAN NOT NULL DEFAULT FALSE,
    source       TEXT NOT NULL DEFAULT 'api',
    action       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    entity_name  TEXT NOT NULL DEFAULT '',
    outcome      TEXT NOT NULL DEFAULT 'success',
    details      TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_txn_ts ON txn(ts);
CREATE INDEX IF NOT EXISTS idx_txn_actor ON txn(actor);
CREATE INDEX IF NOT EXISTS idx_txn_entity ON txn(entity_type, entity_name);
"#;

/// What accounting persists when a job starts. Named fields rather than positional
/// arguments because several share a type, so a transposed pair would compile
/// silently and mis-attribute the job.
pub struct JobStartRecord {
    pub job_id: JobId,
    pub name: String,
    pub user: String,
    pub account: String,
    pub partition: String,
    pub qos: String,
    pub num_nodes: u32,
    pub num_tasks: u32,
    pub cpus_per_task: u32,
    pub memory_mb: u64,
    pub submit_time: DateTime<Utc>,
    pub start_time: DateTime<Utc>,
    pub reservation: Option<String>,
}

/// Record a job start in the database.
///
/// Takes a `&mut PgConnection` (not a hard `&PgPool`) so callers can either
/// acquire a standalone connection from a pool (as the notifier does) or pass
/// one borrowed from an open `Transaction` (`Transaction` derefs to
/// `PgConnection`) to run this alongside other writes atomically, as
/// reconciliation's backfill-then-finalize does.
pub async fn record_job_start(conn: &mut PgConnection, rec: &JobStartRecord) -> anyhow::Result<()> {
    // job_id reuse after a Raft wipe means a conflict is a new, unrelated job.
    sqlx::query(
        r#"
        INSERT INTO jobs (job_id, name, user_name, account, partition_name, qos, num_nodes, num_tasks, cpus_per_task, memory_mb, submit_time, start_time, state, reservation)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'RUNNING', $13)
        ON CONFLICT (job_id) DO UPDATE SET
            name = EXCLUDED.name,
            user_name = EXCLUDED.user_name,
            account = EXCLUDED.account,
            partition_name = EXCLUDED.partition_name,
            qos = EXCLUDED.qos,
            num_nodes = EXCLUDED.num_nodes,
            num_tasks = EXCLUDED.num_tasks,
            cpus_per_task = EXCLUDED.cpus_per_task,
            memory_mb = EXCLUDED.memory_mb,
            submit_time = EXCLUDED.submit_time,
            start_time = EXCLUDED.start_time,
            state = EXCLUDED.state,
            exit_code = 0,
            exit_signal = 0,
            derived_exit_code = 0,
            end_time = NULL
        "#,
    )
    .bind(rec.job_id as i32)
    .bind(&rec.name)
    .bind(&rec.user)
    .bind(&rec.account)
    .bind(&rec.partition)
    .bind(&rec.qos)
    .bind(rec.num_nodes as i32)
    .bind(rec.num_tasks as i32)
    .bind(rec.cpus_per_task as i32)
    .bind(rec.memory_mb as i64)
    .bind(rec.submit_time)
    .bind(rec.start_time)
    .bind(rec.reservation.as_deref().unwrap_or_default())
    .execute(&mut *conn)
    .await?;

    // If end_time is already set, the end notification arrived first and skipped
    // usage computation (start_time was NULL at that point). Compute it now.
    let row = sqlx::query(
        "SELECT user_name, account, start_time, num_tasks, cpus_per_task, end_time FROM jobs WHERE job_id = $1",
    )
    .bind(rec.job_id as i32)
    .fetch_one(&mut *conn)
    .await?;

    let end_time: Option<DateTime<Utc>> = row.get("end_time");
    if let Some(end_time) = end_time {
        update_usage(conn, row, end_time).await?;
    }

    Ok(())
}

/// Record a job completion in the database. See `record_job_start` for why
/// this takes a `&mut PgConnection` rather than a `&PgPool`.
#[allow(clippy::too_many_arguments)]
pub async fn record_job_end(
    conn: &mut PgConnection,
    job_id: i32,
    state: &str,
    exit_code: i32,
    end_time: DateTime<Utc>,
    exit_signal: i32,
    derived_exit_code: i32,
) -> anyhow::Result<()> {
    // RETURNING closes the record_job_start job_id-reuse race by reading in the same statement.
    let row = sqlx::query(
        r#"
        INSERT INTO jobs (job_id, user_name, state, exit_code, end_time, exit_signal, derived_exit_code)
        VALUES ($1, '', $2, $3, $4, $5, $6)
        ON CONFLICT (job_id) DO UPDATE SET
            state = $2,
            exit_code = $3,
            end_time = $4,
            exit_signal = $5,
            derived_exit_code = $6
        RETURNING user_name, account, start_time, num_tasks, cpus_per_task
        "#,
    )
    .bind(job_id)
    .bind(state)
    .bind(exit_code)
    .bind(end_time)
    .bind(exit_signal)
    .bind(derived_exit_code)
    .fetch_one(&mut *conn)
    .await?;

    update_usage(conn, row, end_time).await?;

    Ok(())
}

/// A job row's accounting state, as seen by the reconciliation pass.
pub struct AccountingRowState {
    pub state: String,
    /// True when the row is missing metadata that a proper `record_job_start`
    /// would have populated (e.g. `record_job_end` created a bare row from
    /// scratch because `record_job_start` never landed). A row in this shape
    /// needs a `record_job_start` backfill even if `state` already matches.
    pub needs_start_backfill: bool,
}

/// The accounting DB's current state for a batch of jobs, in a single query.
/// Jobs with no row in `jobs` are simply absent from the returned map. Used
/// by the reconciliation pass to detect jobs missing or stale in accounting.
pub async fn job_accounting_states(
    pool: &PgPool,
    job_ids: &[i32],
) -> anyhow::Result<HashMap<i32, AccountingRowState>> {
    if job_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows =
        sqlx::query("SELECT job_id, state, user_name, start_time FROM jobs WHERE job_id = ANY($1)")
            .bind(job_ids)
            .fetch_all(pool)
            .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let job_id: i32 = r.get("job_id");
            let user_name: String = r.get("user_name");
            let start_time: Option<DateTime<Utc>> = r.get("start_time");
            let row = AccountingRowState {
                state: r.get("state"),
                needs_start_backfill: user_name.is_empty() || start_time.is_none(),
            };
            (job_id, row)
        })
        .collect())
}

/// Update usage accounting for a completed job, from the row `record_job_end` just wrote.
async fn update_usage(
    conn: &mut PgConnection,
    row: PgRow,
    end_time: DateTime<Utc>,
) -> anyhow::Result<()> {
    let user: String = row.get("user_name");
    let account: String = row.get("account");
    let start_time: Option<DateTime<Utc>> = row.get("start_time");
    let Some(start_time) = start_time else {
        // End arrived before start; usage will be computed when start lands.
        return Ok(());
    };
    let num_tasks: i32 = row.get("num_tasks");
    let cpus_per_task: i32 = row.get("cpus_per_task");

    let duration_secs = (end_time - start_time).num_seconds().max(0);
    let cpu_seconds = duration_secs * (num_tasks as i64) * (cpus_per_task as i64);

    // Truncate to hourly period for aggregation
    let period_start = start_time
        .date_naive()
        .and_hms_opt(start_time.hour(), 0, 0)
        .unwrap()
        .and_utc();
    let period_end = period_start + chrono::Duration::hours(1);

    sqlx::query(
        r#"
        INSERT INTO usage (user_name, account, period_start, period_end, cpu_seconds, job_count)
        VALUES ($1, $2, $3, $4, $5, 1)
        ON CONFLICT (user_name, account, period_start) DO UPDATE SET
            cpu_seconds = usage.cpu_seconds + $5,
            job_count = usage.job_count + 1
        "#,
    )
    .bind(&user)
    .bind(&account)
    .bind(period_start)
    .bind(period_end)
    .bind(cpu_seconds)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Job record returned from history queries.
#[derive(Debug)]
pub struct JobRecord {
    pub job_id: i32,
    pub name: String,
    pub user_name: String,
    pub account: String,
    pub partition: String,
    pub state: String,
    pub exit_code: i32,
    pub exit_signal: i32,
    pub derived_exit_code: i32,
    pub num_nodes: i32,
    pub num_tasks: i32,
    pub nodelist: String,
    pub submit_time: DateTime<Utc>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub reservation: String,
}

/// Query job history.
pub async fn get_job_history(
    pool: &PgPool,
    user: Option<&str>,
    account: Option<&str>,
    start_after: Option<DateTime<Utc>>,
    start_before: Option<DateTime<Utc>>,
    states: &[String],
    limit: u32,
) -> anyhow::Result<Vec<JobRecord>> {
    let mut qb = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT job_id, name, user_name, account, partition_name, state, exit_code, \
         exit_signal, derived_exit_code, num_nodes, num_tasks, nodelist, \
         submit_time, start_time, end_time, reservation \
         FROM jobs WHERE 1=1",
    );

    if let Some(u) = user.filter(|u| !u.is_empty()) {
        qb.push(" AND user_name = ").push_bind(u);
    }
    if let Some(a) = account.filter(|a| !a.is_empty()) {
        qb.push(" AND account = ").push_bind(a);
    }
    if let Some(after) = start_after {
        qb.push(" AND start_time >= ").push_bind(after);
    }
    if let Some(before) = start_before {
        qb.push(" AND start_time <= ").push_bind(before);
    }
    if !states.is_empty() {
        qb.push(" AND state IN (");
        let mut sep = qb.separated(", ");
        for s in states {
            sep.push_bind(s.clone());
        }
        sep.push_unseparated(")");
    }

    qb.push(" ORDER BY submit_time DESC LIMIT ")
        .push_bind(effective_query_limit(limit));

    let rows = qb.build().fetch_all(pool).await?;

    let records = rows
        .iter()
        .map(|row| JobRecord {
            job_id: row.get("job_id"),
            name: row.get("name"),
            user_name: row.get("user_name"),
            account: row.get("account"),
            partition: row.get("partition_name"),
            state: row.get("state"),
            exit_code: row.get("exit_code"),
            exit_signal: row.get("exit_signal"),
            derived_exit_code: row.get("derived_exit_code"),
            num_nodes: row.get("num_nodes"),
            num_tasks: row.get("num_tasks"),
            nodelist: row.get("nodelist"),
            submit_time: row.get("submit_time"),
            start_time: row.get("start_time"),
            end_time: row.get("end_time"),
            reservation: row.get("reservation"),
        })
        .collect();

    Ok(records)
}

/// A row from the `txn` audit log.
pub struct TxnRow {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub actor: String,
    pub actor_uid: Option<i64>,
    pub verified: bool,
    pub source: String,
    pub action: String,
    pub entity_type: String,
    pub entity_name: String,
    pub outcome: String,
    pub details: String,
}

/// Optional filters for `get_transactions`. Empty string filters are ignored.
#[derive(Default)]
pub struct TxnFilter<'a> {
    pub actor: Option<&'a str>,
    pub entity_type: Option<&'a str>,
    pub entity_name: Option<&'a str>,
    pub action: Option<&'a str>,
    pub outcome: Option<&'a str>,
    pub start_after: Option<DateTime<Utc>>,
    pub start_before: Option<DateTime<Utc>>,
    pub limit: u32,
}

/// Insert one audit record. Takes `&mut PgConnection` for the same reason as
/// `record_job_start`: callers can acquire a standalone connection or borrow
/// one from an open transaction.
pub async fn record_txn(
    conn: &mut PgConnection,
    rec: &super::txn::TxnRecord,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO txn (ts, actor, actor_uid, verified, source, action, entity_type, entity_name, outcome, details)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(rec.ts)
    .bind(&rec.actor)
    .bind(rec.actor_uid)
    .bind(rec.verified)
    .bind(rec.source.as_str())
    .bind(rec.action.as_str())
    .bind(rec.entity_type.as_str())
    .bind(&rec.entity_name)
    .bind(rec.outcome.as_str())
    .bind(&rec.details)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Default and hard cap on rows a single history/audit query returns, bounding
/// DB load and response size; a larger request is clamped, not rejected.
const DEFAULT_QUERY_LIMIT: u32 = 1_000;
const MAX_QUERY_LIMIT: u32 = 10_000;

fn effective_query_limit(requested: u32) -> i64 {
    let n = if requested == 0 {
        DEFAULT_QUERY_LIMIT
    } else {
        requested.min(MAX_QUERY_LIMIT)
    };
    i64::from(n)
}

/// Query the `txn` audit log, newest first.
pub async fn get_transactions(
    pool: &PgPool,
    filter: &TxnFilter<'_>,
) -> anyhow::Result<Vec<TxnRow>> {
    let mut qb = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT id, ts, actor, actor_uid, verified, source, action, entity_type, \
         entity_name, outcome, details FROM txn WHERE 1=1",
    );
    if let Some(v) = filter.actor.filter(|s| !s.is_empty()) {
        qb.push(" AND actor = ").push_bind(v);
    }
    if let Some(v) = filter.entity_type.filter(|s| !s.is_empty()) {
        qb.push(" AND entity_type = ").push_bind(v);
    }
    if let Some(v) = filter.entity_name.filter(|s| !s.is_empty()) {
        qb.push(" AND entity_name = ").push_bind(v);
    }
    if let Some(v) = filter.action.filter(|s| !s.is_empty()) {
        qb.push(" AND action = ").push_bind(v);
    }
    if let Some(v) = filter.outcome.filter(|s| !s.is_empty()) {
        qb.push(" AND outcome = ").push_bind(v);
    }
    if let Some(after) = filter.start_after {
        qb.push(" AND ts >= ").push_bind(after);
    }
    if let Some(before) = filter.start_before {
        qb.push(" AND ts <= ").push_bind(before);
    }

    qb.push(" ORDER BY ts DESC, id DESC LIMIT ")
        .push_bind(effective_query_limit(filter.limit));

    let rows = qb.build().fetch_all(pool).await?;
    let records = rows
        .iter()
        .map(|row| TxnRow {
            id: row.get("id"),
            ts: row.get("ts"),
            actor: row.get("actor"),
            actor_uid: row.get("actor_uid"),
            verified: row.get("verified"),
            source: row.get("source"),
            action: row.get("action"),
            entity_type: row.get("entity_type"),
            entity_name: row.get("entity_name"),
            outcome: row.get("outcome"),
            details: row.get("details"),
        })
        .collect();
    Ok(records)
}

/// Delete audit rows older than `older_than`, returning the number removed.
pub async fn purge_txn(pool: &PgPool, older_than: DateTime<Utc>) -> anyhow::Result<u64> {
    let res = sqlx::query("DELETE FROM txn WHERE ts < $1")
        .bind(older_than)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Get usage data for fair-share calculation.
pub async fn get_usage(
    pool: &PgPool,
    user: Option<&str>,
    account: Option<&str>,
    since: DateTime<Utc>,
) -> anyhow::Result<Vec<UsageRecord>> {
    let rows = sqlx::query(
        r#"
        SELECT user_name, account,
               SUM(cpu_seconds)::BIGINT as total_cpu_seconds,
               SUM(gpu_seconds)::BIGINT as total_gpu_seconds,
               SUM(job_count)::BIGINT as total_jobs,
               period_start
        FROM usage
        WHERE period_start >= $1
          AND ($2::text IS NULL OR user_name = $2)
          AND ($3::text IS NULL OR account = $3)
        GROUP BY user_name, account, period_start
        ORDER BY period_start
        "#,
    )
    .bind(since)
    .bind(user)
    .bind(account)
    .fetch_all(pool)
    .await?;

    let records = rows
        .iter()
        .map(|row| UsageRecord {
            user_name: row.get("user_name"),
            account: row.get("account"),
            cpu_seconds: row.get::<i64, _>("total_cpu_seconds"),
            gpu_seconds: row.get::<i64, _>("total_gpu_seconds"),
            job_count: row.get::<i64, _>("total_jobs") as u64,
            period_start: row.get("period_start"),
        })
        .collect();

    Ok(records)
}

/// Wall-clock minutes consumed per QOS inside the trailing `window_days`, for
/// `GrpWall` enforcement. Running jobs count their elapsed time so far, and a job
/// that started before the window contributes only the part inside it.
///
/// Each row is clamped at zero inside the `SUM`, not after it: start/end times can
/// be written by callers outside the controller and can skew, and a single
/// end-before-start row would otherwise cancel real consumption in the same QOS
/// and under-report the budget.
pub async fn consumed_wall_minutes_by_qos(
    pool: &PgPool,
    window_days: u32,
) -> anyhow::Result<HashMap<String, u64>> {
    let rows = sqlx::query(
        r#"
        SELECT qos,
               SUM(GREATEST(
                 EXTRACT(EPOCH FROM (
                   LEAST(COALESCE(end_time, now()), now())
                   - GREATEST(start_time, now() - make_interval(days => $1))
                 )),
                 0
               ))::BIGINT as wall_seconds
        FROM jobs
        WHERE qos <> ''
          AND start_time IS NOT NULL
          AND (end_time IS NULL OR end_time > now() - make_interval(days => $1))
        GROUP BY qos
        "#,
    )
    .bind(window_days as i32)
    .fetch_all(pool)
    .await?;

    let consumed = rows
        .iter()
        .map(|row| {
            let seconds = row
                .get::<Option<i64>, _>("wall_seconds")
                .unwrap_or(0)
                .max(0);
            (row.get::<String, _>("qos"), seconds as u64 / 60)
        })
        .collect();

    Ok(consumed)
}

#[derive(Debug)]
pub struct UsageRecord {
    pub user_name: String,
    pub account: String,
    pub cpu_seconds: i64,
    pub gpu_seconds: i64,
    pub job_count: u64,
    pub period_start: DateTime<Utc>,
}

use chrono::Timelike;

// ============================================================
// Account / User / QOS management (sacctmgr operations)
// ============================================================

/// A single bound value in a dynamically built accounting write. The variant
/// carries the concrete type so one builder can mix columns; the `Null*`
/// variants bind SQL `NULL` for `None`, which is how a nullable column is
/// cleared.
#[derive(Clone, Copy)]
enum SqlVal<'a> {
    Text(&'a str),
    NullText(Option<&'a str>),
    Int(i32),
    NullInt(Option<i32>),
    Real(f64),
}

fn push_bound(qb: &mut QueryBuilder<sqlx::Postgres>, val: SqlVal<'_>) {
    match val {
        SqlVal::Text(v) => qb.push_bind(v),
        SqlVal::NullText(v) => qb.push_bind(v),
        SqlVal::Int(v) => qb.push_bind(v),
        SqlVal::NullInt(v) => qb.push_bind(v),
        SqlVal::Real(v) => qb.push_bind(v),
    };
}

/// The tables `upsert_row` can target. A closed set of variants (not a free
/// `&str`) so the table name and ON CONFLICT target spliced into the SQL text
/// can only ever be known literals — no caller can route user input into a
/// SQL identifier position, even by mistake in future edits.
#[derive(Clone, Copy)]
enum UpsertTable {
    Accounts,
    Qos,
}

impl UpsertTable {
    fn name(self) -> &'static str {
        match self {
            UpsertTable::Accounts => "accounts",
            UpsertTable::Qos => "qos",
        }
    }

    fn conflict_target(self) -> &'static str {
        match self {
            UpsertTable::Accounts | UpsertTable::Qos => "name",
        }
    }
}

/// Insert `keys` + `updates`, updating only `updates` columns on conflict
/// (identity `keys` never overwritten; absent columns keep their stored value,
/// take the schema default on insert — the partial-patch contract). No `updates`
/// = create-if-absent. Table is a closed enum, columns `&'static str` — injection-safe.
async fn upsert_row(
    pool: &PgPool,
    table: UpsertTable,
    keys: &[(&'static str, SqlVal<'_>)],
    updates: &[(&'static str, SqlVal<'_>)],
) -> anyhow::Result<()> {
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("INSERT INTO ");
    qb.push(table.name()).push(" (");
    let mut first = true;
    for (col, _) in keys.iter().chain(updates.iter()) {
        if !first {
            qb.push(", ");
        }
        first = false;
        qb.push(*col);
    }
    qb.push(") VALUES (");
    first = true;
    for (_, val) in keys.iter().chain(updates.iter()) {
        if !first {
            qb.push(", ");
        }
        first = false;
        push_bound(&mut qb, *val);
    }
    qb.push(") ON CONFLICT (")
        .push(table.conflict_target())
        .push(")");
    if updates.is_empty() {
        qb.push(" DO NOTHING");
    } else {
        qb.push(" DO UPDATE SET ");
        first = true;
        for (col, _) in updates {
            if !first {
                qb.push(", ");
            }
            first = false;
            qb.push(*col).push(" = EXCLUDED.").push(*col);
        }
    }
    qb.build().execute(pool).await?;
    Ok(())
}

/// Partial-patch fields for [`upsert_account`]. Outer `None` leaves the column
/// unchanged (partial patch on modify); for nullable columns the inner `None`
/// clears it to SQL `NULL`.
#[derive(Default)]
pub struct AccountUpdate<'a> {
    pub description: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub parent: Option<Option<&'a str>>,
    pub fairshare: Option<i32>,
    pub max_running_jobs: Option<Option<i32>>,
    pub grp_tres: Option<Option<&'a str>>,
}

/// Create or update an account, writing only the fields set in `u`. `modify`
/// sends just the restated fields, so unset columns are preserved; `add` sets
/// all of them.
pub async fn upsert_account<'a>(
    pool: &PgPool,
    name: &'a str,
    u: AccountUpdate<'a>,
) -> anyhow::Result<()> {
    let keys = [("name", SqlVal::Text(name))];
    let mut updates: Vec<(&'static str, SqlVal)> = Vec::new();
    if let Some(v) = u.description {
        updates.push(("description", SqlVal::Text(v)));
    }
    if let Some(v) = u.organization {
        updates.push(("organization", SqlVal::Text(v)));
    }
    if let Some(v) = u.parent {
        updates.push(("parent_account", SqlVal::NullText(v)));
    }
    if let Some(v) = u.fairshare {
        updates.push(("fairshare_weight", SqlVal::Int(v)));
    }
    if let Some(v) = u.max_running_jobs {
        updates.push(("max_running_jobs", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.grp_tres {
        updates.push(("grp_tres", SqlVal::NullText(v)));
    }
    upsert_row(pool, UpsertTable::Accounts, &keys, &updates).await
}

/// Delete an account.
pub async fn delete_account(pool: &PgPool, name: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM accounts WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// List all accounts.
pub async fn list_accounts(pool: &PgPool) -> anyhow::Result<Vec<AccountRecord>> {
    let rows = sqlx::query(
        "SELECT name, description, organization, parent_account, fairshare_weight, max_running_jobs, grp_tres FROM accounts ORDER BY name"
    ).fetch_all(pool).await?;

    Ok(rows
        .iter()
        .map(|r| AccountRecord {
            name: r.get("name"),
            description: r.get("description"),
            organization: r.get("organization"),
            parent: r.get("parent_account"),
            fairshare_weight: r.get("fairshare_weight"),
            max_running_jobs: r.get("max_running_jobs"),
            grp_tres: r.get("grp_tres"),
        })
        .collect())
}

#[derive(Debug)]
pub struct AccountRecord {
    pub name: String,
    pub description: String,
    pub organization: String,
    pub parent: Option<String>,
    pub fairshare_weight: i32,
    pub max_running_jobs: Option<i32>,
    /// Account resource allocation as a TRES string; None = unlimited.
    pub grp_tres: Option<String>,
}

/// Update the partition-less association's columns, inserting the row if none
/// exists. `partition_name` is nullable (NULL != NULL), so `ON CONFLICT` can't
/// dedupe; the caller's per-user advisory lock serializes concurrent `add_user`s
/// so two can't double-insert. (`remove_user` takes no lock but only deletes.)
async fn upsert_association(
    conn: &mut PgConnection,
    user: &str,
    account: &str,
    updates: &[(&'static str, SqlVal<'_>)],
) -> anyhow::Result<()> {
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("UPDATE associations SET ");
    let mut first = true;
    for (col, val) in updates {
        if !first {
            qb.push(", ");
        }
        first = false;
        qb.push(*col).push(" = ");
        push_bound(&mut qb, *val);
    }
    qb.push(" WHERE user_name = ").push_bind(user);
    qb.push(" AND account = ").push_bind(account);
    qb.push(" AND (partition_name IS NULL OR partition_name = '')");
    let updated = qb.build().execute(&mut *conn).await?;
    if updated.rows_affected() > 0 {
        return Ok(());
    }

    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("INSERT INTO associations (user_name, account");
    for (col, _) in updates {
        qb.push(", ").push(*col);
    }
    qb.push(") VALUES (");
    qb.push_bind(user).push(", ").push_bind(account);
    for (_, val) in updates {
        qb.push(", ");
        push_bound(&mut qb, *val);
    }
    qb.push(")");
    qb.build().execute(&mut *conn).await?;
    Ok(())
}

/// Partial-patch fields for [`add_user`]. Outer `None` leaves the field
/// unchanged (partial patch on modify); for nullable columns the inner `None`
/// clears it. Numeric limits use `None` (not 0) for "no limit", matching how
/// `list_associations`/`AssociationCache` read an unset limit back out.
#[derive(Default)]
pub struct UserUpdate<'a> {
    pub admin_level: Option<&'a str>,
    pub is_default: Option<bool>,
    pub default_qos: Option<Option<&'a str>>,
    pub allowed_qos: Option<Option<&'a str>>,
    pub max_running_jobs: Option<Option<i32>>,
    pub max_submit_jobs: Option<Option<i32>>,
    pub grp_submit_jobs: Option<Option<i32>>,
    pub max_tres_per_job: Option<Option<&'a str>>,
    pub grp_tres: Option<Option<&'a str>>,
    pub max_wall_min: Option<Option<i32>>,
}

/// Add or modify a user-account association, writing only the fields set in `u`
/// (partial patch). `is_default`: `Some(true)` makes this the default (demoting the
/// user's others), `Some(false)` clears it, `None` preserves it but still defaults a
/// brand-new user's first account. Limits/QOS live in a separate association row.
pub async fn add_user<'a>(
    pool: &PgPool,
    user: &'a str,
    account: &'a str,
    u: UserUpdate<'a>,
) -> anyhow::Result<()> {
    // Per-user advisory lock so concurrent modifies of two different accounts
    // for the same user serialize — otherwise both could win the demote race
    // below and end up default. Cheap: add_user is admin-path.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
        .bind(user)
        .execute(&mut *tx)
        .await?;

    // Clear the default on the user's other rows *before* the upsert sets it
    // here, so the `one_default_account_per_user` unique index never sees two
    // non-null default_account rows mid-transaction.
    if u.is_default == Some(true) {
        sqlx::query(
            "UPDATE users SET default_account = NULL \
             WHERE name = $1 AND account <> $2 AND default_account IS NOT NULL",
        )
        .bind(user)
        .bind(account)
        .execute(&mut *tx)
        .await?;
    }

    // Partial-patch: COALESCE/CASE keep stored admin_level/default_account when
    // omitted ($3/$4 NULL); a brand-new user's first row still claims the default.
    sqlx::query(
        r#"
        INSERT INTO users (name, account, admin_level, default_account)
        VALUES ($1, $2, COALESCE($3, 'none'), CASE
            WHEN $4::bool THEN $2
            WHEN $4::bool IS NULL AND NOT EXISTS (
                SELECT 1 FROM users WHERE name = $1 AND default_account IS NOT NULL
            ) THEN $2
        END)
        ON CONFLICT (name, account) DO UPDATE SET
            admin_level = COALESCE($3, users.admin_level),
            default_account = CASE
                WHEN $4::bool IS NULL THEN users.default_account
                WHEN $4::bool THEN $2
                ELSE NULL
            END
        "#,
    )
    .bind(user)
    .bind(account)
    .bind(u.admin_level)
    .bind(u.is_default)
    .execute(&mut *tx)
    .await?;

    let mut assoc: Vec<(&'static str, SqlVal)> = Vec::new();
    if let Some(v) = u.default_qos {
        assoc.push(("default_qos", SqlVal::NullText(v)));
    }
    if let Some(v) = u.allowed_qos {
        assoc.push(("allowed_qos", SqlVal::NullText(v)));
    }
    if let Some(v) = u.max_running_jobs {
        assoc.push(("max_running_jobs", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_submit_jobs {
        assoc.push(("max_submit_jobs", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.grp_submit_jobs {
        assoc.push(("grp_submit_jobs", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_tres_per_job {
        assoc.push(("max_tres_per_job", SqlVal::NullText(v)));
    }
    if let Some(v) = u.grp_tres {
        assoc.push(("grp_tres", SqlVal::NullText(v)));
    }
    if let Some(v) = u.max_wall_min {
        assoc.push(("max_wall_min", SqlVal::NullInt(v)));
    }
    // Touch the association row (limits/QOS) only when a field was restated.
    if !assoc.is_empty() {
        upsert_association(&mut tx, user, account, &assoc).await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Remove a user from one account, or every account when `account` is empty.
pub async fn remove_user(pool: &PgPool, user: &str, account: &str) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    let associations =
        sqlx::query("DELETE FROM associations WHERE user_name = $1 AND ($2 = '' OR account = $2)")
            .bind(user)
            .bind(account)
            .execute(&mut *tx)
            .await?;
    let users = sqlx::query("DELETE FROM users WHERE name = $1 AND ($2 = '' OR account = $2)")
        .bind(user)
        .bind(account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(associations.rows_affected() + users.rows_affected())
}

/// List users, joining each one's own association row for `default_qos`/
/// `allowed_qos`. `DISTINCT ON ... a.id DESC` picks the newest row if legacy
/// duplicates exist (pre-dating the add_user upsert fix); it never touches
/// the others.
pub async fn list_users(
    pool: &PgPool,
    account: Option<&str>,
    user: Option<&str>,
) -> anyhow::Result<Vec<UserRecord>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (u.name, u.account)
            u.name, u.account, u.admin_level, u.default_account,
            a.default_qos, a.allowed_qos,
            a.max_running_jobs, a.max_submit_jobs, a.grp_submit_jobs,
            a.max_wall_min, a.max_tres_per_job, a.grp_tres
        FROM users u
        LEFT JOIN associations a
            ON a.user_name = u.name AND a.account = u.account
                AND (a.partition_name IS NULL OR a.partition_name = '')
        WHERE ($1::TEXT IS NULL OR u.account = $1)
            AND ($2::TEXT IS NULL OR u.name = $2)
        ORDER BY u.name, u.account, a.id DESC NULLS LAST
        "#,
    )
    .bind(account)
    .bind(user)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| UserRecord {
            name: r.get("name"),
            account: r.get("account"),
            admin_level: r.get("admin_level"),
            default_account: r.get("default_account"),
            default_qos: r.get("default_qos"),
            allowed_qos: r.get("allowed_qos"),
            max_running_jobs: r.get("max_running_jobs"),
            max_submit_jobs: r.get("max_submit_jobs"),
            grp_submit_jobs: r.get("grp_submit_jobs"),
            max_wall_min: r.get("max_wall_min"),
            max_tres_per_job: r.get("max_tres_per_job"),
            grp_tres: r.get("grp_tres"),
        })
        .collect())
}

#[derive(Debug)]
pub struct UserRecord {
    pub name: String,
    pub account: String,
    pub admin_level: String,
    pub default_account: Option<String>,
    pub default_qos: Option<String>,
    pub allowed_qos: Option<String>,
    pub max_running_jobs: Option<i32>,
    pub max_submit_jobs: Option<i32>,
    pub grp_submit_jobs: Option<i32>,
    pub max_wall_min: Option<i32>,
    pub max_tres_per_job: Option<String>,
    pub grp_tres: Option<String>,
}

/// List every user-account association's resource limits, one row per
/// partition-less association — the row the scheduler's admission check
/// enforces against. `DISTINCT ON ... id DESC` mirrors `list_users`: it
/// picks the newest row if legacy duplicates exist.
pub async fn list_associations(pool: &PgPool) -> anyhow::Result<Vec<AssociationRecord>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (user_name, account)
            user_name, account, max_running_jobs, max_submit_jobs, grp_submit_jobs,
            max_tres_per_job, grp_tres, max_wall_min
        FROM associations
        WHERE partition_name IS NULL OR partition_name = ''
        ORDER BY user_name, account, id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| AssociationRecord {
            user_name: r.get("user_name"),
            account: r.get("account"),
            max_running_jobs: r.get("max_running_jobs"),
            max_submit_jobs: r.get("max_submit_jobs"),
            grp_submit_jobs: r.get("grp_submit_jobs"),
            max_tres_per_job: r.get("max_tres_per_job"),
            grp_tres: r.get("grp_tres"),
            max_wall_min: r.get("max_wall_min"),
        })
        .collect())
}

#[derive(Debug)]
pub struct AssociationRecord {
    pub user_name: String,
    pub account: String,
    pub max_running_jobs: Option<i32>,
    pub max_submit_jobs: Option<i32>,
    pub grp_submit_jobs: Option<i32>,
    pub max_tres_per_job: Option<String>,
    pub grp_tres: Option<String>,
    pub max_wall_min: Option<i32>,
}

/// Partial-patch fields for [`upsert_qos`]. Outer `None` leaves the column
/// unchanged (partial patch on modify); for nullable columns the inner `None`
/// clears it to SQL `NULL`.
#[derive(Default)]
pub struct QosUpdate<'a> {
    pub description: Option<&'a str>,
    pub priority: Option<i32>,
    pub preempt_mode: Option<&'a str>,
    /// Comma-separated QOS names; `Some("")` clears the list.
    pub preempt: Option<&'a str>,
    pub usage_factor: Option<f64>,
    pub max_jobs_per_user: Option<Option<i32>>,
    pub max_wall_min: Option<Option<i32>>,
    pub max_tres_per_job: Option<Option<&'a str>>,
    pub max_submit_per_user: Option<Option<i32>>,
    pub max_submit_per_account: Option<Option<i32>>,
    pub grp_submit_jobs: Option<Option<i32>>,
    pub max_tres_per_user: Option<Option<&'a str>>,
    pub grp_tres: Option<Option<&'a str>>,
    pub grp_wall_min: Option<Option<i32>>,
    pub preempt_exempt_time: Option<Option<i32>>,
    pub flags: Option<&'a str>,
}

/// Create or update a QOS, writing only the fields set in `u`. `modify` sends
/// just the restated fields, so unset columns are preserved; `add` sets all of
/// them.
pub async fn upsert_qos<'a>(pool: &PgPool, name: &'a str, u: QosUpdate<'a>) -> anyhow::Result<()> {
    let keys = [("name", SqlVal::Text(name))];
    let mut updates: Vec<(&'static str, SqlVal)> = Vec::new();
    if let Some(v) = u.description {
        updates.push(("description", SqlVal::Text(v)));
    }
    if let Some(v) = u.priority {
        updates.push(("priority", SqlVal::Int(v)));
    }
    if let Some(v) = u.preempt_mode {
        updates.push(("preempt_mode", SqlVal::Text(v)));
    }
    if let Some(v) = u.usage_factor {
        updates.push(("usage_factor", SqlVal::Real(v)));
    }
    if let Some(v) = u.max_jobs_per_user {
        updates.push(("max_jobs_per_user", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_wall_min {
        updates.push(("max_wall_min", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_tres_per_job {
        updates.push(("max_tres_per_job", SqlVal::NullText(v)));
    }
    if let Some(v) = u.max_submit_per_user {
        updates.push(("max_submit_per_user", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_submit_per_account {
        updates.push(("max_submit_per_account", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.grp_submit_jobs {
        updates.push(("grp_submit_jobs", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.max_tres_per_user {
        updates.push(("max_tres_per_user", SqlVal::NullText(v)));
    }
    if let Some(v) = u.grp_tres {
        updates.push(("grp_tres", SqlVal::NullText(v)));
    }
    if let Some(v) = u.grp_wall_min {
        updates.push(("grp_wall_min", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.preempt {
        updates.push(("preempt", SqlVal::Text(v)));
    }
    if let Some(v) = u.preempt_exempt_time {
        updates.push(("preempt_exempt_time", SqlVal::NullInt(v)));
    }
    if let Some(v) = u.flags {
        updates.push(("flags", SqlVal::Text(v)));
    }
    upsert_row(pool, UpsertTable::Qos, &keys, &updates).await
}

/// Delete a QOS.
pub async fn delete_qos(pool: &PgPool, name: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM qos WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a QOS with this name exists. Used to reject setting a
/// nonexistent QOS as a default at write time, rather than only degrading
/// gracefully later when it's read back and no longer resolves.
pub async fn qos_exists(pool: &PgPool, name: &str) -> anyhow::Result<bool> {
    let row = sqlx::query("SELECT 1 FROM qos WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Of the given QOS names, return those that don't exist, in input order.
/// A single query regardless of list length, so validating a user's allow-list
/// doesn't scale DB round-trips with the number of names in it.
pub async fn missing_qos(pool: &PgPool, names: &[&str]) -> anyhow::Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let existing: Vec<String> = sqlx::query_scalar("SELECT name FROM qos WHERE name = ANY($1)")
        .bind(names)
        .fetch_all(pool)
        .await?;
    Ok(names
        .iter()
        .copied()
        .filter(|n| !existing.iter().any(|e| e == n))
        .map(str::to_string)
        .collect())
}

/// List all QOS.
pub async fn list_qos(pool: &PgPool) -> anyhow::Result<Vec<QosRecord>> {
    let rows = sqlx::query(
        "SELECT name, description, priority, preempt_mode, preempt, usage_factor, max_jobs_per_user, max_wall_min, max_tres_per_job, max_submit_per_user, max_submit_per_account, grp_submit_jobs, max_tres_per_user, grp_tres, grp_wall_min, preempt_exempt_time, flags FROM qos ORDER BY name"
    ).fetch_all(pool).await?;

    Ok(rows
        .iter()
        .map(|r| QosRecord {
            name: r.get("name"),
            description: r.get("description"),
            priority: r.get("priority"),
            preempt_mode: r.get("preempt_mode"),
            preempt: r.get::<Option<String>, _>("preempt").unwrap_or_default(),
            // Column is REAL (f32) in the schema; widen to the struct's f64.
            usage_factor: r.get::<f32, _>("usage_factor") as f64,
            max_jobs_per_user: r.get("max_jobs_per_user"),
            max_wall_min: r.get("max_wall_min"),
            max_tres_per_job: r.get("max_tres_per_job"),
            max_submit_per_user: r.get("max_submit_per_user"),
            max_submit_per_account: r.get("max_submit_per_account"),
            grp_submit_jobs: r.get("grp_submit_jobs"),
            max_tres_per_user: r.get("max_tres_per_user"),
            grp_tres: r.get("grp_tres"),
            grp_wall_min: r.get("grp_wall_min"),
            preempt_exempt_time: r.get("preempt_exempt_time"),
            flags: r.get("flags"),
        })
        .collect())
}

#[derive(Debug)]
pub struct QosRecord {
    pub name: String,
    pub description: String,
    pub priority: i32,
    pub preempt_mode: String,
    /// Comma-separated QOS names this QOS may preempt; empty string = none.
    pub preempt: String,
    pub usage_factor: f64,
    pub max_jobs_per_user: Option<i32>,
    pub max_wall_min: Option<i32>,
    pub max_tres_per_job: Option<String>,
    pub max_submit_per_user: Option<i32>,
    pub max_submit_per_account: Option<i32>,
    pub grp_submit_jobs: Option<i32>,
    pub max_tres_per_user: Option<String>,
    pub grp_tres: Option<String>,
    pub grp_wall_min: Option<i32>,
    pub preempt_exempt_time: Option<i32>,
    pub flags: String,
}

#[cfg(test)]
mod job_history_tests {
    use super::*;
    use chrono::Duration;

    fn test_job_id(slot: u32) -> i32 {
        const BASE: i32 = 9_000_000;
        BASE + (std::process::id() as i32 % 10_000) * 10 + slot as i32
    }

    async fn test_pool() -> anyhow::Result<PgPool> {
        let url = std::env::var("DATABASE_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        migrate(&pool).await?;
        Ok(pool)
    }

    async fn delete_jobs(pool: &PgPool, ids: &[i32]) -> anyhow::Result<()> {
        for id in ids {
            sqlx::query("DELETE FROM jobs WHERE job_id = $1")
                .bind(id)
                .execute(pool)
                .await?;
        }
        Ok(())
    }

    /// Standalone-pool wrappers around `record_job_start`/`record_job_end`,
    /// which now take a `&mut PgConnection` so reconciliation can run them
    /// inside a transaction. Tests exercise the pool-per-call path here.
    #[allow(clippy::too_many_arguments)]
    async fn start(
        pool: &PgPool,
        job_id: i32,
        name: &str,
        user: &str,
        account: &str,
        partition: &str,
        num_nodes: i32,
        num_tasks: i32,
        cpus_per_task: i32,
        memory_mb: i64,
        submit_time: DateTime<Utc>,
        start_time: DateTime<Utc>,
        reservation: &str,
    ) -> anyhow::Result<()> {
        start_with_qos(
            pool,
            job_id,
            name,
            user,
            account,
            partition,
            "",
            num_nodes,
            num_tasks,
            cpus_per_task,
            memory_mb,
            submit_time,
            start_time,
            reservation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_qos(
        pool: &PgPool,
        job_id: i32,
        name: &str,
        user: &str,
        account: &str,
        partition: &str,
        qos: &str,
        num_nodes: i32,
        num_tasks: i32,
        cpus_per_task: i32,
        memory_mb: i64,
        submit_time: DateTime<Utc>,
        start_time: DateTime<Utc>,
        reservation: &str,
    ) -> anyhow::Result<()> {
        let mut conn = pool.acquire().await?;
        record_job_start(
            &mut conn,
            &JobStartRecord {
                job_id: job_id as JobId,
                name: name.to_string(),
                user: user.to_string(),
                account: account.to_string(),
                partition: partition.to_string(),
                qos: qos.to_string(),
                num_nodes: num_nodes as u32,
                num_tasks: num_tasks as u32,
                cpus_per_task: cpus_per_task as u32,
                memory_mb: memory_mb as u64,
                submit_time,
                start_time,
                reservation: Some(reservation.to_string()),
            },
        )
        .await
    }

    async fn end(
        pool: &PgPool,
        job_id: i32,
        state: &str,
        exit_code: i32,
        end_time: DateTime<Utc>,
        exit_signal: i32,
        derived_exit_code: i32,
    ) -> anyhow::Result<()> {
        let mut conn = pool.acquire().await?;
        record_job_end(
            &mut conn,
            job_id,
            state,
            exit_code,
            end_time,
            exit_signal,
            derived_exit_code,
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn get_job_history_query_builder() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let id0 = test_job_id(0);
        let id1 = test_job_id(1);
        let id2 = test_job_id(2);
        let ids = [id0, id1, id2];

        let pid = std::process::id();
        let user_a = format!("spur_hist_a_{pid}");
        let user_b = format!("spur_hist_b_{pid}");
        let account_one = format!("spur_acct1_{pid}");
        let account_two = format!("spur_acct2_{pid}");

        let t1 = Utc::now() - Duration::hours(2);
        let t2 = Utc::now() - Duration::hours(1);

        delete_jobs(&pool, &ids).await.ok();

        start(
            &pool,
            id0,
            "job-a",
            &user_a,
            &account_one,
            "debug",
            1,
            1,
            1,
            0,
            t1,
            t1,
            "",
        )
        .await?;
        end(&pool, id0, "COMPLETED", 0, t1 + Duration::minutes(5), 0, 0).await?;

        start(
            &pool,
            id1,
            "job-b",
            &user_b,
            &account_one,
            "debug",
            1,
            1,
            1,
            0,
            t1,
            t1,
            "",
        )
        .await?;
        end(&pool, id1, "FAILED", 137, t1 + Duration::minutes(5), 9, 137).await?;

        start(
            &pool,
            id2,
            "job-c",
            &user_a,
            &account_two,
            "debug",
            1,
            1,
            1,
            0,
            t2,
            t2,
            "",
        )
        .await?;
        end(&pool, id2, "COMPLETED", 0, t2 + Duration::minutes(5), 0, 0).await?;

        let by_user = get_job_history(&pool, Some(&user_a), None, None, None, &[], 100).await?;
        assert_eq!(by_user.len(), 2);
        assert!(by_user.iter().all(|r| r.user_name == user_a));

        let by_account =
            get_job_history(&pool, None, Some(&account_one), None, None, &[], 100).await?;
        assert_eq!(
            by_account
                .iter()
                .filter(|r| ids.contains(&r.job_id))
                .count(),
            2
        );

        let completed = get_job_history(
            &pool,
            Some(&user_a),
            None,
            None,
            None,
            &[String::from("COMPLETED")],
            100,
        )
        .await?;
        assert_eq!(completed.len(), 2);

        let failed = get_job_history(
            &pool,
            Some(&user_b),
            None,
            None,
            None,
            &[String::from("FAILED")],
            100,
        )
        .await?;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].job_id, id1);
        assert_eq!(failed[0].exit_signal, 9);
        assert_eq!(failed[0].derived_exit_code, 137);

        let after = get_job_history(&pool, Some(&user_a), None, Some(t2), None, &[], 100).await?;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].job_id, id2);

        let limited = get_job_history(&pool, Some(&user_a), None, None, None, &[], 1).await?;
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].job_id, id2);

        delete_jobs(&pool, &ids).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn record_job_start_overwrites_reused_job_id() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let id = test_job_id(3);
        delete_jobs(&pool, &[id]).await.ok();

        let submit1 = Utc::now() - Duration::hours(3);
        let start1 = Utc::now() - Duration::hours(2);
        start(
            &pool, id, "old-job", "root", "acct-old", "debug", 2, 4, 2, 8192, submit1, start1, "",
        )
        .await?;
        end(
            &pool,
            id,
            "FAILED",
            137,
            start1 + Duration::minutes(5),
            9,
            137,
        )
        .await?;

        let submit2 = Utc::now() - Duration::minutes(90);
        let start2 = Utc::now() - Duration::hours(1);
        start(
            &pool, id, "new-job", "vm", "acct-new", "gpu", 1, 1, 1, 1024, submit2, start2, "",
        )
        .await?;

        let history = get_job_history(&pool, None, None, None, None, &[], 100)
            .await?
            .into_iter()
            .find(|r| r.job_id == id)
            .expect("reused job_id should still be queryable");
        assert_eq!(history.name, "new-job");
        assert_eq!(history.user_name, "vm");
        assert_eq!(history.account, "acct-new");
        assert_eq!(history.partition, "gpu");
        assert_eq!(history.state, "RUNNING");
        assert_eq!(history.exit_code, 0);
        assert_eq!(history.exit_signal, 0);
        assert_eq!(history.derived_exit_code, 0);
        assert!(history.end_time.is_none());
        assert_eq!(
            history.submit_time.timestamp(),
            submit2.timestamp(),
            "submit_time must not carry over from the previous job"
        );

        delete_jobs(&pool, &[id]).await?;
        Ok(())
    }

    /// GrpWall attributes consumption by the QOS recorded on each job, so a start
    /// that fails to persist it leaves every budget reading zero and the limit
    /// silently unenforced. Exercise the write and the aggregate together.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn consumed_wall_minutes_attributes_a_job_to_the_qos_it_ran_under() -> anyhow::Result<()>
    {
        let pool = test_pool().await?;
        let inside = test_job_id(4);
        let running = test_job_id(5);
        let expired = test_job_id(6);
        let clipped = test_job_id(7);
        let unqualified = test_job_id(8);
        let ids = [inside, running, expired, clipped, unqualified];
        delete_jobs(&pool, &ids).await.ok();

        let qos = format!("spur_gw_{}", std::process::id());
        let other = format!("{qos}_other");
        let clip = format!("{qos}_clip");
        let now = Utc::now();

        // Finished 30 minutes ago, ran 20 minutes: fully inside the window.
        start_with_qos(
            &pool,
            inside,
            "inside",
            "root",
            "",
            "",
            &qos,
            1,
            1,
            1,
            0,
            now - Duration::minutes(60),
            now - Duration::minutes(50),
            "",
        )
        .await?;
        end(
            &pool,
            inside,
            "COMPLETED",
            0,
            now - Duration::minutes(30),
            0,
            0,
        )
        .await?;

        // Still running: contributes the 10 minutes accrued so far. Seeded half a
        // minute past 10 so the truncating division cannot tip to 9 or 11 if the
        // test body takes a moment to reach the aggregate.
        start_with_qos(
            &pool,
            running,
            "running",
            "root",
            "",
            "",
            &qos,
            1,
            1,
            1,
            0,
            now - Duration::minutes(15),
            now - Duration::seconds(630),
            "",
        )
        .await?;

        // Ran a year ago under a different QOS: outside the window entirely.
        start_with_qos(
            &pool,
            expired,
            "expired",
            "root",
            "",
            "",
            &other,
            1,
            1,
            1,
            0,
            now - Duration::days(365),
            now - Duration::days(365),
            "",
        )
        .await?;
        end(
            &pool,
            expired,
            "COMPLETED",
            0,
            now - Duration::days(365) + Duration::minutes(90),
            0,
            0,
        )
        .await?;

        // Started 20 days ago and ended 2 days ago: with a 14-day window only the
        // last 12 days count. This is the only case that exercises the lower clamp
        // on `start_time`; without it, the whole 18-day run would be charged.
        start_with_qos(
            &pool,
            clipped,
            "clipped",
            "root",
            "",
            "",
            &clip,
            1,
            1,
            1,
            0,
            now - Duration::days(20),
            now - Duration::days(20),
            "",
        )
        .await?;
        end(
            &pool,
            clipped,
            "COMPLETED",
            0,
            now - Duration::days(2),
            0,
            0,
        )
        .await?;

        // No QOS: the `qos <> ''` filter is the only thing keeping every un-QOS'd
        // job in the cluster out of one shared bucket, so assert it is applied.
        start_with_qos(
            &pool,
            unqualified,
            "unqualified",
            "root",
            "",
            "",
            "",
            1,
            1,
            1,
            0,
            now - Duration::minutes(90),
            now - Duration::minutes(90),
            "",
        )
        .await?;
        end(
            &pool,
            unqualified,
            "COMPLETED",
            0,
            now - Duration::minutes(30),
            0,
            0,
        )
        .await?;

        let consumed = consumed_wall_minutes_by_qos(&pool, 14).await?;
        assert_eq!(
            consumed.get(&qos).copied(),
            Some(30),
            "20 finished + 10 accrued minutes must be attributed to the job's QOS"
        );
        assert_eq!(
            consumed.get(&other),
            None,
            "a job older than the window must not contribute"
        );
        assert_eq!(
            consumed.get(&clip).copied(),
            Some(12 * 24 * 60),
            "a job that began before the window must contribute only the part inside it"
        );
        assert_eq!(
            consumed.get(""),
            None,
            "jobs with no QOS must not collect in a shared bucket"
        );

        delete_jobs(&pool, &ids).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn list_qos_round_trips_all_limits() -> anyhow::Result<()> {
        // Regression: usage_factor is REAL (f32) in the schema; decoding it as
        // f64 panicked the worker and broke ListQos (and the controller's QoS
        // cache) until widened. Exercise the full upsert -> list path.
        let pool = test_pool().await?;
        let name = format!("spur_qos_{}", std::process::id());
        sqlx::query("DELETE FROM qos WHERE name = $1")
            .bind(&name)
            .execute(&pool)
            .await?;

        upsert_qos(
            &pool,
            &name,
            QosUpdate {
                description: Some("d"),
                priority: Some(5),
                preempt_mode: Some("cluster"),
                preempt: None,
                usage_factor: Some(1.5),
                max_jobs_per_user: Some(Some(3)),
                max_wall_min: Some(Some(60)),
                max_tres_per_job: Some(Some("cpu=2")),
                max_submit_per_user: Some(Some(4)),
                max_submit_per_account: Some(Some(7)),
                grp_submit_jobs: Some(Some(9)),
                max_tres_per_user: Some(Some("cpu=16")),
                grp_tres: Some(Some("cpu=64")),
                grp_wall_min: Some(Some(120)),
                preempt_exempt_time: None,
                flags: Some("DenyOnLimit"),
            },
        )
        .await?;

        let got = list_qos(&pool)
            .await?
            .into_iter()
            .find(|q| q.name == name)
            .expect("qos present");
        assert_eq!(got.usage_factor, 1.5);
        assert_eq!(got.priority, 5);
        assert_eq!(got.max_jobs_per_user, Some(3));
        assert_eq!(got.max_wall_min, Some(60));
        assert_eq!(got.max_tres_per_job.as_deref(), Some("cpu=2"));
        assert_eq!(got.max_submit_per_user, Some(4));
        assert_eq!(got.max_submit_per_account, Some(7));
        assert_eq!(got.grp_submit_jobs, Some(9));
        assert_eq!(got.max_tres_per_user.as_deref(), Some("cpu=16"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=64"));
        assert_eq!(got.grp_wall_min, Some(120));
        assert_eq!(got.flags, "DenyOnLimit");

        sqlx::query("DELETE FROM qos WHERE name = $1")
            .bind(&name)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_round_trips_default_qos() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_qosdef_user_{pid}");
        let account = format!("spur_qosdef_acct_{pid}");
        let qos_name = format!("spur_qosdef_qos_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM qos WHERE name = $1")
            .bind(&qos_name)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;
        upsert_qos(
            &pool,
            &qos_name,
            QosUpdate {
                description: Some("d"),
                ..Default::default()
            },
        )
        .await?;

        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(true),
                default_qos: Some(Some(&qos_name)),
                ..Default::default()
            },
        )
        .await?;
        let got = list_users(&pool, Some(&account), None)
            .await?
            .into_iter()
            .find(|u| u.name == user)
            .expect("user present");
        assert_eq!(got.default_qos.as_deref(), Some(qos_name.as_str()));

        // An unrestated default_qos must be preserved while the restated field
        // is still applied.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                max_running_jobs: Some(Some(5)),
                ..Default::default()
            },
        )
        .await?;
        let got = list_users(&pool, Some(&account), None)
            .await?
            .into_iter()
            .find(|u| u.name == user)
            .expect("user present");
        assert_eq!(got.default_qos.as_deref(), Some(qos_name.as_str()));

        // An explicit empty default_qos clears it.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                default_qos: Some(None),
                ..Default::default()
            },
        )
        .await?;
        let got = list_users(&pool, Some(&account), None)
            .await?
            .into_iter()
            .find(|u| u.name == user)
            .expect("user present");
        assert_eq!(got.default_qos, None);

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM qos WHERE name = $1")
            .bind(&qos_name)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_is_default_patches_default_account_only_when_restated() -> anyhow::Result<()>
    {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_defacct_user_{pid}");
        let account = format!("spur_defacct_acct_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                ..Default::default()
            },
        )
        .await?;

        let default_account = |pool: PgPool, account: String, user: String| async move {
            list_users(&pool, Some(&account), None)
                .await
                .unwrap()
                .into_iter()
                .find(|u| u.name == user)
                .expect("user present")
                .default_account
        };

        // add with is_default=true records this account as the user's default.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            default_account(pool.clone(), account.clone(), user.clone())
                .await
                .as_deref(),
            Some(account.as_str())
        );

        // An unrestated is_default must leave default_account untouched.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                max_running_jobs: Some(Some(5)),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            default_account(pool.clone(), account.clone(), user.clone())
                .await
                .as_deref(),
            Some(account.as_str()),
            "unrestated is_default must not touch default_account"
        );

        // Restating is_default=false clears default_account.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                is_default: Some(false),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            default_account(pool.clone(), account.clone(), user.clone()).await,
            None
        );

        // Restating is_default=true sets it back.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            default_account(pool.clone(), account.clone(), user.clone())
                .await
                .as_deref(),
            Some(account.as_str())
        );

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_default_account_is_unique_per_user() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_1def_user_{pid}");
        let acct_a = format!("spur_1def_a_{pid}");
        let acct_b = format!("spur_1def_b_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
            upsert_account(
                &pool,
                a,
                AccountUpdate {
                    description: Some("d"),
                    ..Default::default()
                },
            )
            .await?;
        }

        // alice's default starts as account A, then B is made the default.
        add_user(
            &pool,
            &user,
            &acct_a,
            UserUpdate {
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;
        add_user(
            &pool,
            &user,
            &acct_b,
            UserUpdate {
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;

        // Only B remains the user's default in the users table; A is demoted.
        let users = list_users(&pool, None, Some(&user)).await?;
        let a_row = users
            .iter()
            .find(|u| u.account == acct_a)
            .expect("A row present");
        let b_row = users
            .iter()
            .find(|u| u.account == acct_b)
            .expect("B row present");
        assert_eq!(
            a_row.default_account, None,
            "A must be demoted when B becomes default"
        );
        assert_eq!(b_row.default_account.as_deref(), Some(acct_b.as_str()));

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_defaults_first_account_and_later_adds_dont_demote() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_defadd_user_{pid}");
        let acct_a = format!("spur_defadd_a_{pid}");
        let acct_b = format!("spur_defadd_b_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
            upsert_account(
                &pool,
                a,
                AccountUpdate {
                    description: Some("d"),
                    ..Default::default()
                },
            )
            .await?;
        }

        // A plain add (is_default = None) makes a user's first account the default.
        add_user(&pool, &user, &acct_a, UserUpdate::default()).await?;
        // A later plain add to another account must not demote that default.
        add_user(&pool, &user, &acct_b, UserUpdate::default()).await?;

        let users = list_users(&pool, None, Some(&user)).await?;
        let a_row = users
            .iter()
            .find(|u| u.account == acct_a)
            .expect("A row present");
        let b_row = users
            .iter()
            .find(|u| u.account == acct_b)
            .expect("B row present");
        assert_eq!(
            a_row.default_account.as_deref(),
            Some(acct_a.as_str()),
            "a user's first account must become the default"
        );
        assert_eq!(
            b_row.default_account, None,
            "a plain add must not steal the default from an existing account"
        );

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn migrate_upgrades_pre_fix_default_account_schema() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_dedup_user_{pid}");
        // Names chosen so acct_a sorts before acct_b: the dedup keeps the lowest.
        let acct_a = format!("spur_dedup_a_{pid}");
        let acct_b = format!("spur_dedup_b_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
            upsert_account(
                &pool,
                a,
                AccountUpdate {
                    description: Some("d"),
                    ..Default::default()
                },
            )
            .await?;
        }

        // Reconstruct a pre-fix database: the dead is_default column present and
        // two rows marked default (which the live index forbids). Drop the index,
        // restore the column, inject the rows, then let migrate() run the upgrade.
        sqlx::query("DROP INDEX IF EXISTS one_default_account_per_user")
            .execute(&pool)
            .await?;
        sqlx::query(
            "ALTER TABLE associations \
             ADD COLUMN IF NOT EXISTS is_default BOOLEAN NOT NULL DEFAULT false",
        )
        .execute(&pool)
        .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query(
                "INSERT INTO users (name, account, admin_level, default_account) \
                 VALUES ($1, $2, 'none', $2)",
            )
            .bind(&user)
            .bind(a)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO associations (user_name, account, is_default) VALUES ($1, $2, true)",
            )
            .bind(&user)
            .bind(a)
            .execute(&pool)
            .await?;
        }

        // Re-running the schema drops the dead column, collapses the duplicates,
        // then rebuilds the index on the now-clean data.
        migrate(&pool).await?;

        let has_is_default: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_name = 'associations' AND column_name = 'is_default')",
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            !has_is_default,
            "migrate must drop the redundant is_default column"
        );

        let users = list_users(&pool, None, Some(&user)).await?;
        let with_default: Vec<&str> = users
            .iter()
            .filter(|u| u.default_account.is_some())
            .map(|u| u.account.as_str())
            .collect();
        assert_eq!(
            with_default,
            vec![acct_a.as_str()],
            "exactly one default survives, the lowest account name"
        );

        // The rebuilt index now rejects a second default for the user.
        let dup = sqlx::query(
            "UPDATE users SET default_account = account WHERE name = $1 AND account = $2",
        )
        .bind(&user)
        .bind(&acct_b)
        .execute(&pool)
        .await;
        assert!(
            dup.is_err(),
            "unique index must reject a second default account"
        );

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        for a in [&acct_a, &acct_b] {
            sqlx::query("DELETE FROM accounts WHERE name = $1")
                .bind(a)
                .execute(&pool)
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn migrate_serializes_concurrent_runs() -> anyhow::Result<()> {
        // Two controllers booting against one database run migrate() at once;
        // the advisory lock must serialize them so neither errors nor deadlocks.
        let pool = test_pool().await?;
        let (first, second) = tokio::join!(migrate(&pool), migrate(&pool));
        first?;
        second?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn missing_qos_reports_only_absent_names() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let present_a = format!("spur_mq_a_{pid}");
        let present_b = format!("spur_mq_b_{pid}");
        let absent = format!("spur_mq_absent_{pid}");

        for q in [&present_a, &present_b, &absent] {
            sqlx::query("DELETE FROM qos WHERE name = $1")
                .bind(q)
                .execute(&pool)
                .await?;
        }
        for q in [&present_a, &present_b] {
            upsert_qos(
                &pool,
                q,
                QosUpdate {
                    description: Some("d"),
                    ..Default::default()
                },
            )
            .await?;
        }

        // Empty input short-circuits without a query and reports nothing.
        assert!(missing_qos(&pool, &[]).await?.is_empty());

        // Only the absent name is returned, preserving input order and
        // ignoring existing names in any position.
        let missing = missing_qos(
            &pool,
            &[present_a.as_str(), absent.as_str(), present_b.as_str()],
        )
        .await?;
        assert_eq!(missing, vec![absent.clone()]);

        // All-present resolves to no missing.
        assert!(
            missing_qos(&pool, &[present_a.as_str(), present_b.as_str()])
                .await?
                .is_empty()
        );

        for q in [&present_a, &present_b, &absent] {
            sqlx::query("DELETE FROM qos WHERE name = $1")
                .bind(q)
                .execute(&pool)
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_round_trips_account_limits() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_assoclimru_user_{pid}");
        let account = format!("spur_assoclimru_acct_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;

        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(true),
                max_running_jobs: Some(Some(2)),
                max_submit_jobs: Some(Some(4)),
                max_tres_per_job: Some(Some("cpu=8")),
                grp_tres: Some(Some("cpu=32")),
                max_wall_min: Some(Some(60)),
                ..Default::default()
            },
        )
        .await?;

        let got = list_associations(&pool)
            .await?
            .into_iter()
            .find(|a| a.user_name == user && a.account == account)
            .expect("association present");
        assert_eq!(got.max_running_jobs, Some(2));
        assert_eq!(got.max_submit_jobs, Some(4));
        assert_eq!(got.max_tres_per_job.as_deref(), Some("cpu=8"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=32"));
        assert_eq!(got.max_wall_min, Some(60));

        // A partial update that restates only max_running_jobs must overwrite
        // that one limit while preserving every limit it didn't restate.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                max_running_jobs: Some(Some(10)),
                ..Default::default()
            },
        )
        .await?;
        let got = list_associations(&pool)
            .await?
            .into_iter()
            .find(|a| a.user_name == user && a.account == account)
            .expect("association present");
        assert_eq!(got.max_running_jobs, Some(10));
        assert_eq!(got.max_submit_jobs, Some(4));
        assert_eq!(got.max_tres_per_job.as_deref(), Some("cpu=8"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=32"));
        assert_eq!(got.max_wall_min, Some(60));

        // Explicitly clearing each limit (inner None) sets it back to no-limit.
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                max_running_jobs: Some(None),
                max_submit_jobs: Some(None),
                max_tres_per_job: Some(None),
                grp_tres: Some(None),
                max_wall_min: Some(None),
                ..Default::default()
            },
        )
        .await?;
        let got = list_associations(&pool)
            .await?
            .into_iter()
            .find(|a| a.user_name == user && a.account == account)
            .expect("association present");
        assert_eq!(got.max_running_jobs, None);
        assert_eq!(got.max_submit_jobs, None);
        assert_eq!(got.max_tres_per_job, None);
        assert_eq!(got.grp_tres, None);
        assert_eq!(got.max_wall_min, None);

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn list_users_filters_by_user_name() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let account = format!("spur_userfilter_acct_{pid}");
        let matching_user = format!("spur_userfilter_match_{pid}");
        let other_user = format!("spur_userfilter_other_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name IN ($1, $2)")
            .bind(&matching_user)
            .bind(&other_user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name IN ($1, $2)")
            .bind(&matching_user)
            .bind(&other_user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;
        add_user(
            &pool,
            &matching_user,
            &account,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;
        add_user(
            &pool,
            &other_user,
            &account,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await?;

        let users = list_users(&pool, None, Some(&matching_user)).await?;
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, matching_user);

        let users = list_users(&pool, Some(&account), Some(&matching_user)).await?;
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].account, account);

        let users = list_users(&pool, Some(&account), Some("missing-user")).await?;
        assert!(users.is_empty());

        sqlx::query("DELETE FROM associations WHERE user_name IN ($1, $2)")
            .bind(&matching_user)
            .bind(&other_user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name IN ($1, $2)")
            .bind(&matching_user)
            .bind(&other_user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn add_user_never_creates_a_duplicate_association_row() -> anyhow::Result<()> {
        // Repeated add_user calls (what `sacctmgr modify user` now does)
        // must converge on one row, not accumulate duplicates.
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_nodupe_user_{pid}");
        let account = format!("spur_nodupe_acct_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;

        let base = || UserUpdate {
            admin_level: Some("none"),
            is_default: Some(true),
            max_running_jobs: Some(Some(1)),
            ..Default::default()
        };
        add_user(&pool, &user, &account, base()).await?;
        add_user(&pool, &user, &account, base()).await?;
        add_user(
            &pool,
            &user,
            &account,
            UserUpdate {
                default_qos: Some(Some("highprio-does-not-need-to-exist")),
                ..base()
            },
        )
        .await?;

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM associations WHERE user_name = $1 AND account = $2",
        )
        .bind(&user)
        .bind(&account)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            count, 1,
            "repeated add_user must update in place, not accumulate rows"
        );

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn remove_user_deletes_one_or_all_account_associations() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_remove_user_{pid}");
        let account_one = format!("spur_remove_acct_one_{pid}");
        let account_two = format!("spur_remove_acct_two_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name IN ($1, $2)")
            .bind(&account_one)
            .bind(&account_two)
            .execute(&pool)
            .await?;

        let acct_update = || AccountUpdate {
            description: Some("d"),
            organization: Some("o"),
            fairshare: Some(1),
            ..Default::default()
        };
        upsert_account(&pool, &account_one, acct_update()).await?;
        upsert_account(&pool, &account_two, acct_update()).await?;
        add_user(
            &pool,
            &user,
            &account_one,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(true),
                max_running_jobs: Some(Some(1)),
                ..Default::default()
            },
        )
        .await?;
        add_user(
            &pool,
            &user,
            &account_two,
            UserUpdate {
                admin_level: Some("none"),
                is_default: Some(false),
                max_running_jobs: Some(Some(1)),
                ..Default::default()
            },
        )
        .await?;

        let deleted = remove_user(&pool, &user, &account_one).await?;
        assert_eq!(deleted, 2);
        let remaining = list_users(&pool, None, None).await?;
        assert!(!remaining
            .iter()
            .any(|record| record.name == user && record.account == account_one));
        assert!(remaining
            .iter()
            .any(|record| record.name == user && record.account == account_two));

        let deleted = remove_user(&pool, &user, "").await?;
        assert_eq!(deleted, 2);
        let remaining = list_users(&pool, None, None).await?;
        assert!(!remaining.iter().any(|record| record.name == user));
        let association_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM associations WHERE user_name = $1")
                .bind(&user)
                .fetch_one(&pool)
                .await?;
        assert_eq!(association_count, 0);

        let deleted = remove_user(&pool, &user, "").await?;
        assert_eq!(deleted, 0);

        sqlx::query("DELETE FROM accounts WHERE name IN ($1, $2)")
            .bind(&account_one)
            .bind(&account_two)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn list_users_reads_default_qos_from_a_legacy_empty_partition_row() -> anyhow::Result<()>
    {
        // The join must match partition_name = '' as well as NULL, same as
        // add_user's UPDATE, or this row's default_qos would be invisible.
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_emptypart_user_{pid}");
        let account = format!("spur_emptypart_acct_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;
        sqlx::query("INSERT INTO users (name, account, admin_level) VALUES ($1, $2, 'none')")
            .bind(&user)
            .bind(&account)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO associations (user_name, account, partition_name, default_qos) \
             VALUES ($1, $2, '', 'highprio')",
        )
        .bind(&user)
        .bind(&account)
        .execute(&pool)
        .await?;

        let got = list_users(&pool, Some(&account), None)
            .await?
            .into_iter()
            .find(|u| u.name == user)
            .expect("user present");
        assert_eq!(got.default_qos.as_deref(), Some("highprio"));

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn list_associations_reads_limit_columns() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let pid = std::process::id();
        let user = format!("spur_assoclim_user_{pid}");
        let account = format!("spur_assoclim_acct_{pid}");

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                ..Default::default()
            },
        )
        .await?;
        sqlx::query(
            "INSERT INTO associations \
             (user_name, account, max_running_jobs, max_submit_jobs, max_tres_per_job, grp_tres, max_wall_min) \
             VALUES ($1, $2, 3, 5, 'cpu=2', 'cpu=16', 60)",
        )
        .bind(&user)
        .bind(&account)
        .execute(&pool)
        .await?;

        let got = list_associations(&pool)
            .await?
            .into_iter()
            .find(|a| a.user_name == user && a.account == account)
            .expect("association present");
        assert_eq!(got.max_running_jobs, Some(3));
        assert_eq!(got.max_submit_jobs, Some(5));
        assert_eq!(got.max_tres_per_job.as_deref(), Some("cpu=2"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=16"));
        assert_eq!(got.max_wall_min, Some(60));

        sqlx::query("DELETE FROM associations WHERE user_name = $1")
            .bind(&user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn account_grp_tres_preserves_on_omit_and_clears_on_explicit() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let account = format!("spur_acct_grptres_{}", std::process::id());
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                description: Some("d"),
                organization: Some("o"),
                grp_tres: Some(Some("cpu=16,mem=32768,gres/gpu=8")),
                ..Default::default()
            },
        )
        .await?;
        let got = list_accounts(&pool)
            .await?
            .into_iter()
            .find(|a| a.name == account)
            .expect("account present");
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=16,mem=32768,gres/gpu=8"));

        // An unrestated grp_tres must be preserved while the restated field is
        // still applied.
        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                fairshare: Some(5),
                ..Default::default()
            },
        )
        .await?;
        let got = list_accounts(&pool)
            .await?
            .into_iter()
            .find(|a| a.name == account)
            .expect("account present");
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=16,mem=32768,gres/gpu=8"));
        assert_eq!(got.fairshare_weight, 5);

        // An explicit empty grp_tres (inner None) clears it.
        upsert_account(
            &pool,
            &account,
            AccountUpdate {
                grp_tres: Some(None),
                ..Default::default()
            },
        )
        .await?;
        let got = list_accounts(&pool)
            .await?
            .into_iter()
            .find(|a| a.name == account)
            .expect("account present");
        assert_eq!(got.grp_tres, None);

        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod txn_tests {
    use super::*;
    use crate::accounting::txn::{TxnAction, TxnEntity, TxnOutcome, TxnRecord, TxnSource};
    use chrono::Duration;

    #[test]
    fn effective_query_limit_defaults_and_caps() {
        assert_eq!(effective_query_limit(0), i64::from(DEFAULT_QUERY_LIMIT));
        assert_eq!(effective_query_limit(50), 50);
        assert_eq!(
            effective_query_limit(MAX_QUERY_LIMIT),
            i64::from(MAX_QUERY_LIMIT)
        );
        assert_eq!(effective_query_limit(1_000_000), i64::from(MAX_QUERY_LIMIT));
        assert_eq!(effective_query_limit(u32::MAX), i64::from(MAX_QUERY_LIMIT));
    }

    async fn test_pool() -> anyhow::Result<PgPool> {
        let url = std::env::var("DATABASE_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        migrate(&pool).await?;
        Ok(pool)
    }

    fn sample(
        entity: &str,
        action: TxnAction,
        outcome: TxnOutcome,
        ts: DateTime<Utc>,
    ) -> TxnRecord {
        TxnRecord {
            ts,
            actor: format!("actor_{}", std::process::id()),
            actor_uid: Some(1000),
            verified: true,
            source: TxnSource::Api,
            action,
            entity_type: TxnEntity::Reservation,
            entity_name: entity.to_string(),
            outcome,
            details: r#"{"k":"v"}"#.to_string(),
        }
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn record_and_query_transactions() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let entity = format!("resv_txn_{}", std::process::id());
        let actor = format!("actor_{}", std::process::id());

        sqlx::query("DELETE FROM txn WHERE entity_name = $1")
            .bind(&entity)
            .execute(&pool)
            .await?;

        let now = Utc::now();
        let mut conn = pool.acquire().await?;
        record_txn(
            &mut conn,
            &sample(
                &entity,
                TxnAction::Create,
                TxnOutcome::Success,
                now - Duration::minutes(2),
            ),
        )
        .await?;
        record_txn(
            &mut conn,
            &sample(
                &entity,
                TxnAction::Delete,
                TxnOutcome::Denied,
                now - Duration::minutes(1),
            ),
        )
        .await?;

        // Filter by entity_name; expect newest-first ordering.
        let rows = get_transactions(
            &pool,
            &TxnFilter {
                entity_name: Some(&entity),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "delete");
        assert_eq!(rows[0].outcome, "denied");
        assert_eq!(rows[1].action, "create");
        assert_eq!(rows[0].actor, actor);
        assert_eq!(rows[0].actor_uid, Some(1000));
        assert!(rows[0].verified);

        let denied = get_transactions(
            &pool,
            &TxnFilter {
                entity_name: Some(&entity),
                outcome: Some("denied"),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].action, "delete");

        sqlx::query("DELETE FROM txn WHERE entity_name = $1")
            .bind(&entity)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn purge_removes_old_rows() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let entity = format!("resv_purge_{}", std::process::id());

        sqlx::query("DELETE FROM txn WHERE entity_name = $1")
            .bind(&entity)
            .execute(&pool)
            .await?;

        let now = Utc::now();
        let mut conn = pool.acquire().await?;
        record_txn(
            &mut conn,
            &sample(
                &entity,
                TxnAction::Create,
                TxnOutcome::Success,
                now - Duration::days(10),
            ),
        )
        .await?;
        record_txn(
            &mut conn,
            &sample(&entity, TxnAction::Update, TxnOutcome::Success, now),
        )
        .await?;

        let removed = purge_txn(&pool, now - Duration::days(1)).await?;
        assert!(removed >= 1);

        let rows = get_transactions(
            &pool,
            &TxnFilter {
                entity_name: Some(&entity),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "update");

        sqlx::query("DELETE FROM txn WHERE entity_name = $1")
            .bind(&entity)
            .execute(&pool)
            .await?;
        Ok(())
    }
}
