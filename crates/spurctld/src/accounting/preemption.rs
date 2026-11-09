// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use spur_core::job::{Job, JobId};
use spur_core::preemption_usage::{PreemptionInitiator, PreemptionUsage, UnusedReason};
use spur_core::rbac::ReadScope;
use spur_proto::proto::{
    GetPreemptionUsageRequest, GetPreemptionUsageResponse, PreemptionUsageEntry,
    UnusedPreemptionCount,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS preemption_checkpoints (
    job_id BIGINT NOT NULL,
    incarnation TEXT NOT NULL,
    user_name TEXT NOT NULL,
    account TEXT NOT NULL,
    used NUMERIC(40,0) NOT NULL DEFAULT 0,
    unused NUMERIC(40,0) NOT NULL DEFAULT 0,
    open NUMERIC(40,0) NOT NULL DEFAULT 0,
    PRIMARY KEY (job_id, incarnation)
);
CREATE TABLE IF NOT EXISTS preemption_rollups (
    user_name TEXT NOT NULL,
    account TEXT NOT NULL,
    used NUMERIC(40,0) NOT NULL DEFAULT 0 CHECK (used >= 0),
    unused NUMERIC(40,0) NOT NULL DEFAULT 0 CHECK (unused >= 0),
    open NUMERIC(40,0) NOT NULL DEFAULT 0 CHECK (open >= 0),
    PRIMARY KEY (user_name, account)
);
CREATE INDEX IF NOT EXISTS idx_preemption_rollups_account
    ON preemption_rollups(account, user_name);
ALTER TABLE preemption_checkpoints ADD COLUMN IF NOT EXISTS unused_by_cause JSONB NOT NULL DEFAULT '{}';
CREATE TABLE IF NOT EXISTS preemption_cause_rollups (
    user_name TEXT NOT NULL,
    account TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('started_elsewhere', 'launch_failed', 'cancelled', 'ended_other')),
    initiator TEXT NOT NULL CHECK (initiator IN ('user', 'admin', 'system', 'unknown')),
    count NUMERIC(40,0) NOT NULL CHECK (count >= 0),
    PRIMARY KEY (user_name, account, reason, initiator)
);

CREATE TABLE IF NOT EXISTS preemption_migrations (name TEXT PRIMARY KEY);

-- Checkpoints deliberately outlive job history; retries must not import it twice.
WITH migration AS (
    INSERT INTO preemption_migrations VALUES ('retained_jobs')
    ON CONFLICT DO NOTHING RETURNING name
), imported AS (
    INSERT INTO preemption_checkpoints (job_id, incarnation, user_name, account, used, unused, open, unused_by_cause)
    SELECT job_id, 'legacy', user_name, account,
        COALESCE((preemption_usage->>'used')::numeric, 0),
        COALESCE((SELECT SUM(count.value::numeric)
            FROM jsonb_each(COALESCE(preemption_usage->'unused', '{}'::jsonb)) reason,
                 jsonb_each_text(reason.value) count), 0),
        COALESCE((preemption_usage->>'open')::numeric, 0),
        COALESCE(preemption_usage->'unused', '{}'::jsonb)
    FROM jobs WHERE EXISTS (SELECT 1 FROM migration) AND COALESCE((preemption_usage->>'revision')::numeric, 0) > 0
    ON CONFLICT DO NOTHING
    RETURNING user_name, account, used, unused, open, unused_by_cause
), imported_causes AS (
    INSERT INTO preemption_cause_rollups (user_name, account, reason, initiator, count)
    SELECT user_name, account, reason.key, initiator.key, SUM(initiator.value::numeric)
    FROM imported,
         jsonb_each(unused_by_cause) reason,
         jsonb_each_text(reason.value) initiator
    GROUP BY user_name, account, reason.key, initiator.key
    ON CONFLICT (user_name, account, reason, initiator) DO UPDATE SET
        count = preemption_cause_rollups.count + EXCLUDED.count
    RETURNING user_name
)
INSERT INTO preemption_rollups (user_name, account, used, unused, open)
SELECT user_name, account, SUM(used), SUM(unused), SUM(open)
FROM imported GROUP BY user_name, account
ON CONFLICT (user_name, account) DO UPDATE SET
    used = preemption_rollups.used + EXCLUDED.used,
    unused = preemption_rollups.unused + EXCLUDED.unused,
    open = preemption_rollups.open + EXCLUDED.open;
