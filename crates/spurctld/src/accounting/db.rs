// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{PgPool, QueryBuilder, Row};

/// Run database migrations (create tables if they don't exist).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    Ok(())
}

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
    usage_factor    REAL NOT NULL DEFAULT 1.0,
    max_jobs_per_user INTEGER,
    max_submit_per_user INTEGER,
    max_tres_per_job TEXT,
    max_tres_per_user TEXT,
    grp_tres        TEXT,
    max_wall_min    INTEGER,
    grp_wall_min    INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS associations (
    id              SERIAL PRIMARY KEY,
    user_name       TEXT NOT NULL,
    account         TEXT NOT NULL REFERENCES accounts(name),
    partition_name  TEXT,
    fairshare_weight INTEGER NOT NULL DEFAULT 1,
    is_default      BOOLEAN NOT NULL DEFAULT false,
    max_running_jobs INTEGER,
    max_submit_jobs INTEGER,
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
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS grp_tres TEXT;
"#;

/// Record a job start in the database.
///
/// Takes a `&mut PgConnection` (not a hard `&PgPool`) so callers can either
/// acquire a standalone connection from a pool (as the notifier does) or pass
/// one borrowed from an open `Transaction` (`Transaction` derefs to
/// `PgConnection`) to run this alongside other writes atomically, as
/// reconciliation's backfill-then-finalize does.
#[allow(clippy::too_many_arguments)]
pub async fn record_job_start(
    conn: &mut PgConnection,
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
    // job_id reuse after a Raft wipe means a conflict is a new, unrelated job.
    sqlx::query(
        r#"
        INSERT INTO jobs (job_id, name, user_name, account, partition_name, num_nodes, num_tasks, cpus_per_task, memory_mb, submit_time, start_time, state, reservation)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'RUNNING', $12)
        ON CONFLICT (job_id) DO UPDATE SET
            name = EXCLUDED.name,
            user_name = EXCLUDED.user_name,
            account = EXCLUDED.account,
            partition_name = EXCLUDED.partition_name,
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
    .bind(job_id)
    .bind(name)
    .bind(user)
    .bind(account)
    .bind(partition)
    .bind(num_nodes)
    .bind(num_tasks)
    .bind(cpus_per_task)
    .bind(memory_mb)
    .bind(submit_time)
    .bind(start_time)
    .bind(reservation)
    .execute(&mut *conn)
    .await?;

    // If end_time is already set, the end notification arrived first and skipped
    // usage computation (start_time was NULL at that point). Compute it now.
    let row = sqlx::query(
        "SELECT user_name, account, start_time, num_tasks, cpus_per_task, end_time FROM jobs WHERE job_id = $1",
    )
    .bind(job_id)
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

    qb.push(" ORDER BY submit_time DESC");
    let effective_limit: i64 = if limit > 0 { limit.into() } else { 1000 };
    qb.push(" LIMIT ").push_bind(effective_limit);

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

/// Create or update an account. Like `upsert_qos`/`add_user`, this is a full
/// resend on modify, not a partial patch: a `None` `grp_tres` clears it.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_account(
    pool: &PgPool,
    name: &str,
    description: &str,
    organization: &str,
    parent: Option<&str>,
    fairshare: i32,
    max_running_jobs: Option<i32>,
    grp_tres: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO accounts (name, description, organization, parent_account, fairshare_weight, max_running_jobs, grp_tres)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (name) DO UPDATE SET
            description = $2, organization = $3, parent_account = $4,
            fairshare_weight = $5, max_running_jobs = $6, grp_tres = $7
        "#,
    )
    .bind(name).bind(description).bind(organization)
    .bind(parent).bind(fairshare).bind(max_running_jobs).bind(grp_tres)
    .execute(pool).await?;
    Ok(())
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

/// Add a user-account association. Like `upsert_account`/`upsert_qos`, this
/// is a full resend on modify, not a partial patch: an empty `default_qos`,
/// or a `None`/zero limit, clears any existing value rather than preserving
/// it. Numeric limits use `None` (not 0) for "no limit", matching how
/// `list_associations`/`AssociationCache` read an unset limit back out.
#[allow(clippy::too_many_arguments)]
pub async fn add_user(
    pool: &PgPool,
    user: &str,
    account: &str,
    admin_level: &str,
    is_default: bool,
    default_qos: &str,
    allowed_qos: &str,
    max_running_jobs: Option<i32>,
    max_submit_jobs: Option<i32>,
    max_tres_per_job: Option<&str>,
    grp_tres: Option<&str>,
    max_wall_min: Option<i32>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO users (name, account, admin_level, default_account)
        VALUES ($1, $2, $3, CASE WHEN $4 THEN $2 ELSE NULL END)
        ON CONFLICT (name, account) DO UPDATE SET admin_level = $3
        "#,
    )
    .bind(user)
    .bind(account)
    .bind(admin_level)
    .bind(is_default)
    .execute(pool)
    .await?;

    // partition_name is nullable (NULL != NULL), so ON CONFLICT on it can't
    // upsert; update explicitly, serialized per (user, account) so two
    // concurrent first-time calls can't both insert.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1 || ':' || $2)::bigint)")
        .bind(user)
        .bind(account)
        .execute(&mut *tx)
        .await?;

    let updated = sqlx::query(
        r#"
        UPDATE associations SET is_default = $3, default_qos = NULLIF($4, ''),
            allowed_qos = NULLIF($5, ''),
            max_running_jobs = $6, max_submit_jobs = $7, max_tres_per_job = $8,
            grp_tres = $9, max_wall_min = $10
        WHERE user_name = $1 AND account = $2
          AND (partition_name IS NULL OR partition_name = '')
        "#,
    )
    .bind(user)
    .bind(account)
    .bind(is_default)
    .bind(default_qos)
    .bind(allowed_qos)
    .bind(max_running_jobs)
    .bind(max_submit_jobs)
    .bind(max_tres_per_job)
    .bind(grp_tres)
    .bind(max_wall_min)
    .execute(&mut *tx)
    .await?;

    if updated.rows_affected() == 0 {
        sqlx::query(
            r#"
            INSERT INTO associations (user_name, account, is_default, default_qos,
                allowed_qos, max_running_jobs, max_submit_jobs, max_tres_per_job,
                grp_tres, max_wall_min)
            VALUES ($1, $2, $3, NULLIF($4, ''), NULLIF($5, ''), $6, $7, $8, $9, $10)
            "#,
        )
        .bind(user)
        .bind(account)
        .bind(is_default)
        .bind(default_qos)
        .bind(allowed_qos)
        .bind(max_running_jobs)
        .bind(max_submit_jobs)
        .bind(max_tres_per_job)
        .bind(grp_tres)
        .bind(max_wall_min)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Remove a user from an account.
pub async fn remove_user(pool: &PgPool, user: &str, account: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM users WHERE name = $1 AND account = $2")
        .bind(user)
        .bind(account)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM associations WHERE user_name = $1 AND account = $2")
        .bind(user)
        .bind(account)
        .execute(pool)
        .await?;
    Ok(())
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
            a.default_qos, a.allowed_qos
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
}

/// List every user-account association's resource limits, one row per
/// partition-less association — the row the scheduler's admission check
/// enforces against. `DISTINCT ON ... id DESC` mirrors `list_users`: it
/// picks the newest row if legacy duplicates exist.
pub async fn list_associations(pool: &PgPool) -> anyhow::Result<Vec<AssociationRecord>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (user_name, account)
            user_name, account, max_running_jobs, max_submit_jobs,
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
    pub max_tres_per_job: Option<String>,
    pub grp_tres: Option<String>,
    pub max_wall_min: Option<i32>,
}

/// Create or update a QOS.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_qos(
    pool: &PgPool,
    name: &str,
    description: &str,
    priority: i32,
    preempt_mode: &str,
    usage_factor: f64,
    max_jobs_per_user: Option<i32>,
    max_wall_min: Option<i32>,
    max_tres_per_job: Option<&str>,
    max_submit_per_user: Option<i32>,
    max_tres_per_user: Option<&str>,
    grp_tres: Option<&str>,
    grp_wall_min: Option<i32>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO qos (name, description, priority, preempt_mode, usage_factor,
                         max_jobs_per_user, max_wall_min, max_tres_per_job,
                         max_submit_per_user, max_tres_per_user, grp_tres, grp_wall_min)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (name) DO UPDATE SET
            description = $2, priority = $3, preempt_mode = $4, usage_factor = $5,
            max_jobs_per_user = $6, max_wall_min = $7, max_tres_per_job = $8,
            max_submit_per_user = $9, max_tres_per_user = $10, grp_tres = $11,
            grp_wall_min = $12
        "#,
    )
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(preempt_mode)
    .bind(usage_factor)
    .bind(max_jobs_per_user)
    .bind(max_wall_min)
    .bind(max_tres_per_job)
    .bind(max_submit_per_user)
    .bind(max_tres_per_user)
    .bind(grp_tres)
    .bind(grp_wall_min)
    .execute(pool)
    .await?;
    Ok(())
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

