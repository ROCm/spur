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
    unused_by_cause JSONB NOT NULL DEFAULT '{}',
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
CREATE TABLE IF NOT EXISTS preemption_cause_rollups (
    user_name TEXT NOT NULL,
    account TEXT NOT NULL,
    reason TEXT NOT NULL,
    initiator TEXT NOT NULL,
    count NUMERIC(40,0) NOT NULL CHECK (count >= 0),
    PRIMARY KEY (user_name, account, reason, initiator)
);

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
    let revision = checkpoint.usage.revision();
    if revision == 0 {
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
    let old_revision = PreemptionUsage::revision_of(used, unused, open);
    let old_causes = old
        .try_get::<sqlx::types::Json<CauseCounts>, _>("unused_by_cause")?
        .0;
    if old_revision > revision
        || (old_revision == revision && old_causes == checkpoint.usage.unused)
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
        let mut cursor = None;
        loop {
            interval.tick().await;
            if !raft.ensure_leader().await {
                continue;
            }
            if let Err(error) = sync_once(&pool, &cluster, &mut cursor).await {
                tracing::warn!(%error, "preemption accounting export failed; retaining unexported jobs");
            }
        }
    });
}

fn connection_failed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .is_some_and(|error| match error {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed => true,
            sqlx::Error::Database(error) => error.code().is_some_and(|code| {
                code.starts_with("08") || matches!(code.as_ref(), "57P01" | "57P02" | "57P03")
            }),
            _ => false,
        })
}