"#;

#[derive(Clone)]
pub(crate) struct Checkpoint {
    pub job_id: JobId,
    pub submit_time: DateTime<Utc>,
    pub incarnation: String,
    pub user: String,
    pub account: String,
    pub usage: PreemptionUsage,
}

impl From<&Job> for Checkpoint {
    fn from(job: &Job) -> Self {
        Self {
            job_id: job.job_id,
            submit_time: job.submit_time,
            incarnation: job
                .spec
                .submitted_at
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| "legacy".into()),
            user: job.spec.user.clone(),
            account: job.spec.account.clone().unwrap_or_default(),
            usage: job.preemption_usage.clone(),
        }
    }
}

pub(crate) fn revision(usage: &PreemptionUsage) -> u128 {
    2 * (u128::from(usage.used) + u128::from(usage.unused_total())) + u128::from(usage.open)
}

type CauseCounts = BTreeMap<UnusedReason, BTreeMap<PreemptionInitiator, u64>>;

fn cause_deltas(old: &CauseCounts, new: &CauseCounts) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut deltas = Vec::new();
    for reason in UnusedReason::ALL {
        for initiator in PreemptionInitiator::ALL {
            let count = |counts: &CauseCounts| {
                counts
                    .get(&reason)
                    .and_then(|counts| counts.get(&initiator))
                    .copied()
                    .unwrap_or(0)
            };
            let delta = count(new)
                .checked_sub(count(old))
                .ok_or_else(|| anyhow::anyhow!("preemption cause count regressed"))?;
            if delta > 0 {
                deltas.push(serde_json::json!({
                    "reason": reason.as_str(),
                    "initiator": initiator.as_str(),
                    "count": delta.to_string(),
                }));
            }
        }
    }
    Ok(deltas)
}