/// List all QOS.
pub async fn list_qos(pool: &PgPool) -> anyhow::Result<Vec<QosRecord>> {
    let rows = sqlx::query(
        "SELECT name, description, priority, preempt_mode, usage_factor, max_jobs_per_user, max_wall_min, max_tres_per_job, max_submit_per_user, max_tres_per_user, grp_tres, grp_wall_min FROM qos ORDER BY name"
    ).fetch_all(pool).await?;

    Ok(rows
        .iter()
        .map(|r| QosRecord {
            name: r.get("name"),
            description: r.get("description"),
            priority: r.get("priority"),
            preempt_mode: r.get("preempt_mode"),
            // Column is REAL (f32) in the schema; widen to the struct's f64.
            usage_factor: r.get::<f32, _>("usage_factor") as f64,
            max_jobs_per_user: r.get("max_jobs_per_user"),
            max_wall_min: r.get("max_wall_min"),
            max_tres_per_job: r.get("max_tres_per_job"),
            max_submit_per_user: r.get("max_submit_per_user"),
            max_tres_per_user: r.get("max_tres_per_user"),
            grp_tres: r.get("grp_tres"),
            grp_wall_min: r.get("grp_wall_min"),
        })
        .collect())
}

#[derive(Debug)]
pub struct QosRecord {
    pub name: String,
    pub description: String,
    pub priority: i32,
    pub preempt_mode: String,
    pub usage_factor: f64,
    pub max_jobs_per_user: Option<i32>,
    pub max_wall_min: Option<i32>,
    pub max_tres_per_job: Option<String>,
    pub max_submit_per_user: Option<i32>,
    pub max_tres_per_user: Option<String>,
    pub grp_tres: Option<String>,
    pub grp_wall_min: Option<i32>,
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
        let mut conn = pool.acquire().await?;
        record_job_start(
            &mut conn,
            job_id,
            name,
            user,
            account,
            partition,
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
            "d",
            5,
            "cluster",
            1.5,
            Some(3),
            Some(60),
            Some("cpu=2"),
            Some(4),
            Some("cpu=16"),
            Some("cpu=64"),
            Some(120),
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
        assert_eq!(got.max_tres_per_user.as_deref(), Some("cpu=16"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=64"));
        assert_eq!(got.grp_wall_min, Some(120));

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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;
        upsert_qos(
            &pool, &qos_name, "d", 0, "off", 1.0, None, None, None, None, None, None, None,
        )
        .await?;

        add_user(
            &pool, &user, &account, "none", true, &qos_name, "", None, None, None, None, None,
        )
        .await?;
        let got = list_users(&pool, Some(&account), None)
            .await?
            .into_iter()
            .find(|u| u.name == user)
            .expect("user present");
        assert_eq!(got.default_qos.as_deref(), Some(qos_name.as_str()));

        // Upsert again with an empty default_qos: clears it (full resend,
        // not a partial patch — matches modify account/qos semantics).
        add_user(
            &pool, &user, &account, "none", true, "", "", None, None, None, None, None,
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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;

        add_user(
            &pool,
            &user,
            &account,
            "none",
            true,
            "",
            "",
            Some(2),
            Some(4),
            Some("cpu=8"),
            Some("cpu=32"),
            Some(60),
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

        // Upsert again with different non-zero limits: the UPDATE branch
        // must overwrite the existing values, not merge with them.
        add_user(
            &pool,
            &user,
            &account,
            "none",
            true,
            "",
            "",
            Some(10),
            Some(20),
            Some("cpu=16"),
            Some("cpu=64"),
            Some(120),
        )
        .await?;
        let got = list_associations(&pool)
            .await?
            .into_iter()
            .find(|a| a.user_name == user && a.account == account)
            .expect("association present");
        assert_eq!(got.max_running_jobs, Some(10));
        assert_eq!(got.max_submit_jobs, Some(20));
        assert_eq!(got.max_tres_per_job.as_deref(), Some("cpu=16"));
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=64"));
        assert_eq!(got.max_wall_min, Some(120));

        // Upsert again with no limits: clears them (full resend, not a
        // partial patch — matches every other field on this association).
        add_user(
            &pool, &user, &account, "none", true, "", "", None, None, None, None, None,
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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;
        add_user(
            &pool,
            &matching_user,
            &account,
            "none",
            true,
            "",
            "",
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
        add_user(
            &pool,
            &other_user,
            &account,
            "none",
            true,
            "",
            "",
            None,
            None,
            None,
            None,
            None,
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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;

        add_user(
            &pool, &user, &account, "none", true, "", "", None, None, None, None, None,
        )
        .await?;
        add_user(
            &pool, &user, &account, "none", true, "", "", None, None, None, None, None,
        )
        .await?;
        add_user(
            &pool,
            &user,
            &account,
            "none",
            true,
            "highprio-does-not-need-to-exist",
            "",
            None,
            None,
            None,
            None,
            None,
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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;
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

        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;
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
    async fn account_grp_tres_round_trips_and_clears() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let account = format!("spur_acct_grptres_{}", std::process::id());
        sqlx::query("DELETE FROM accounts WHERE name = $1")
            .bind(&account)
            .execute(&pool)
            .await?;

        upsert_account(
            &pool,
            &account,
            "d",
            "o",
            None,
            1,
            None,
            Some("cpu=16,mem=32768,gres/gpu=8"),
        )
        .await?;
        let got = list_accounts(&pool)
            .await?
            .into_iter()
            .find(|a| a.name == account)
            .expect("account present");
        assert_eq!(got.grp_tres.as_deref(), Some("cpu=16,mem=32768,gres/gpu=8"));

        // Full-resend upsert with no allocation clears it (same contract as qos/user).
        upsert_account(&pool, &account, "d", "o", None, 1, None, None).await?;
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