pub(crate) async fn sync_once(
    pool: &PgPool,
    cluster: &ClusterManager,
    cursor: &mut Option<JobId>,
) -> anyhow::Result<()> {
    let batch = cluster.pending_preemption_checkpoints(1000, *cursor);
    let last_batch = batch.len() < 1000;
    for checkpoint in batch {
        let result = tokio::time::timeout(super::notifier::ATTEMPT_TIMEOUT, async {
            let mut tx = pool.begin().await?;
            write(&mut tx, &checkpoint).await?;
            tx.commit().await?;
            anyhow::Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => cluster.acknowledge_preemption_checkpoint(&checkpoint),
            Ok(Err(error)) if connection_failed(&error) => return Err(error),
            Ok(Err(error)) => tracing::warn!(job_id = checkpoint.job_id, %error,
                "preemption checkpoint failed; retaining this job and continuing the batch"),
            // Step past a row stuck behind a lock, but stop: the database may be stalled.
            Err(elapsed) => {
                *cursor = Some(checkpoint.job_id);
                return Err(elapsed.into());
            }
        }
        *cursor = Some(checkpoint.job_id);
    }
    if last_batch {
        *cursor = None;
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
    if matches!(scope, ReadScope::Caller(user) | ReadScope::Coordinated { user, .. } if user.is_empty())
    {
        return Ok(GetPreemptionUsageResponse {
            scoped_to_user: Some(String::new()),
            ..Default::default()
        });
    }
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
            let unused_by_cause = row
                .try_get::<sqlx::types::Json<Vec<Cause>>, _>("unused_by_cause")?
                .0
                .into_iter()
                .map(|cause| {
                    Ok(UnusedPreemptionCount {
                        reason: cause.reason,
                        initiator: cause.initiator,
                        count: cause.count.parse()?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(PreemptionUsageEntry {
                user: row.try_get("user_name")?,
                account: row.try_get("account")?,
                used: row.try_get::<String, _>("used")?.parse()?,
                unused: row.try_get::<String, _>("unused")?.parse()?,
                open: row.try_get::<String, _>("open")?.parse()?,
                unused_by_cause,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let next_page_token = match entries.last().filter(|_| more) {
        Some(last) => serde_json::to_string(&(&last.user, &last.account))?,
        None => String::new(),
    };
    let partial = match scope {
        ReadScope::Coordinated { user, accounts } => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM preemption_rollups WHERE user_name <> $1 AND NOT (account = ANY($2)))"
        ).bind(user).bind(accounts).fetch_one(pool).await?,
        _ => false,
    };
    Ok(GetPreemptionUsageResponse {
        entries,
        next_page_token,
        scoped_to_user: match scope {
            ReadScope::Caller(user) => Some(user.clone()),
            _ => None,
        },
        partial,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::preemption_usage::{PreemptionInitiator, UnusedReason};

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
            exercise_causes(&pool).await?;
            exercise_failed_checkpoints(&pool).await?;
            exercise_timed_out_checkpoint(&pool).await
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
                for entry in &reply.entries {
                    let expected_causes = if entry.user == "alice" { 1 } else { 0 };
                    assert_eq!(entry.unused_by_cause.len(), expected_causes);
                }
            }
        }

        sqlx::query("DELETE FROM jobs").execute(pool).await?;
        save(pool, &open).await?;
        save(pool, &alice).await?;
        let retained = query(pool, Default::default(), ReadScope::All).await?;
        assert_eq!(retained.entries.len(), 3);
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

    async fn exercise_failed_checkpoints(pool: &PgPool) -> anyhow::Result<()> {
        use crate::raft::StateMachineApply;
        sqlx::query(
            "TRUNCATE preemption_checkpoints, preemption_rollups, preemption_cause_rollups, jobs",
        )
        .execute(pool)
        .await?;
        let jobs: Vec<_> = (1..=1001)
            .map(|id| {
                let mut job = Job::new(
                    id,
                    spur_core::job::JobSpec {
                        user: "alice".into(),
                        ..Default::default()
                    },
                );
                job.preemption_usage.used = 1;
                job
            })
            .collect();
        let dir = tempfile::tempdir()?;
        let cluster = ClusterManager::new(
            serde_json::from_str(r#"{"cluster_name":"test"}"#)?,
            dir.path(),
        )?;
        cluster.restore_from_snapshot(&serde_json::to_vec(&serde_json::json!({
            "jobs": jobs, "nodes": [], "reservations": [], "steps": [], "license_pool": {}
        }))?)?;
        sqlx::query("INSERT INTO preemption_checkpoints (job_id, incarnation, user_name, account, unused_by_cause) SELECT id, 'legacy', 'alice', '', '{\"future_reason\": {\"system\": 1}}'::jsonb FROM generate_series(1, 1000) id").execute(pool).await?;
        let mut cursor = None;
        sync_once(pool, &cluster, &mut cursor).await?;
        assert_eq!(cursor, Some(1000));
        sync_once(pool, &cluster, &mut cursor).await?;
        assert_eq!(cursor, None);
        assert_eq!(
            cluster.pending_preemption_checkpoints(2000, None).len(),
            1000
        );
        let report = query(pool, Default::default(), ReadScope::All).await?;
        assert_eq!(report.entries[0].used, 1);
        sqlx::query("UPDATE preemption_checkpoints SET unused_by_cause='{}'")
            .execute(pool)
            .await?;
        sync_once(pool, &cluster, &mut cursor).await?;
        assert!(cluster
            .pending_preemption_checkpoints(2000, None)
            .is_empty());
        assert_eq!(
            query(pool, Default::default(), ReadScope::All)
                .await?
                .entries[0]
                .used,
            1001
        );

        for value in [
            serde_json::json!({"unused":{"future_reason":{"system":1}}}),
            serde_json::json!(42),
        ] {
            sqlx::query("INSERT INTO jobs (job_id, user_name, preemption_usage) VALUES (1,'alice',$1) ON CONFLICT (job_id) DO UPDATE SET preemption_usage=EXCLUDED.preemption_usage")
                .bind(value).execute(pool).await?;
            let states = super::super::db::job_accounting_states(pool, &[1]).await?;
            assert_eq!(states[&1].preemption_usage, PreemptionUsage::default());
            let history = super::super::db::get_job_history(pool, &Default::default()).await?;
            assert_eq!(history[0].preemption_usage, PreemptionUsage::default());
        }
        sqlx::query(
            "INSERT INTO preemption_rollups (user_name, account, used) VALUES ('', 'unnamed', 7)",
        )
        .execute(pool)
        .await?;
        for scope in [
            ReadScope::Caller(String::new()),
            ReadScope::Coordinated {
                user: String::new(),
                accounts: vec!["unnamed".into()],
            },
        ] {
            let hidden = query(
                pool,
                GetPreemptionUsageRequest {
                    by_cause: true,
                    ..Default::default()
                },
                scope,
            )
            .await?;
            assert!(hidden.entries.is_empty());
            assert!(hidden.next_page_token.is_empty());
        }
        sqlx::query("DELETE FROM preemption_rollups WHERE user_name=''")
            .execute(pool)
            .await?;
        let complete = query(
            pool,
            Default::default(),
            ReadScope::Coordinated {
                user: "outsider".into(),
                accounts: vec!["".into()],
            },
        )
        .await?;
        assert!(!complete.partial);
        Ok(())
    }

    async fn exercise_timed_out_checkpoint(pool: &PgPool) -> anyhow::Result<()> {
        use crate::raft::StateMachineApply;
        sqlx::query(
            "TRUNCATE preemption_checkpoints, preemption_rollups, preemption_cause_rollups",
        )
        .execute(pool)
        .await?;
        let jobs: Vec<_> = [1, 2]
            .map(|id| {
                let mut job = Job::new(id, Default::default());
                job.preemption_usage.used = 1;
                job
            })
            .into();
        let dir = tempfile::tempdir()?;
        let cluster = ClusterManager::new(
            serde_json::from_str(r#"{"cluster_name":"test"}"#)?,
            dir.path(),
        )?;
        cluster.restore_from_snapshot(&serde_json::to_vec(&serde_json::json!({
            "jobs": jobs, "nodes": [], "reservations": [], "steps": [], "license_pool": {}
        }))?)?;
        sqlx::query("INSERT INTO preemption_checkpoints (job_id, incarnation, user_name, account) VALUES (1, 'legacy', '', '')")
            .execute(pool)
            .await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT 1 FROM preemption_checkpoints WHERE job_id = 1 FOR UPDATE")
            .execute(&mut *blocker)
            .await?;
        let mut cursor = None;
        tokio::time::pause();
        let stalled = sync_once(pool, &cluster, &mut cursor).await;
        tokio::time::resume();
        assert!(stalled.is_err());
        assert_eq!(cursor, Some(1));
        sync_once(pool, &cluster, &mut cursor).await?;
        let pending: Vec<_> = cluster
            .pending_preemption_checkpoints(10, None)
            .into_iter()
            .map(|checkpoint| checkpoint.job_id)
            .collect();
        assert_eq!(pending, vec![1]);
        blocker.rollback().await?;
        Ok(())
    }

    fn assert_causes(entry: &PreemptionUsageEntry, expected: &[(&str, &str, u64)]) {
        let actual: Vec<_> = entry
            .unused_by_cause
            .iter()
            .map(|cause| (cause.reason.as_str(), cause.initiator.as_str(), cause.count))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            entry.unused,
            actual.iter().map(|cause| cause.2).sum::<u64>()
        );
    }

    async fn exercise_causes(pool: &PgPool) -> anyhow::Result<()> {
        sqlx::query(
            "TRUNCATE preemption_checkpoints, preemption_rollups, preemption_cause_rollups",
        )
        .execute(pool)
        .await?;
        let detailed = GetPreemptionUsageRequest {
            by_cause: true,
            ..Default::default()
        };
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
        assert!(summary.entries.iter().all(|e| e.unused_by_cause.is_empty()));
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