// The caller owns the transaction, including the checkpoint and rollup deltas.
pub(super) async fn write(conn: &mut PgConnection, checkpoint: &Checkpoint) -> anyhow::Result<()> {
    if revision(&checkpoint.usage) == 0 {
        return Ok(());
    }
    sqlx::query("INSERT INTO preemption_checkpoints (job_id, incarnation, user_name, account) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(i64::from(checkpoint.job_id)).bind(&checkpoint.incarnation)
        .bind(&checkpoint.user).bind(&checkpoint.account).execute(&mut *conn).await?;
    let old = sqlx::query("SELECT user_name, account, used::text, unused::text, open::text, unused_by_cause FROM preemption_checkpoints WHERE job_id=$1 AND incarnation=$2 FOR UPDATE")
        .bind(i64::from(checkpoint.job_id)).bind(&checkpoint.incarnation).fetch_one(&mut *conn).await?;
    let used: u64 = old.try_get::<String, _>("used")?.parse()?;
    let unused: u64 = old.try_get::<String, _>("unused")?.parse()?;
    let open: u64 = old.try_get::<String, _>("open")?.parse()?;
    let old_revision = 2 * (u128::from(used) + u128::from(unused)) + u128::from(open);
    let old_causes = old
        .try_get::<sqlx::types::Json<CauseCounts>, _>("unused_by_cause")?
        .0;
    if old_revision > revision(&checkpoint.usage)
        || (old_revision == revision(&checkpoint.usage) && old_causes == checkpoint.usage.unused)
    {
        return Ok(());
    }
    let new = &checkpoint.usage;
    let deltas = cause_deltas(&old_causes, &new.unused)?;
    // Retain the requesting association captured by the first export.
    sqlx::query(
        "INSERT INTO preemption_rollups (user_name, account) VALUES ($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(old.try_get::<String, _>("user_name")?)
    .bind(old.try_get::<String, _>("account")?)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE preemption_rollups SET used=used+$3::text::numeric, unused=unused+$4::text::numeric, open=open+$5::text::numeric WHERE user_name=$1 AND account=$2")
        .bind(old.try_get::<String, _>("user_name")?).bind(old.try_get::<String, _>("account")?)
        .bind((i128::from(new.used)-i128::from(used)).to_string())
        .bind((i128::from(new.unused_total())-i128::from(unused)).to_string())
        .bind((i128::from(new.open)-i128::from(open)).to_string())
        .execute(&mut *conn).await?;
    if !deltas.is_empty() {
        sqlx::query("INSERT INTO preemption_cause_rollups (user_name, account, reason, initiator, count) SELECT $1,$2,cause.reason,cause.initiator,cause.count::numeric FROM jsonb_to_recordset($3) AS cause(reason text, initiator text, count text) ON CONFLICT (user_name, account, reason, initiator) DO UPDATE SET count=preemption_cause_rollups.count+EXCLUDED.count")
        .bind(old.try_get::<String, _>("user_name")?).bind(old.try_get::<String, _>("account")?)
        .bind(sqlx::types::Json(deltas)).execute(&mut *conn).await?;
    }
    sqlx::query("UPDATE preemption_checkpoints SET used=$3::text::numeric, unused=$4::text::numeric, open=$5::text::numeric, unused_by_cause=$6 WHERE job_id=$1 AND incarnation=$2")
        .bind(i64::from(checkpoint.job_id)).bind(&checkpoint.incarnation)
        .bind(new.used.to_string()).bind(new.unused_total().to_string()).bind(new.open.to_string())
        .bind(sqlx::types::Json(&new.unused))
        .execute(&mut *conn).await?;
    Ok(())
}

pub(super) fn spawn_loop(pool: PgPool, cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            if !raft.ensure_leader().await {
                continue;
            }
            if let Err(error) = sync_once(&pool, &cluster).await {
                tracing::warn!(%error, "preemption accounting export failed; retaining unexported jobs");
            }
        }
    });
}

pub(crate) async fn sync_once(pool: &PgPool, cluster: &ClusterManager) -> anyhow::Result<()> {
    for checkpoint in cluster.pending_preemption_checkpoints(1000) {
        tokio::time::timeout(super::notifier::ATTEMPT_TIMEOUT, async {
            let mut tx = pool.begin().await?;
            write(&mut tx, &checkpoint).await?;
            tx.commit().await?;
            anyhow::Ok(())
        })
        .await??;
        cluster.acknowledge_preemption_checkpoint(&checkpoint);
    }
    Ok(())
}

pub(super) struct Page {
    limit: u32,
    after: Option<(String, String)>,
}

impl Page {
    pub(super) fn parse(req: &GetPreemptionUsageRequest) -> Result<Self, tonic::Status> {
        if req.limit > 1000
            || req.page_token.len() > 8192
            || req.user.len() > 1024
            || req.account.len() > 1024
        {
            return Err(tonic::Status::invalid_argument(
                "limit must be at most 1000; filter or cursor too long",
            ));
        }
        let after = if req.page_token.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&req.page_token)
                    .map_err(|_| tonic::Status::invalid_argument("invalid page token"))?,
            )
        };
        Ok(Self {
            limit: if req.limit == 0 { 100 } else { req.limit },
            after,
        })
    }
}

pub(super) async fn report(
    pool: &PgPool,
    req: &GetPreemptionUsageRequest,
    page: Page,
    scope: &ReadScope,
) -> anyhow::Result<GetPreemptionUsageResponse> {
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT user_name, account, used::text, unused::text, open::text, ",
    );
    if req.by_cause {
        query.push("COALESCE((SELECT jsonb_agg(jsonb_build_object('reason', c.reason, 'initiator', c.initiator, 'count', c.count::text) ORDER BY c.reason, c.initiator) FROM preemption_cause_rollups c WHERE c.user_name=preemption_rollups.user_name AND c.account=preemption_rollups.account AND c.count > 0), '[]'::jsonb)");
    } else {
        query.push("'[]'::jsonb");
    }
    query.push(" AS unused_by_cause FROM preemption_rollups WHERE TRUE");
    if !req.user.is_empty() {
        query.push(" AND user_name=").push_bind(&req.user);
    }
    if !req.account.is_empty() {
        query.push(" AND account=").push_bind(&req.account);
    }
    match scope {
        ReadScope::All => {}
        ReadScope::Caller(user) => {
            query.push(" AND user_name=").push_bind(user);
        }
        ReadScope::Coordinated { user, accounts } => {
            query
                .push(" AND (user_name=")
                .push_bind(user)
                .push(" OR account=ANY(")
                .push_bind(accounts)
                .push("))");
        }
    }
    if let Some((user, account)) = &page.after {
        query
            .push(" AND (user_name, account) > (")
            .push_bind(user)
            .push(",")
            .push_bind(account)
            .push(")");
    }
    query
        .push(" ORDER BY user_name, account LIMIT ")
        .push_bind(i64::from(page.limit) + 1);
    let rows = query.build().fetch_all(pool).await?;
    let more = rows.len() > page.limit as usize;
    let entries: Vec<PreemptionUsageEntry> = rows
        .into_iter()
        .take(page.limit as usize)
        .map(|row| {
            #[derive(serde::Deserialize)]
            struct Cause {
                reason: String,
                initiator: String,
                count: String,
            }
            let unused: u64 = row.try_get::<String, _>("unused")?.parse()?;
            let mut classified = 0u64;
            let unused_by_cause = row
                .try_get::<sqlx::types::Json<Vec<Cause>>, _>("unused_by_cause")?
                .0
                .into_iter()
                .map(|cause| {
                    let count = cause.count.parse()?;
                    classified = classified
                        .checked_add(count)
                        .ok_or_else(|| anyhow::anyhow!("preemption cause total overflow"))?;
                    Ok(UnusedPreemptionCount {
                        reason: cause.reason,
                        initiator: cause.initiator,
                        count,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(PreemptionUsageEntry {
                user: row.try_get("user_name")?,
                account: row.try_get("account")?,
                used: row.try_get::<String, _>("used")?.parse()?,
                unused,
                open: row.try_get::<String, _>("open")?.parse()?,
                unused_by_cause,
                unclassified_unused: if req.by_cause {
                    unused
                        .checked_sub(classified)
                        .ok_or_else(|| anyhow::anyhow!("preemption causes exceed unused total"))?
                } else {
                    0
                },
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let next_page_token = match entries.last().filter(|_| more) {
        Some(last) => serde_json::to_string(&(&last.user, &last.account))?,
        None => String::new(),
    };
    Ok(GetPreemptionUsageResponse {
        causes_included: req.by_cause,
        entries,
        next_page_token,
        scoped_to_user: match scope {
            ReadScope::Caller(user) => Some(user.clone()),
            _ => None,
        },
        partial: matches!(scope, ReadScope::Coordinated { .. }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::preemption_usage::{PreemptionInitiator, UnusedReason};

    #[test]
    fn revision_increases_for_open_and_settlement_without_u64_overflow() {
        let mut usage = PreemptionUsage::default();
        assert_eq!(revision(&usage), 0);
        usage.open_one();
        assert_eq!(revision(&usage), 1);
        usage.settle(None);
        assert_eq!(revision(&usage), 2);
        usage.used = u64::MAX;
        assert_eq!(revision(&usage), 2 * u128::from(u64::MAX));
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn durable_preemption_rollups_replay_cleanup_pagination_and_privacy() -> anyhow::Result<()>
    {
        let url = std::env::var("DATABASE_URL")?;
        let admin = PgPool::connect(&url).await?;
        let schema = format!("preemption_rollups_test_{}", std::process::id());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let search = format!("SET search_path TO {schema}");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |conn, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(sqlx::AssertSqlSafe(search))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await?;
        super::super::db::migrate(&pool).await?;
        let result = async {
            exercise(&pool).await?;
            exercise_causes(&pool).await
        }
        .await;
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        result
    }

    async fn save(pool: &PgPool, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        let mut tx = pool.begin().await?;
        write(&mut tx, checkpoint).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn query(
        pool: &PgPool,
        request: GetPreemptionUsageRequest,
        scope: ReadScope,
    ) -> anyhow::Result<GetPreemptionUsageResponse> {
        let page = Page::parse(&request)?;
        report(pool, &request, page, &scope).await
    }

    async fn exercise(pool: &PgPool) -> anyhow::Result<()> {
        let mut alice = Checkpoint {
            job_id: 1,
            submit_time: DateTime::UNIX_EPOCH,
            incarnation: "legacy".into(),
            user: "alice".into(),
            account: "research".into(),
            usage: PreemptionUsage::default(),
        };
        alice.usage.open_one();
        save(pool, &alice).await?;
        let open = alice.clone();
        alice.usage.settle(None);
        let (a, b) = tokio::join!(save(pool, &alice), save(pool, &alice));
        a?;
        b?;
        save(pool, &open).await?;
        let first = query(pool, Default::default(), ReadScope::All).await?;
        assert_eq!(
            (
                first.entries[0].used,
                first.entries[0].unused,
                first.entries[0].open
            ),
            (1, 0, 0)
        );

        alice.usage.open_one();
        alice
            .usage
            .settle(Some((UnusedReason::Cancelled, PreemptionInitiator::User)));
        let mut tx = pool.begin().await?;
        write(&mut tx, &alice).await?;
        tx.rollback().await?;
        assert_eq!(
            query(pool, Default::default(), ReadScope::All)
                .await?
                .entries[0]
                .unused,
            0
        );
        save(pool, &alice).await?;
        // A later submission with the same numeric ID is a new contribution.
        let mut reused = alice.clone();
        reused.submit_time += chrono::Duration::days(1);
        reused.incarnation = reused.submit_time.to_rfc3339();
        reused.usage = PreemptionUsage {
            used: 4,
            ..Default::default()
        };
        save(pool, &reused).await?;
        let mut bob = reused.clone();
        bob.job_id = 2;
        bob.user = "bob".into();
        save(pool, &bob).await?;
        let mut carol = bob.clone();
        carol.job_id = 3;
        carol.user = "carol".into();
        carol.account = "other".into();
        save(pool, &carol).await?;

        let first = query(
            pool,
            GetPreemptionUsageRequest {
                limit: 1,
                ..Default::default()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(first.entries.len(), 1);
        assert_eq!((first.entries[0].used, first.entries[0].unused), (5, 1));
        let second = query(
            pool,
            GetPreemptionUsageRequest {
                limit: 1,
                page_token: first.next_page_token,
                ..Default::default()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(second.entries[0].user, "bob");
        let third = query(
            pool,
            GetPreemptionUsageRequest {
                limit: 1,
                page_token: second.next_page_token,
                ..Default::default()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(third.entries[0].user, "carol");
        assert!(third.next_page_token.is_empty());
        let private = query(pool, Default::default(), ReadScope::Caller("bob".into())).await?;
        assert_eq!(private.entries.len(), 1);
        assert_eq!(private.scoped_to_user.as_deref(), Some("bob"));
        let hidden = query(
            pool,
            GetPreemptionUsageRequest {
                user: "alice".into(),
                ..Default::default()
            },
            ReadScope::Caller("bob".into()),
        )
        .await?;
        assert!(hidden.entries.is_empty());
        let coordinated = query(
            pool,
            Default::default(),
            ReadScope::Coordinated {
                user: "outsider".into(),
                accounts: vec!["research".into()],
            },
        )
        .await?;
        assert_eq!(coordinated.entries.len(), 2);
        assert!(coordinated.partial);
        assert!(
            query(pool, Default::default(), ReadScope::Caller(String::new()))
                .await?
                .entries
                .is_empty()
        );

        use spur_proto::proto::slurm_accounting_server::SlurmAccounting;
        let service = super::super::grpc::AccountingService::unavailable("test");
        service.install_pool(pool.clone());
        for category in [
            spur_core::config::PrivateData::Jobs,
            spur_core::config::PrivateData::Usage,
        ] {
            service.attach_auth_source(move || {
                let mut auth = spur_core::config::AuthConfig::default();
                auth.private_data.insert(category.clone());
                auth
            });
            for (user, admin, expected) in [("bob", false, 1), ("bob", true, 3), ("", false, 0)] {
                let mut request = tonic::Request::new(GetPreemptionUsageRequest {
                    by_cause: true,
                    ..Default::default()
                });
                request.extensions_mut().insert(spur_core::auth::Identity {
                    user: user.into(),
                    uid: 1000,
                    gid: 1000,
                    is_admin: admin,
                    trusted_unix: false,
                });
                let reply = service.get_preemption_usage(request).await?.into_inner();
                assert_eq!(reply.entries.len(), expected);
                assert!(reply.causes_included);
                for entry in &reply.entries {
                    let expected_causes = if entry.user == "alice" { 1 } else { 0 };
                    assert_eq!(entry.unused_by_cause.len(), expected_causes);
                }
            }
        }

        // Backfill retained history exactly once, then remove history and retry old exports.
        sqlx::query("INSERT INTO jobs(job_id, user_name, account, submit_time, preemption_usage) VALUES (4,'dan','research',$1,$2)")
            .bind(DateTime::UNIX_EPOCH).bind(serde_json::json!({"used":2,"unused":{"cancelled":{"user":3}},"open":1,"revision":11})).execute(pool).await?;
        sqlx::query("DELETE FROM preemption_migrations")
            .execute(pool)
            .await?;
        super::super::db::migrate(pool).await?;
        super::super::db::migrate(pool).await?;
        let backfill = query(
            pool,
            GetPreemptionUsageRequest {
                user: "dan".into(),
                by_cause: true,
                ..Default::default()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(
            (
                backfill.entries[0].used,
                backfill.entries[0].unused,
                backfill.entries[0].open
            ),
            (2, 3, 1)
        );
        assert_causes(&backfill.entries[0], &[("cancelled", "user", 3)], 0);
        sqlx::query("DELETE FROM jobs").execute(pool).await?;
        save(pool, &open).await?;
        save(pool, &alice).await?;
        let retained = query(pool, Default::default(), ReadScope::All).await?;
        assert_eq!(retained.entries.len(), 4);
        assert_eq!(
            (
                retained.entries[0].used,
                retained.entries[0].unused,
                retained.entries[0].open
            ),
            (5, 1, 0)
        );
        Ok(())
    }

    fn assert_causes(
        entry: &PreemptionUsageEntry,
        expected: &[(&str, &str, u64)],
        unclassified: u64,
    ) {
        let actual: Vec<_> = entry
            .unused_by_cause
            .iter()
            .map(|cause| (cause.reason.as_str(), cause.initiator.as_str(), cause.count))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(entry.unclassified_unused, unclassified);
        assert_eq!(
            entry.unused,
            actual.iter().map(|cause| cause.2).sum::<u64>() + unclassified
        );
    }

    async fn exercise_causes(pool: &PgPool) -> anyhow::Result<()> {
        sqlx::query(
            "TRUNCATE preemption_checkpoints, preemption_rollups, preemption_cause_rollups",
        )
        .execute(pool)
        .await?;
        // An older exporter saved the total but no cause checkpoint.
        sqlx::query("INSERT INTO preemption_checkpoints (job_id, incarnation, user_name, account, unused) VALUES (10,'legacy','alice','research',3)")
            .execute(pool).await?;
        sqlx::query("INSERT INTO preemption_rollups (user_name, account, unused) VALUES ('alice','research',3)")
            .execute(pool).await?;
        let detailed = GetPreemptionUsageRequest {
            by_cause: true,
            ..Default::default()
        };
        let before = query(pool, detailed.clone(), ReadScope::All).await?;
        assert_causes(&before.entries[0], &[], 3);
        let mut checkpoint = Checkpoint {
            job_id: 10,
            submit_time: DateTime::UNIX_EPOCH,
            incarnation: "legacy".into(),
            user: "alice".into(),
            account: "research".into(),
            usage: PreemptionUsage::default(),
        };
        for cause in [
            (UnusedReason::Cancelled, PreemptionInitiator::User),
            (UnusedReason::Cancelled, PreemptionInitiator::User),
            (UnusedReason::LaunchFailed, PreemptionInitiator::System),
        ] {
            checkpoint.usage.open_one();
            checkpoint.usage.settle(Some(cause));
        }
        let (a, b) = tokio::join!(save(pool, &checkpoint), save(pool, &checkpoint));
        a?;
        b?;
        let enriched = query(pool, detailed.clone(), ReadScope::All).await?;
        assert_causes(
            &enriched.entries[0],
            &[("cancelled", "user", 2), ("launch_failed", "system", 1)],
            0,
        );
        let stale = checkpoint.clone();
        // Later association edits must not move cause deltas to a different owner.
        checkpoint.user = "bob".into();
        checkpoint.account = "other".into();
        checkpoint.usage.open_one();
        checkpoint
            .usage
            .settle(Some((UnusedReason::Cancelled, PreemptionInitiator::Admin)));
        let mut tx = pool.begin().await?;
        write(&mut tx, &checkpoint).await?;
        tx.rollback().await?;
        let rolled_back = query(pool, detailed.clone(), ReadScope::All).await?;
        assert_eq!(rolled_back, enriched);
        save(pool, &checkpoint).await?;
        save(pool, &stale).await?;
        let settled = query(pool, detailed.clone(), ReadScope::All).await?;
        assert_eq!(settled.entries.len(), 1);
        assert_eq!(settled.entries[0].user, "alice");
        assert_causes(
            &settled.entries[0],
            &[
                ("cancelled", "admin", 1),
                ("cancelled", "user", 2),
                ("launch_failed", "system", 1),
            ],
            0,
        );
        // Reused job IDs have their own deduplication key and requesting association.
        checkpoint.incarnation = DateTime::UNIX_EPOCH.to_rfc3339();
        save(pool, &checkpoint).await?;
        let filtered = GetPreemptionUsageRequest {
            account: "other".into(),
            ..detailed.clone()
        };
        let bob = query(pool, filtered.clone(), ReadScope::Caller("bob".into())).await?;
        assert_eq!(bob.entries.len(), 1);
        assert_causes(
            &bob.entries[0],
            &[
                ("cancelled", "admin", 1),
                ("cancelled", "user", 2),
                ("launch_failed", "system", 1),
            ],
            0,
        );
        assert!(query(pool, filtered, ReadScope::Caller("alice".into()))
            .await?
            .entries
            .is_empty());
        let first = query(
            pool,
            GetPreemptionUsageRequest {
                limit: 1,
                ..detailed.clone()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(first.entries, settled.entries);
        let second = query(
            pool,
            GetPreemptionUsageRequest {
                limit: 1,
                page_token: first.next_page_token,
                ..detailed.clone()
            },
            ReadScope::All,
        )
        .await?;
        assert_eq!(second.entries, bob.entries);
        assert!(second.next_page_token.is_empty());
        let summary = query(pool, Default::default(), ReadScope::All).await?;
        assert!(!summary.causes_included);
        assert!(summary
            .entries
            .iter()
            .all(|e| e.unused_by_cause.is_empty() && e.unclassified_unused == 0));
        // Re-running schema setup and exports after history deletion cannot add counts.
        sqlx::query("DELETE FROM jobs").execute(pool).await?;
        super::super::db::migrate(pool).await?;
        save(pool, &checkpoint).await?;
        save(pool, &stale).await?;
        let retained = query(pool, detailed, ReadScope::All).await?;
        assert_eq!(retained.entries, [settled.entries, bob.entries].concat());
        Ok(())
    }
}
