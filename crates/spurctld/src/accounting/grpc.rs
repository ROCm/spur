// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tonic::{Request, Response, Status};

use spur_core::accounting::{limit_to_wire, TresRecord};
use spur_proto::proto::slurm_accounting_server::{SlurmAccounting, SlurmAccountingServer};
use spur_proto::proto::*;

use super::{db, txn};
use crate::audit::Annotation;

fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

/// `allowed` of `None` is the unrestricted view.
fn account_allowed(allowed: Option<&[String]>, name: &str) -> bool {
    match allowed {
        None => true,
        Some(names) => names.iter().any(|n| n == name),
    }
}

fn scoped_fairshare(
    scope: &spur_core::rbac::ReadScope,
    entries: impl IntoIterator<Item = FairshareEntry>,
) -> Vec<FairshareEntry> {
    entries
        .into_iter()
        .filter(|entry| scope.permits_row(&entry.user, &entry.account))
        .collect()
}

/// Tells the client its reply was pinned to one user, so it does not present
/// the remaining rows as account or cluster totals. A coordinator's subset
/// leaves this unset and sets `partial` instead.
fn scoped_to_user(scope: &spur_core::rbac::ReadScope) -> Option<String> {
    match scope {
        spur_core::rbac::ReadScope::All | spur_core::rbac::ReadScope::Coordinated { .. } => None,
        spur_core::rbac::ReadScope::Caller(user) => Some(user.clone()),
    }
}

/// Accounts a partial usage reply covers, including those with no rows in the
/// window. Empty when the reply is the whole cluster or a single user.
fn usage_visible_accounts(scope: &spur_core::rbac::ReadScope, partial: bool) -> Vec<String> {
    if !partial {
        return Vec::new();
    }
    let Some(accounts) = scope.coordinated_accounts() else {
        return Vec::new();
    };
    let mut names = accounts.to_vec();
    names.sort();
    names.dedup();
    names
}

/// The user predicate a private read hands to SQL.
#[derive(Debug)]
enum ScopedUser {
    /// The request names someone this caller cannot see.
    None,
    /// `None` is every user. A coordinator is not one SQL user, so the handler
    /// fetches this set and keeps the rows `permits_row` allows.
    Query(Option<String>),
}

fn coordinated_filter(scope: &spur_core::rbac::ReadScope) -> Option<(&str, &[String])> {
    match scope {
        spur_core::rbac::ReadScope::Coordinated { user, accounts } => {
            Some((user.as_str(), accounts.as_slice()))
        }
        _ => None,
    }
}

fn is_foreign_key_violation(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::Database(db)) => db.code().as_deref() == Some("23503"),
        _ => false,
    }
}

fn scoped_sql_user(scope: &spur_core::rbac::ReadScope, requested: &str) -> ScopedUser {
    let requested = (!requested.is_empty()).then(|| requested.to_owned());
    match scope {
        spur_core::rbac::ReadScope::Coordinated { .. } => ScopedUser::Query(requested),
        _ => match scope.job_user_filter(requested.as_deref()) {
            Some(user) => ScopedUser::Query(user),
            None => ScopedUser::None,
        },
    }
}

/// On the caller's transaction, so the row commits with the change or not at
/// all. A failure fails the mutation, rather than applying it unaudited.
async fn write_txn_in_band(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    audit: &crate::audit::InBand,
    annotation: Annotation,
) -> Result<(), Status> {
    let Some(record) = audit.row(annotation) else {
        return Ok(());
    };
    db::record_txn(tx, &record).await.map_err(internal)
}

/// Reject a TRES string (e.g. `grptres=`/`maxtresperjob=`/`maxtresperuser=`)
/// that doesn't parse, instead of letting it silently become a no-op limit.
fn validate_tres(field: &str, raw: &str) -> Result<(), Status> {
    if raw.is_empty() {
        return Ok(());
    }
    TresRecord::parse(raw)
        .map(|_| ())
        .map_err(|e| Status::invalid_argument(format!("invalid {field}: {e}")))
}

/// Canonicalize an `adminlevel` for storage, rejecting anything that is not a level. The column is
/// free text, so an unrecognised value would otherwise store and display as if it were a privilege.
fn normalize_admin_level(raw: &str) -> Result<&str, Status> {
    if raw.is_empty() {
        return Ok(raw);
    }
    super::canonical_admin_level(raw).ok_or_else(|| {
        Status::invalid_argument(format!(
            "invalid adminlevel '{raw}': expected None, Operator, or Admin \
             (also spelled Administrator or SuperUser)"
        ))
    })
}

/// Map an optional TRES/text proto field to a nullable-column patch: unset ->
/// keep, empty -> clear (SQL NULL), otherwise -> set.
fn nullable_str(field: &Option<String>) -> Option<Option<&str>> {
    field
        .as_deref()
        .map(|s| if s.is_empty() { None } else { Some(s) })
}

/// Map an optional u32 limit proto field to a nullable-int patch: unset ->
/// keep, INFINITE (`u32::MAX`) -> clear (no limit / SQL NULL), n -> set the
/// literal (including 0, which means "block all"). Errors if `n` overflows i32.
fn nullable_limit(field: Option<u32>, what: &str) -> Result<Option<Option<i32>>, Status> {
    match field {
        None => Ok(None),
        Some(spur_core::accounting::INFINITE) => Ok(Some(None)),
        Some(n) => Ok(Some(Some(i32::try_from(n).map_err(|_| {
            Status::invalid_argument(format!("{what} exceeds i32::MAX"))
        })?))),
    }
}

/// Map the `grace_time` proto fields to a column patch: `clear` -> NULL
/// (inherit the partition's), unset -> keep, n -> store n (0 means no grace).
/// A value together with a clear is ambiguous, so it is refused; so is an `n`
/// that overflows i32.
fn grace_time_patch(field: Option<u32>, clear: bool) -> Result<Option<Option<i32>>, Status> {
    if clear {
        if field.is_some() {
            return Err(Status::invalid_argument(
                "grace_time and clear_grace_time cannot both be set",
            ));
        }
        return Ok(Some(None));
    }
    field
        .map(|n| {
            i32::try_from(n)
                .map(Some)
                .map_err(|_| Status::invalid_argument("grace_time exceeds i32::MAX"))
        })
        .transpose()
}

/// What the audit record shows for a grace patch: the stored seconds, or JSON
/// null when the value was cleared, so a clear is recorded like any other change.
fn grace_time_audit(patch: Option<Option<i32>>) -> Option<serde_json::Value> {
    patch.map(|stored| stored.map_or(serde_json::Value::Null, Into::into))
}

fn qos_info_from_record(r: db::QosRecord) -> QosInfo {
    QosInfo {
        idle_fill_preemptable: r.idle_fill_preemptable,
        name: r.name,
        description: r.description,
        priority: r.priority,
        preempt_mode: r.preempt_mode,
        preempt: r.preempt,
        usage_factor: r.usage_factor,
        max_jobs_per_user: limit_to_wire(r.max_jobs_per_user),
        max_wall_minutes: limit_to_wire(r.max_wall_min),
        max_tres_per_job: r.max_tres_per_job.unwrap_or_default(),
        max_submit_jobs_per_user: limit_to_wire(r.max_submit_per_user),
        max_tres_per_user: r.max_tres_per_user.unwrap_or_default(),
        grp_tres: r.grp_tres.unwrap_or_default(),
        grp_wall_minutes: limit_to_wire(r.grp_wall_min),
        preempt_exempt_time: r.preempt_exempt_time.map(|v| v as u32),
        max_submit_jobs_per_account: limit_to_wire(r.max_submit_per_account),
        grp_submit_jobs: limit_to_wire(r.grp_submit_jobs),
        flags: r.flags,
        grace_time: r.grace_time.and_then(|v| u32::try_from(v).ok()),
    }
}

/// Validate and canonicalize the QOS `flags` string. Only `DenyOnLimit` is
/// recognized; an unknown token errors loudly rather than being silently
/// dropped (a dropped flag reads as "set" but never takes effect).
fn canonicalize_qos_flags(raw: &str) -> Result<String, Status> {
    let mut deny_on_limit = false;
    for token in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if token.eq_ignore_ascii_case("denyonlimit") {
            deny_on_limit = true;
        } else {
            return Err(Status::invalid_argument(format!(
                "unknown QOS flag '{token}'. Supported: DenyOnLimit"
            )));
        }
    }
    Ok(if deny_on_limit { "DenyOnLimit" } else { "" }.to_string())
}

/// Fairshare is a whole share count stored as `INTEGER`, but the proto carries
/// it as `double` (kept for wire stability). Reject a fractional, negative, or
/// out-of-range value rather than silently truncating it (e.g. 2.9 -> 2).
fn fairshare_to_i32(v: f64) -> Result<i32, Status> {
    if !v.is_finite() || v.fract() != 0.0 || v < 0.0 || v > f64::from(i32::MAX) {
        return Err(Status::invalid_argument(format!(
            "fairshare must be a whole number in [0, {}], got {v}",
            i32::MAX
        )));
    }
    Ok(v as i32)
}

/// The accounting gRPC service. The pool is held behind a shared, swappable
/// slot so a controller that boots while the database is unreachable can
/// install the pool later, once the background bring-up connects — the same
/// handle keeps serving `Unavailable` until then. Cloning shares that slot, so
/// the copy handed to the gRPC server and the copy the bring-up keeps see each
/// other's writes.
pub(crate) struct AccountingService {
    inner: std::sync::Arc<AccountingInner>,
}

struct AccountingInner {
    pool: parking_lot::RwLock<Option<PgPool>>,
    reason: parking_lot::RwLock<&'static str>,
    assoc: parking_lot::RwLock<Option<std::sync::Arc<crate::association_cache::AssociationCache>>>,
    /// Live `[auth]`. `reconfigure` swaps the cluster config this reads.
    auth_source: parking_lot::RwLock<
        Option<std::sync::Arc<dyn Fn() -> spur_core::config::AuthConfig + Send + Sync>>,
    >,
    fairshare_halflife_days: std::sync::atomic::AtomicU32,
    billing_weights: std::sync::Arc<parking_lot::RwLock<spur_core::config::BillingWeights>>,
}

impl Clone for AccountingService {
    fn clone(&self) -> Self {
        Self {
            inner: std::sync::Arc::clone(&self.inner),
        }
    }
}

impl AccountingService {
    pub(crate) fn unavailable(reason: &'static str) -> Self {
        Self {
            inner: std::sync::Arc::new(AccountingInner {
                pool: parking_lot::RwLock::new(None),
                reason: parking_lot::RwLock::new(reason),
                assoc: parking_lot::RwLock::new(None),
                auth_source: parking_lot::RwLock::new(None),
                fairshare_halflife_days: std::sync::atomic::AtomicU32::new(0),
                billing_weights: std::sync::Arc::new(parking_lot::RwLock::new(
                    spur_core::config::BillingWeights::default(),
                )),
            }),
        }
    }

    pub(crate) fn attach_fairshare_halflife(&self, days: u32) {
        self.inner
            .fairshare_halflife_days
            .store(days, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn attach_billing_weights(&self, weights: spur_core::config::BillingWeights) {
        *self.inner.billing_weights.write() = weights;
    }

    fn billing_weights(&self) -> spur_core::config::BillingWeights {
        *self.inner.billing_weights.read()
    }

    pub(crate) fn billing_weights_shared(
        &self,
    ) -> std::sync::Arc<parking_lot::RwLock<spur_core::config::BillingWeights>> {
        std::sync::Arc::clone(&self.inner.billing_weights)
    }

    fn configured_halflife_days(&self) -> u32 {
        self.inner
            .fairshare_halflife_days
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Install the pool once the background bring-up connects, flipping every
    /// clone of this service from `Unavailable` to serving live queries.
    pub(crate) fn install_pool(&self, pool: PgPool) {
        *self.inner.pool.write() = Some(pool);
    }

    /// Record why the service is still unavailable, so a client hitting it
    /// during the connect-retry window sees the latest cause rather than a
    /// stale one.
    pub(crate) fn mark_unavailable(&self, reason: &'static str) {
        *self.inner.reason.write() = reason;
    }

    pub(crate) fn attach_association_cache(
        &self,
        cache: std::sync::Arc<crate::association_cache::AssociationCache>,
    ) {
        *self.inner.assoc.write() = Some(cache);
    }

    /// Read `[auth]` from the running cluster so `scontrol reconfigure` applies
    /// without a restart. With no source, `[auth]` is the default.
    pub(crate) fn attach_auth_source(
        &self,
        source: impl Fn() -> spur_core::config::AuthConfig + Send + Sync + 'static,
    ) {
        *self.inner.auth_source.write() = Some(std::sync::Arc::new(source));
    }

    fn current_auth(&self) -> spur_core::config::AuthConfig {
        self.inner
            .auth_source
            .read()
            .as_ref()
            .map(|source| source())
            .unwrap_or_default()
    }

    /// One `[auth]` read and one association-cache lookup for this request.
    fn auth_role<T>(
        &self,
        request: &Request<T>,
    ) -> (spur_core::config::AuthConfig, Option<spur_core::rbac::Role>) {
        let auth = self.current_auth();
        let role = request
            .extensions()
            .get::<spur_core::auth::Identity>()
            .map(|id| self.resolved_role(id, &auth));
        (auth, role)
    }

    fn caller_read_scope<T>(
        &self,
        request: &Request<T>,
        private: impl Fn(&spur_core::config::AuthConfig) -> bool,
    ) -> spur_core::rbac::ReadScope {
        self.caller_read_scope_with(request, private, false)
    }

    /// `widen` adds the accounts this caller coordinates. The live queue does
    /// not, so `squeue` stays the caller's own jobs.
    fn caller_read_scope_with<T>(
        &self,
        request: &Request<T>,
        private: impl Fn(&spur_core::config::AuthConfig) -> bool,
        widen: bool,
    ) -> spur_core::rbac::ReadScope {
        let (auth, role) = self.auth_role(request);
        self.scope_from(request, role, private(&auth), widen)
    }

    fn scope_from<T>(
        &self,
        request: &Request<T>,
        role: Option<spur_core::rbac::Role>,
        private: bool,
        widen: bool,
    ) -> spur_core::rbac::ReadScope {
        let identity = request.extensions().get::<spur_core::auth::Identity>();
        let scope = spur_core::rbac::read_scope(identity, role, private);
        if !widen {
            return scope;
        }
        let spur_core::rbac::ReadScope::Caller(user) = &scope else {
            return scope;
        };
        match self.coordinated_loaded(user) {
            Some(accounts) => scope.widen_to(accounts),
            None => scope,
        }
    }

    /// `None` when no cache is installed or it has not loaded.
    fn coordinated_loaded(&self, user: &str) -> Option<Vec<String>> {
        self.inner.assoc.read().as_ref()?.coordinated_accounts(user)
    }

    async fn directory_partial(
        &self,
        scope: &spur_core::rbac::ReadScope,
        pool: &PgPool,
    ) -> Result<bool, Status> {
        let Some(mine) = scope.coordinated_accounts() else {
            return Ok(false);
        };
        let all = db::list_accounts(pool).await.map_err(internal)?;
        Ok(all
            .iter()
            .any(|account| !mine.iter().any(|c| c == &account.name)))
    }

    fn resolved_role(
        &self,
        id: &spur_core::auth::Identity,
        auth: &spur_core::config::AuthConfig,
    ) -> spur_core::rbac::Role {
        let cache = self.inner.assoc.read().clone();
        let (level, loaded) = match cache.as_deref() {
            Some(cache) => (cache.admin_level(&id.user), cache.is_loaded()),
            None => (None, false),
        };
        spur_core::rbac::resolve_caller_role(id, auth, level.as_deref(), loaded)
    }

    /// Anonymous is allowed, matching the controller's gate. An identified
    /// caller below Operator is denied.
    fn require_admin<T>(&self, request: &Request<T>, op: &str) -> Result<(), Status> {
        if self.may_see_actors(request) {
            return Ok(());
        }
        spur_core::native_metrics::inc_role_deny();
        Err(Status::permission_denied(format!(
            "{op} requires cluster operator or administrator"
        )))
    }

    /// Whether this caller may see who acted on a job, the same audience as
    /// `get_transactions`. No deny metric: this only trims what a read returns.
    fn may_see_actors<T>(&self, request: &Request<T>) -> bool {
        self.auth_role(request)
            .1
            .is_none_or(spur_core::rbac::Role::operates_jobs)
    }

    fn kick_assoc(&self) {
        if let Some(cache) = self.inner.assoc.read().as_ref() {
            cache.kick_refresh();
        }
    }

    fn pool(&self) -> Result<PgPool, Status> {
        match self.inner.pool.read().as_ref() {
            Some(pool) => Ok(pool.clone()),
            None => {
                let reason = *self.inner.reason.read();
                Err(Status::unavailable(format!(
                    "accounting service is not available ({reason})"
                )))
            }
        }
    }
}

/// Build a ready-to-register tonic service for embedding in another server.
pub(crate) fn accounting_server(
    service: AccountingService,
) -> SlurmAccountingServer<AccountingService> {
    spur_proto::accounting_server(service)
}

#[tonic::async_trait]
impl SlurmAccounting for AccountingService {
    async fn record_job_start(
        &self,
        request: Request<RecordJobStartRequest>,
    ) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let start_time = req
            .start_time
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or_else(Utc::now);
        let submit_time = req
            .submit_time
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or(start_time);

        let (memory_mb, cpus, total_gpus) = req
            .resources
            .as_ref()
            .map(|r| {
                let gpus: u32 = r
                    .devices
                    .values()
                    .map(|d| d.devices.iter().map(|a| a.count as u32).sum::<u32>())
                    .sum();
                (r.memory_mb, r.cpus, gpus)
            })
            .unwrap_or((0, 1, 0));

        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        db::record_job_start(
            &mut conn,
            &db::JobStartRecord {
                job_id: req.job_id,
                name: req.name,
                user: req.user,
                account: req.account,
                partition: req.partition,
                qos: req.qos,
                num_nodes: 1, // simplified
                num_tasks: cpus,
                cpus_per_task: 1,
                total_gpus,
                memory_mb,
                submit_time,
                start_time,
                reservation: Some(req.reservation),
                // RecordJobStartRequest carries no node list.
                nodelist: String::new(),
                // The external accounting RPC carries no idle-fill notion; a run
                // recorded through it is treated as an ordinary one.
                idle_fill: false,
                time_limit_secs: None,
            },
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
    }

    async fn record_job_end(
        &self,
        request: Request<RecordJobEndRequest>,
    ) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let end_time = req
            .end_time
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or_else(Utc::now);

        let state_str = match req.final_state {
            3 => "COMPLETED",
            4 => "FAILED",
            5 => "CANCELLED",
            6 => "TIMEOUT",
            7 => "NODE_FAIL",
            10 => "DEADLINE",
            _ => "UNKNOWN",
        };

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        db::record_job_end(
            &mut tx,
            &db::JobEndRecord {
                job_id: req.job_id,
                state: state_str.to_string(),
                exit_code: req.exit_code,
                end_time,
                exit_signal: req.exit_signal,
                derived_exit_code: req.derived_exit_code,
                preempted_by: None,
                preempt_mode: String::new(),
                preempt_qos: String::new(),
                reason: None,
                reason_detail: String::new(),
                ended_by: String::new(),
                ended_by_uid: None,
                submission: None,
            },
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
    }

    async fn get_job_history(
        &self,
        request: Request<GetJobHistoryRequest>,
    ) -> Result<Response<GetJobHistoryResponse>, Status> {
        let (auth, role) = self.auth_role(&request);
        let show_actors = role.is_none_or(spur_core::rbac::Role::operates_jobs);
        let scope = self.scope_from(&request, role, auth.jobs_private(), true);
        let req = request.into_inner();
        let sql_user = match scoped_sql_user(&scope, &req.user) {
            ScopedUser::None => {
                return Ok(Response::new(GetJobHistoryResponse { jobs: Vec::new() }));
            }
            ScopedUser::Query(user) => user,
        };
        let user = sql_user.as_deref();

        let pool = self.pool()?;
        let pool = &pool;

        let start_after = timestamp_to_utc(req.start_after)?;
        let start_before = timestamp_to_utc(req.start_before)?;

        let states: Vec<String> = req
            .states
            .iter()
            .filter_map(|s| proto_job_state_to_db_str(*s).map(String::from))
            .collect();
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };

        let mut records = db::get_job_history(
            pool,
            &db::JobHistoryQuery {
                user,
                account,
                start_after,
                start_before,
                states: &states,
                job_ids: &req.job_ids,
                limit: req.limit,
                coordinated: coordinated_filter(&scope),
            },
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        if matches!(scope, spur_core::rbac::ReadScope::Coordinated { .. }) {
            records.retain(|r| {
                scope.permits_row(&r.user_name, &r.account) && user.is_none_or(|u| r.user_name == u)
            });
        }

        let jobs = records
            .iter()
            .map(|r| JobInfo {
                fabric_bindings: Default::default(),
                gpu_bindings: Default::default(),
                job_id: r.job_id,
                name: r.name.clone(),
                user: r.user_name.clone(),
                uid: 0,
                partition: r.partition.clone(),
                account: r.account.clone(),
                state: db_job_state_str_to_proto(&r.state),
                state_reason: String::new(),
                submit_time: Some(datetime_to_proto(r.submit_time)),
                start_time: r.start_time.map(datetime_to_proto),
                end_time: r.end_time.map(datetime_to_proto),
                time_limit: r.time_limit_secs.map(|s| prost_types::Duration {
                    seconds: s,
                    nanos: 0,
                }),
                run_time: match (r.start_time, r.end_time) {
                    (Some(s), Some(e)) => Some(prost_types::Duration {
                        seconds: (e - s).num_seconds(),
                        nanos: 0,
                    }),
                    _ => None,
                },
                num_nodes: r.num_nodes as u32,
                num_tasks: r.num_tasks as u32,
                cpus_per_task: 1,
                nodelist: r.nodelist.clone(),
                work_dir: String::new(),
                command: String::new(),
                exit_code: r.exit_code,
                exit_signal: r.exit_signal,
                derived_exit_code: r.derived_exit_code,
                stdout_path: String::new(),
                stderr_path: String::new(),
                stdin_path: String::new(),
                resources: None,
                priority: 0,
                qos: String::new(),
                array_job_id: 0,
                array_task_id: 0,
                reservation: r.reservation.clone(),
                comment: String::new(),
                srun_step_dispatch: false,
                req_gpus: r.total_gpus as u32,
                req_gpus_detail: String::new(),
                preempted_by: r.preempted_by.unwrap_or(0),
                preempt_mode: r.preempt_mode.clone(),
                preempt_qos: r.preempt_qos.clone(),
                idle_fill: r.idle_fill,
                end_reason: r.reason.clone(),
                end_reason_detail: r.reason_detail.clone(),
                ended_by: if show_actors {
                    r.ended_by.clone()
                } else {
                    String::new()
                },
                ended_by_uid: r.ended_by_uid.filter(|_| show_actors),
                // Live-only: a finished row has no current suspend or requeue.
                suspended_by: String::new(),
                last_requeue: String::new(),
                last_requeue_by: String::new(),
                preempt_time: None,
                // Kept exhaustive so a new JobInfo field forces a decision here;
                // the accounting store has no requested-placement columns.
                req_nodelist: String::new(),
                exc_nodelist: String::new(),
                features: String::new(),
                dependency: Vec::new(),
                submit_line: String::new(),
                req_tres: String::new(),
                min_cpus_node: 0,
                min_memory_node_mb: 0,
                min_memory_is_per_cpu: false,
                eligible_time: None,
                accrue_time: None,
                last_sched_eval: None,
                deadline: None,
                time_min: None,
                requeue: false,
                restarts: 0,
                batch_flag: false,
                exclusive: false,
                planned_start_time: None,
                sched_nodelist: String::new(),
            })
            .collect();

        Ok(Response::new(GetJobHistoryResponse { jobs }))
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let job_scoped = request.get_ref().job_scoped;
        let scope = self.caller_read_scope_with(
            &request,
            |auth| {
                if job_scoped {
                    auth.jobs_private()
                } else {
                    auth.usage_private()
                }
            },
            true,
        );
        let req = request.into_inner();
        let scoped_to = scoped_to_user(&scope);
        let sql_user = match scoped_sql_user(&scope, &req.user) {
            ScopedUser::None => {
                let w = self.billing_weights();
                return Ok(Response::new(GetUsageResponse {
                    entries: Vec::new(),
                    billing_weight_cpu: w.cpu,
                    billing_weight_gpu: w.gpu,
                    has_billing_weights: true,
                    scoped_to_user: scoped_to,
                    partial: false,
                    visible_accounts: usage_visible_accounts(&scope, false),
                }));
            }
            ScopedUser::Query(user) => user,
        };
        let user = sql_user.as_deref();
        let pool = self.pool()?;
        let pool = &pool;

        let since = req
            .since
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };

        let records = db::get_usage_visible(pool, user, account, since, coordinated_filter(&scope))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let partial = self.directory_partial(&scope, pool).await?;

        let mut agg: std::collections::HashMap<(String, String), (f64, f64, u64)> =
            std::collections::HashMap::new();
        for r in &records {
            if !scope.permits_row(&r.user_name, &r.account) {
                continue;
            }
            if let Some(want) = user {
                if r.user_name != want {
                    continue;
                }
            }
            let e = agg
                .entry((r.user_name.clone(), r.account.clone()))
                .or_default();
            e.0 += r.cpu_seconds as f64;
            e.1 += r.gpu_seconds as f64;
            e.2 += r.job_count;
        }

        let entries = agg
            .into_iter()
            .map(|((user, account), (cpu, gpu, jobs))| UsageEntry {
                user,
                account,
                cpu_seconds: cpu,
                gpu_seconds: gpu,
                job_count: jobs,
            })
            .collect();

        let w = self.billing_weights();
        Ok(Response::new(GetUsageResponse {
            entries,
            billing_weight_cpu: w.cpu,
            billing_weight_gpu: w.gpu,
            has_billing_weights: true,
            scoped_to_user: scoped_to,
            partial,
            visible_accounts: usage_visible_accounts(&scope, partial),
        }))
    }

    // ============================================================
    // Account management
    // ============================================================

    async fn create_account(
        &self,
        request: Request<CreateAccountRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "create account")?;
        let audit = crate::audit::InBand::capture(&request, "CreateAccount");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        if let Some(g) = &req.grp_tres {
            validate_tres("grptres", g)?;
        }
        let update = db::AccountUpdate {
            description: req.description.as_deref(),
            organization: req.organization.as_deref(),
            parent: nullable_str(&req.parent_account),
            fairshare: req.fairshare_weight.map(fairshare_to_i32).transpose()?,
            max_running_jobs: nullable_limit(req.max_running_jobs, "max_running_jobs")?,
            grp_tres: nullable_str(&req.grp_tres),
        };
        let mut tx = pool.begin().await.map_err(internal)?;
        let result = db::upsert_account(&mut tx, &req.name, update)
            .await
            .map_err(internal)?;
        let details = txn::requested(&[
            ("description", req.description.map(Into::into)),
            ("organization", req.organization.map(Into::into)),
            ("parent_account", req.parent_account.map(Into::into)),
            ("fairshare_weight", req.fairshare_weight.map(Into::into)),
            ("max_running_jobs", req.max_running_jobs.map(Into::into)),
            ("grp_tres", req.grp_tres.map(Into::into)),
        ]);
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(&req.name, txn::with_changes(details, result.changed))
                .action(result.outcome.action()),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();

        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn delete_account(
        &self,
        request: Request<DeleteAccountRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "delete account")?;
        let audit = crate::audit::InBand::capture(&request, "DeleteAccount");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let mut tx = pool.begin().await.map_err(internal)?;
        db::delete_account(&mut tx, &req.name)
            .await
            .map_err(internal)?;
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(&req.name, txn::delete_details(None)),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn list_accounts(
        &self,
        request: Request<ListAccountsRequest>,
    ) -> Result<Response<ListAccountsResponse>, Status> {
        // The share tree is not an account listing. Slurm's sshare reads it
        // even when PrivateData=accounts would deny `sacctmgr show account`.
        if request.get_ref().shares {
            let pool = self.pool()?;
            let records = db::list_accounts(&pool)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            let accounts = records
                .into_iter()
                .map(|r| AccountInfo {
                    name: r.name,
                    parent_account: r.parent.unwrap_or_default(),
                    fairshare_weight: r.fairshare_weight as f64,
                    ..Default::default()
                })
                .collect();
            return Ok(Response::new(ListAccountsResponse {
                accounts,
                partial: false,
            }));
        }
        let scope =
            self.caller_read_scope(&request, spur_core::config::AuthConfig::accounts_private);
        // Operators and an unrestricted caller see every account. Everyone else
        // sees only accounts they coordinate. Membership is not enough: Slurm
        // denies a user who coordinates nothing.
        let allowed = match &scope {
            spur_core::rbac::ReadScope::All => None,
            spur_core::rbac::ReadScope::Caller(user)
            | spur_core::rbac::ReadScope::Coordinated { user, .. } => {
                let Some(accounts) = self.coordinated_loaded(user) else {
                    return Err(Status::unavailable("association cache is still loading"));
                };
                if accounts.is_empty() {
                    return Err(Status::permission_denied(
                        "account definitions are visible to their coordinators",
                    ));
                }
                Some(accounts)
            }
        };
        let pool = self.pool()?;
        let pool = &pool;
        let records = db::list_accounts(pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let partial = allowed
            .as_ref()
            .is_some_and(|names| records.iter().any(|r| !names.iter().any(|n| n == &r.name)));

        let accounts = records
            .into_iter()
            .filter(|r| account_allowed(allowed.as_deref(), &r.name))
            .map(|r| AccountInfo {
                name: r.name,
                description: r.description,
                organization: r.organization,
                parent_account: r.parent.unwrap_or_default(),
                fairshare_weight: r.fairshare_weight as f64,
                max_running_jobs: limit_to_wire(r.max_running_jobs),
                grp_tres: r.grp_tres.unwrap_or_default(),
            })
            .collect();

        Ok(Response::new(ListAccountsResponse { accounts, partial }))
    }

    async fn add_user(&self, request: Request<AddUserRequest>) -> Result<Response<()>, Status> {
        self.require_admin(&request, "add user")?;
        let audit = crate::audit::InBand::capture(&request, "AddUser");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let admin_level = req
            .admin_level
            .as_deref()
            .map(normalize_admin_level)
            .transpose()?;
        // QOS references are validated against the live DB, not QosCache: a QOS
        // created just now may not have reached the cache's next refresh yet,
        // the same lag job submission already accepts for QosCache reads. Each
        // check runs only for a field the request actually restated.
        if let Some(dq) = req.default_qos.as_deref() {
            if !dq.is_empty() {
                let exists = db::qos_exists(pool, dq)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
                if !exists {
                    return Err(Status::not_found(format!("QOS '{dq}' does not exist")));
                }
            }
        }
        // Normalize + validate an explicitly restated allow-list. `None` leaves
        // the stored allow-list untouched; an explicit empty clears it.
        let allowed_qos_normalized: Option<String> = match req.allowed_qos.as_deref() {
            None => None,
            Some(list) => {
                let names: Vec<&str> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                let missing = db::missing_qos(pool, &names)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
                if let Some(name) = missing.first() {
                    return Err(Status::not_found(format!("QOS '{name}' does not exist")));
                }
                Some(names.join(","))
            }
        };
        // Cross-check the default is in the allow-list only when both are
        // restated here; a preserved (unset) side can't be validated without a
        // read and is left to the stored value.
        if let (Some(dq), Some(list)) = (
            req.default_qos.as_deref(),
            allowed_qos_normalized.as_deref(),
        ) {
            if !dq.is_empty() && !list.is_empty() && !list.split(',').any(|q| q == dq) {
                return Err(Status::invalid_argument(format!(
                    "default QOS '{dq}' must be included in qos={list}"
                )));
            }
        }
        if let Some(t) = &req.max_tres_per_job {
            validate_tres("maxtresperjob", t)?;
        }
        if let Some(t) = &req.grp_tres {
            validate_tres("grptres", t)?;
        }
        let update = db::UserUpdate {
            admin_level,
            is_default: req.is_default,
            default_qos: nullable_str(&req.default_qos),
            allowed_qos: allowed_qos_normalized.as_deref().map(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }),
            max_running_jobs: nullable_limit(req.max_running_jobs, "max_running_jobs")?,
            max_submit_jobs: nullable_limit(req.max_submit_jobs, "max_submit_jobs")?,
            grp_submit_jobs: nullable_limit(req.grp_submit_jobs, "grp_submit_jobs")?,
            max_tres_per_job: nullable_str(&req.max_tres_per_job),
            grp_tres: nullable_str(&req.grp_tres),
            max_wall_min: nullable_limit(req.max_wall_minutes, "max_wall_minutes")?,
        };
        let mut tx = pool.begin().await.map_err(internal)?;
        let outcome = db::add_user(&mut tx, &req.user, &req.account, update)
            .await
            .map_err(internal)?;
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(
                &req.user,
                txn::requested(&[
                    ("account", Some(req.account.clone().into())),
                    ("admin_level", admin_level.map(Into::into)),
                    ("is_default", req.is_default.map(Into::into)),
                    ("default_qos", req.default_qos.map(Into::into)),
                    ("allowed_qos", allowed_qos_normalized.map(Into::into)),
                    ("max_running_jobs", req.max_running_jobs.map(Into::into)),
                    ("max_wall_minutes", req.max_wall_minutes.map(Into::into)),
                ]),
            )
            .action(outcome.action()),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn remove_user(
        &self,
        request: Request<RemoveUserRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "remove user")?;
        let audit = crate::audit::InBand::capture(&request, "RemoveUser");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let mut tx = pool.begin().await.map_err(internal)?;
        let deleted = db::remove_user(&mut tx, &req.user, &req.account)
            .await
            .map_err(internal)?;
        if deleted == 0 {
            // Dropping the transaction rolls back; nothing happened, so the
            // layer records the NOT_FOUND attempt rather than this path.
            let target = if req.account.is_empty() {
                format!("user '{}'", req.user)
            } else {
                format!("user '{}' in account '{}'", req.user, req.account)
            };
            return Err(Status::not_found(format!("{target} does not exist")));
        }
        // An empty account removes the user from every one of them, so record
        // the scope only when the request narrowed it.
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(
                &req.user,
                txn::requested(&[(
                    "account",
                    (!req.account.is_empty()).then(|| req.account.clone().into()),
                )]),
            ),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn modify_usage(
        &self,
        request: Request<ModifyUsageRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "modify usage")?;
        let audit = crate::audit::InBand::capture(&request, "ModifyUsage");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        if req.user.is_empty() && req.account.is_empty() {
            return Err(Status::invalid_argument("user or account required"));
        }
        if !req.user.is_empty() && req.account.is_empty() {
            return Err(Status::invalid_argument(
                "account= required for user-level usage modification",
            ));
        }
        if req.user.is_empty() && req.raw_usage != 0 {
            return Err(Status::invalid_argument(
                "account-level usage adjustment only supports reset to zero",
            ));
        }
        if !req.user.is_empty() {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM associations WHERE user_name = $1 AND account = $2)",
            )
            .bind(&req.user)
            .bind(&req.account)
            .fetch_one(pool)
            .await
            .map_err(internal)?;
            if !exists {
                return Err(Status::not_found(format!(
                    "no association for user '{}' in account '{}'",
                    req.user, req.account
                )));
            }
        }
        let halflife_days = self.configured_halflife_days();
        let weights = self.billing_weights();
        let mut tx = pool.begin().await.map_err(internal)?;

        let (prev, entity_name) = if req.user.is_empty() {
            let descendants = db::get_account_descendants(pool, &req.account)
                .await
                .map_err(internal)?;
            let mut total_prev = 0i64;
            for acct in &descendants {
                let p = db::modify_usage(&mut tx, "", acct, 0, halflife_days, &weights)
                    .await
                    .map_err(internal)?;
                total_prev += p;
            }
            (total_prev, req.account.clone())
        } else {
            let p = db::modify_usage(
                &mut tx,
                &req.user,
                &req.account,
                req.raw_usage,
                halflife_days,
                &weights,
            )
            .await
            .map_err(internal)?;
            let name = format!("{}@{}", req.user, req.account);
            (p, name)
        };

        let new_total = if req.raw_usage == 0 {
            0
        } else if req.raw_usage < 0 {
            (prev + req.raw_usage).max(0)
        } else {
            req.raw_usage
        };
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(
                &entity_name,
                serde_json::json!({
                    "previous_usage": prev,
                    "requested_value": req.raw_usage,
                    "resulting_usage": new_total,
                }),
            ),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        Ok(Response::new(()))
    }

    async fn list_users(
        &self,
        request: Request<ListUsersRequest>,
    ) -> Result<Response<ListUsersResponse>, Status> {
        let associations = request.get_ref().associations;
        let scope = self.caller_read_scope_with(
            &request,
            spur_core::config::AuthConfig::users_private,
            associations,
        );
        let pinned = scoped_to_user(&scope);
        let req = request.into_inner();
        let sql_user = match scoped_sql_user(&scope, &req.user) {
            ScopedUser::None => {
                return Ok(Response::new(ListUsersResponse {
                    users: Vec::new(),
                    scoped_to_user: pinned,
                    partial: false,
                }));
            }
            ScopedUser::Query(user) => user,
        };
        let user = sql_user.as_deref();
        let pool = self.pool()?;
        let pool = &pool;
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };
        let mut records = db::list_users_visible(pool, account, user, coordinated_filter(&scope))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        if matches!(scope, spur_core::rbac::ReadScope::Coordinated { .. }) {
            records.retain(|r| {
                scope.permits_row(&r.name, &r.account) && user.is_none_or(|u| r.name == u)
            });
        }
        let partial = self.directory_partial(&scope, pool).await?;

        let users = records
            .into_iter()
            .map(|r| UserInfo {
                name: r.name,
                account: r.account,
                admin_level: r.admin_level,
                default_account: r.default_account.unwrap_or_default(),
                default_qos: r.default_qos.unwrap_or_default(),
                allowed_qos: r.allowed_qos.unwrap_or_default(),
                max_running_jobs: limit_to_wire(r.max_running_jobs),
                max_submit_jobs: limit_to_wire(r.max_submit_jobs),
                grp_submit_jobs: limit_to_wire(r.grp_submit_jobs),
                max_wall_minutes: limit_to_wire(r.max_wall_min),
                max_tres_per_job: r.max_tres_per_job.unwrap_or_default(),
                grp_tres: r.grp_tres.unwrap_or_default(),
                partition: r.partition.unwrap_or_default(),
                // Present when the association carries a weight; absent (None)
                // only when the LEFT JOIN found no association row. A real
                // stored weight — including 0 — is sent as-is, not defaulted.
                fairshare: r.fairshare_weight.map(|w| w.max(0) as u32),
            })
            .collect();

        Ok(Response::new(ListUsersResponse {
            users,
            scoped_to_user: pinned,
            partial,
        }))
    }

    async fn add_coordinator(
        &self,
        request: Request<AddCoordinatorRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "add coordinator")?;
        let audit = crate::audit::InBand::capture(&request, "AddCoordinator");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        if req.account.is_empty() || req.users.is_empty() {
            return Err(Status::invalid_argument("account and users are required"));
        }
        let mut tx = pool.begin().await.map_err(internal)?;
        // A typo would sit unused until a later user of that name inherited it.
        for user in &req.users {
            if !db::user_exists(&mut tx, user).await.map_err(internal)? {
                return Err(Status::not_found(format!("user '{user}' does not exist")));
            }
        }
        let mut added = 0;
        for user in &req.users {
            added += db::add_coordinator(&mut tx, user, &req.account)
                .await
                .map_err(|e| {
                    if is_foreign_key_violation(&e) {
                        Status::not_found(format!("account '{}' does not exist", req.account))
                    } else {
                        internal(e)
                    }
                })?;
        }
        if added == 0 {
            return Err(Status::already_exists(format!(
                "coordinator assignment already exists for account '{}'",
                req.account
            )));
        }
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(
                &req.account,
                txn::requested(&[("users", Some(serde_json::json!(req.users)))]),
            ),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn remove_coordinator(
        &self,
        request: Request<RemoveCoordinatorRequest>,
    ) -> Result<Response<()>, Status> {
        self.require_admin(&request, "remove coordinator")?;
        let audit = crate::audit::InBand::capture(&request, "RemoveCoordinator");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        if req.account.is_empty() || req.users.is_empty() {
            return Err(Status::invalid_argument("account and users are required"));
        }
        let mut tx = pool.begin().await.map_err(internal)?;
        let mut deleted = 0;
        for user in &req.users {
            deleted += db::remove_coordinator(&mut tx, user, &req.account)
                .await
                .map_err(internal)?;
        }
        if deleted == 0 {
            return Err(Status::not_found(format!(
                "no coordinator matched account '{}'",
                req.account
            )));
        }
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(&req.account, txn::delete_details(None)),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn list_coordinators(
        &self,
        request: Request<ListCoordinatorsRequest>,
    ) -> Result<Response<ListCoordinatorsResponse>, Status> {
        let scope =
            self.caller_read_scope(&request, spur_core::config::AuthConfig::accounts_private);
        let allowed = match &scope {
            spur_core::rbac::ReadScope::All => None,
            spur_core::rbac::ReadScope::Caller(user)
            | spur_core::rbac::ReadScope::Coordinated { user, .. } => {
                let Some(accounts) = self.coordinated_loaded(user) else {
                    return Err(Status::unavailable("association cache is still loading"));
                };
                if accounts.is_empty() {
                    return Err(Status::permission_denied(
                        "account definitions are visible to their coordinators",
                    ));
                }
                Some(accounts)
            }
        };
        let req = request.into_inner();
        let pool = self.pool()?;
        let rows = db::list_coordinators(&pool).await.map_err(internal)?;
        let coordinators = rows
            .into_iter()
            .filter(|(user, account)| {
                allowed
                    .as_ref()
                    .is_none_or(|names| names.iter().any(|n| n == account))
                    && (req.account.is_empty() || req.account == *account)
                    && (req.user.is_empty() || req.user == *user)
            })
            .map(|(user, account)| CoordinatorInfo { user, account })
            .collect();
        Ok(Response::new(ListCoordinatorsResponse { coordinators }))
    }

    async fn create_qos(&self, request: Request<CreateQosRequest>) -> Result<Response<()>, Status> {
        self.require_admin(&request, "create qos")?;
        let audit = crate::audit::InBand::capture(&request, "CreateQos");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        if let Some(t) = &req.max_tres_per_job {
            validate_tres("maxtresperjob", t)?;
        }
        if let Some(t) = &req.max_tres_per_user {
            validate_tres("maxtresperuser", t)?;
        }
        if let Some(t) = &req.grp_tres {
            validate_tres("grptres", t)?;
        }
        // Validate that every QOS name in the preempt allow-list exists before
        // writing — a typo here becomes silent dead config that could retroactively
        // grant preempt rights if a QOS with that name is created later.
        let preempt_normalized: Option<String> = match req.preempt.as_deref() {
            None | Some("") => req.preempt.clone(),
            Some(list) => {
                let names: Vec<&str> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                let missing = db::missing_qos(pool, &names)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
                if let Some(name) = missing.first() {
                    return Err(Status::not_found(format!(
                        "QOS '{name}' does not exist (in preempt allow-list)"
                    )));
                }
                Some(names.join(","))
            }
        };

        let flags = req
            .flags
            .as_deref()
            .map(canonicalize_qos_flags)
            .transpose()?;
        // Canonicalize so a typo is rejected here rather than silently reading
        // back as "no override" once the cache reloads.
        let preempt_mode = req
            .preempt_mode
            .as_deref()
            .map(|mode| {
                spur_core::accounting::parse_qos_preempt_mode(mode)
                    .map(spur_core::accounting::qos_preempt_mode_str)
                    .map_err(Status::invalid_argument)
            })
            .transpose()?;
        let grace_time = grace_time_patch(req.grace_time, req.clear_grace_time)?;
        let update = db::QosUpdate {
            idle_fill_preemptable: req.idle_fill_preemptable,
            description: req.description.as_deref(),
            priority: req.priority,
            preempt_mode,
            preempt: preempt_normalized.as_deref(),
            usage_factor: req.usage_factor,
            max_jobs_per_user: nullable_limit(req.max_jobs_per_user, "max_jobs_per_user")?,
            max_wall_min: nullable_limit(req.max_wall_minutes, "max_wall_minutes")?,
            max_tres_per_job: nullable_str(&req.max_tres_per_job),
            max_submit_per_user: nullable_limit(
                req.max_submit_jobs_per_user,
                "max_submit_jobs_per_user",
            )?,
            max_submit_per_account: nullable_limit(
                req.max_submit_jobs_per_account,
                "max_submit_jobs_per_account",
            )?,
            grp_submit_jobs: nullable_limit(req.grp_submit_jobs, "grp_submit_jobs")?,
            max_tres_per_user: nullable_str(&req.max_tres_per_user),
            grp_tres: nullable_str(&req.grp_tres),
            grp_wall_min: nullable_limit(req.grp_wall_minutes, "grp_wall_minutes")?,
            preempt_exempt_time: if req.clear_preempt_exempt_time {
                Some(None)
            } else {
                req.preempt_exempt_time.map(|v| Some(v as i32))
            },
            grace_time,
            flags: flags.as_deref(),
        };
        let mut tx = pool.begin().await.map_err(internal)?;
        let result = db::upsert_qos(&mut tx, &req.name, update)
            .await
            .map_err(internal)?;
        let details = txn::requested(&[
            ("description", req.description.map(Into::into)),
            ("priority", req.priority.map(Into::into)),
            ("preempt_mode", req.preempt_mode.map(Into::into)),
            ("preempt", preempt_normalized.map(Into::into)),
            ("usage_factor", req.usage_factor.map(Into::into)),
            ("max_jobs_per_user", req.max_jobs_per_user.map(Into::into)),
            ("max_wall_minutes", req.max_wall_minutes.map(Into::into)),
            ("grp_wall_minutes", req.grp_wall_minutes.map(Into::into)),
            ("max_tres_per_job", req.max_tres_per_job.map(Into::into)),
            ("grp_tres", req.grp_tres.map(Into::into)),
            ("grace_time", grace_time_audit(grace_time)),
            ("flags", flags.map(Into::into)),
        ]);
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(&req.name, txn::with_changes(details, result.changed))
                .action(result.outcome.action()),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn delete_qos(&self, request: Request<DeleteQosRequest>) -> Result<Response<()>, Status> {
        self.require_admin(&request, "delete qos")?;
        let audit = crate::audit::InBand::capture(&request, "DeleteQos");
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();
        let mut tx = pool.begin().await.map_err(internal)?;
        db::delete_qos(&mut tx, &req.name).await.map_err(internal)?;
        write_txn_in_band(
            &mut tx,
            &audit,
            Annotation::new(&req.name, txn::delete_details(None)),
        )
        .await?;
        tx.commit().await.map_err(internal)?;
        audit.mark_written();
        self.kick_assoc();
        Ok(Response::new(()))
    }

    async fn list_qos(
        &self,
        _request: Request<ListQosRequest>,
    ) -> Result<Response<ListQosResponse>, Status> {
        let pool = self.pool()?;
        let pool = &pool;
        let records = db::list_qos(pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let qos_list = records.into_iter().map(qos_info_from_record).collect();

        Ok(Response::new(ListQosResponse { qos_list }))
    }

    async fn get_fairshare_factors(
        &self,
        request: Request<GetFairshareFactorsRequest>,
    ) -> Result<Response<GetFairshareFactorsResponse>, Status> {
        let job_scoped = request.get_ref().job_scoped;
        // `sprio` asks for the jobs rule and does not widen: its job list is
        // the caller's own jobs. `sshare` widens to coordinated accounts.
        let scope = self.caller_read_scope_with(
            &request,
            |auth| {
                if job_scoped {
                    auth.jobs_private()
                } else {
                    auth.usage_private()
                }
            },
            !job_scoped,
        );
        let req = request.into_inner();

        // When the client sends 0, use the cluster's configured half-life
        // so sshare/sprio agree with the scheduler.
        let halflife = if req.halflife_days == 0 {
            self.configured_halflife_days()
        } else {
            req.halflife_days
        };
        // `fairshare_factors` normalises its input (0→14, clamp 1–365).
        let effective = if halflife == 0 {
            14
        } else {
            halflife.clamp(1, 365)
        };
        // An identified caller with no name must not fall through to every row.
        if matches!(&scope, spur_core::rbac::ReadScope::Caller(user) if user.is_empty()) {
            return Ok(Response::new(GetFairshareFactorsResponse {
                entries: Vec::new(),
                effective_halflife_days: effective,
                scoped_to_user: scoped_to_user(&scope),
                partial: false,
            }));
        }

        let pool = self.pool()?;
        let weights = self.billing_weights();
        let raw_factors = super::fairshare_factors(&pool, halflife, &weights)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let entries = scoped_fairshare(
            &scope,
            raw_factors
                .into_iter()
                .map(|((user, account), factor)| FairshareEntry {
                    user,
                    account,
                    factor,
                }),
        );

        let partial = self.directory_partial(&scope, &pool).await?;
        Ok(Response::new(GetFairshareFactorsResponse {
            entries,
            effective_halflife_days: effective,
            scoped_to_user: scoped_to_user(&scope),
            partial,
        }))
    }

    async fn get_transactions(
        &self,
        request: Request<GetTransactionsRequest>,
    ) -> Result<Response<GetTransactionsResponse>, Status> {
        // Gated above the rest of this service: the log carries every user's
        // actions and peer addresses, so it is not ordinary accounting data.
        self.require_admin(&request, "show transactions")?;
        let pool = self.pool()?;
        let pool = &pool;
        let req = request.into_inner();

        let start_after = timestamp_to_utc(req.start_after)?;
        let start_before = timestamp_to_utc(req.start_before)?;

        let filter = db::TxnFilter {
            actor: (!req.actor.is_empty()).then_some(req.actor.as_str()),
            entity_type: (!req.entity_type.is_empty()).then_some(req.entity_type.as_str()),
            entity_name: (!req.entity_name.is_empty()).then_some(req.entity_name.as_str()),
            action: (!req.action.is_empty()).then_some(req.action.as_str()),
            outcome: (!req.outcome.is_empty()).then_some(req.outcome.as_str()),
            peer_addr: (!req.peer_addr.is_empty()).then_some(req.peer_addr.as_str()),
            start_after,
            start_before,
            limit: req.limit,
        };

        let rows = db::get_transactions(pool, &filter)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let transactions = rows
            .into_iter()
            .map(|r| TransactionRecord {
                id: r.id,
                timestamp: Some(datetime_to_proto(r.ts)),
                actor: r.actor,
                actor_uid: r.actor_uid.and_then(|u| u32::try_from(u).ok()),
                verified: r.verified,
                source: r.source,
                action: r.action,
                entity_type: r.entity_type,
                entity_name: r.entity_name,
                outcome: r.outcome,
                details: r.details,
                peer_addr: r.peer_addr,
            })
            .collect();

        Ok(Response::new(GetTransactionsResponse { transactions }))
    }
}

fn proto_job_state_to_db_str(code: i32) -> Option<&'static str> {
    match code {
        0 => Some("PENDING"),
        1 => Some("RUNNING"),
        3 => Some("COMPLETED"),
        4 => Some("FAILED"),
        5 => Some("CANCELLED"),
        6 => Some("TIMEOUT"),
        7 => Some("NODE_FAIL"),
        8 => Some("PREEMPTED"),
        10 => Some("DEADLINE"),
        11 => Some("OUT_OF_MEMORY"),
        12 => Some("REQUEUED"),
        _ => None,
    }
}

fn db_job_state_str_to_proto(state: &str) -> i32 {
    match state {
        "COMPLETED" => JobState::JobCompleted as i32,
        "FAILED" => JobState::JobFailed as i32,
        "CANCELLED" => JobState::JobCancelled as i32,
        "TIMEOUT" => JobState::JobTimeout as i32,
        "NODE_FAIL" => JobState::JobNodeFail as i32,
        "PREEMPTED" => JobState::JobPreempted as i32,
        "DEADLINE" => JobState::JobDeadline as i32,
        "OUT_OF_MEMORY" => JobState::JobOutOfMemory as i32,
        "REQUEUED" => JobState::JobRequeued as i32,
        "RUNNING" => JobState::JobRunning as i32,
        "PENDING" => JobState::JobPending as i32,
        _ => JobState::JobCompleted as i32,
    }
}

/// Convert an optional proto timestamp to UTC, rejecting a present-but-invalid
/// value with `invalid_argument` instead of silently coercing it to the epoch
/// (which would widen a query window). Guards the nanos cast against negatives.
fn timestamp_to_utc(ts: Option<prost_types::Timestamp>) -> Result<Option<DateTime<Utc>>, Status> {
    let Some(t) = ts else { return Ok(None) };
    let nanos = u32::try_from(t.nanos)
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .ok_or_else(|| Status::invalid_argument(format!("invalid timestamp nanos: {}", t.nanos)))?;
    DateTime::from_timestamp(t.seconds, nanos)
        .map(Some)
        .ok_or_else(|| {
            Status::invalid_argument(format!("timestamp out of range (seconds={})", t.seconds))
        })
}

fn datetime_to_proto(dt: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_fail_round_trips_through_the_history_filter_and_display_mapping() {
        assert_eq!(
            proto_job_state_to_db_str(JobState::JobNodeFail as i32),
            Some("NODE_FAIL")
        );
        assert_eq!(
            db_job_state_str_to_proto("NODE_FAIL"),
            JobState::JobNodeFail as i32
        );
    }

    #[test]
    fn running_and_pending_states_are_preserved_in_the_history_filter() {
        // A dropped code empties the states filter, which the query then
        // treats as "match everything" instead of narrowing it.
        assert_eq!(
            proto_job_state_to_db_str(JobState::JobRunning as i32),
            Some("RUNNING")
        );
        assert_eq!(
            proto_job_state_to_db_str(JobState::JobPending as i32),
            Some("PENDING")
        );
    }

    #[test]
    fn every_finalized_proto_state_maps_to_its_own_db_string_and_back() {
        let finalized_states = [
            (JobState::JobCompleted, "COMPLETED"),
            (JobState::JobFailed, "FAILED"),
            (JobState::JobCancelled, "CANCELLED"),
            (JobState::JobTimeout, "TIMEOUT"),
            (JobState::JobNodeFail, "NODE_FAIL"),
            (JobState::JobPreempted, "PREEMPTED"),
            (JobState::JobDeadline, "DEADLINE"),
            (JobState::JobOutOfMemory, "OUT_OF_MEMORY"),
            (JobState::JobRequeued, "REQUEUED"),
        ];
        for (proto_state, db_str) in finalized_states {
            assert_eq!(
                proto_job_state_to_db_str(proto_state as i32),
                Some(db_str),
                "states filter dropped {db_str}"
            );
            assert_eq!(
                db_job_state_str_to_proto(db_str),
                proto_state as i32,
                "display mapping mis-mapped {db_str}"
            );
        }
    }

    fn assert_startup_unavailable<T>(result: Result<Response<T>, Status>) {
        let status = match result {
            Ok(_) => panic!("accounting RPC unexpectedly succeeded"),
            Err(status) => status,
        };
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(
            status.message(),
            "accounting service is not available (database connection failed at startup)"
        );
    }

    #[tokio::test]
    async fn unavailable_service_rejects_every_accounting_rpc() {
        let service = AccountingService::unavailable("database connection failed at startup");

        macro_rules! assert_rpc_unavailable {
            ($method:ident, $request:ty) => {
                assert_startup_unavailable(
                    service.$method(Request::new(<$request>::default())).await,
                );
            };
        }

        assert_rpc_unavailable!(record_job_start, RecordJobStartRequest);
        assert_rpc_unavailable!(record_job_end, RecordJobEndRequest);
        assert_rpc_unavailable!(get_job_history, GetJobHistoryRequest);
        assert_rpc_unavailable!(get_usage, GetUsageRequest);
        assert_rpc_unavailable!(create_account, CreateAccountRequest);
        assert_rpc_unavailable!(delete_account, DeleteAccountRequest);
        assert_rpc_unavailable!(list_accounts, ListAccountsRequest);
        assert_rpc_unavailable!(add_user, AddUserRequest);
        assert_rpc_unavailable!(remove_user, RemoveUserRequest);
        assert_rpc_unavailable!(list_users, ListUsersRequest);
        assert_rpc_unavailable!(create_qos, CreateQosRequest);
        assert_rpc_unavailable!(delete_qos, DeleteQosRequest);
        assert_rpc_unavailable!(list_qos, ListQosRequest);
        assert_rpc_unavailable!(get_fairshare_factors, GetFairshareFactorsRequest);
        assert_rpc_unavailable!(get_transactions, GetTransactionsRequest);
    }

    #[tokio::test]
    async fn unavailable_accounting_server_returns_unavailable_over_grpc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(accounting_server(AccountingService::unavailable(
                    "database connection failed at startup",
                )))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = spur_proto::accounting_client(channel);
        let status = client
            .list_accounts(ListAccountsRequest::default())
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(
            status.message(),
            "accounting service is not available (database connection failed at startup)"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unavailable_service_reports_migration_failure() {
        let service = AccountingService::unavailable("database migration failed at startup");
        let status = service
            .list_accounts(Request::new(ListAccountsRequest::default()))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(
            status.message(),
            "accounting service is not available (database migration failed at startup)"
        );
    }

    // The core of the cold-start recovery: a service that boots unavailable must
    // start serving once the background bring-up installs the pool, and every
    // clone must see that install through the shared slot (the gRPC server holds
    // one clone, the bring-up task another). `connect_lazy` yields a real
    // `PgPool` without touching a database, so this exercises the actual
    // install/read path rather than a stand-in.
    #[tokio::test]
    async fn install_pool_recovers_a_cloned_unavailable_service() {
        let service = AccountingService::unavailable("connecting to accounting database");
        let server_copy = service.clone();
        assert!(
            server_copy.pool().is_err(),
            "must report unavailable before the pool is installed"
        );

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://spur:spur@127.0.0.1:5433/does-not-connect")
            .expect("lazy pool builds without connecting");
        service.install_pool(pool);

        assert!(
            server_copy.pool().is_ok(),
            "the server's clone must observe the pool the bring-up installed"
        );
    }

    #[test]
    fn mark_unavailable_updates_the_reported_reason() {
        let service = AccountingService::unavailable("connecting to accounting database");
        service.mark_unavailable("database migration failed");
        let status = service.pool().unwrap_err();
        assert_eq!(
            status.message(),
            "accounting service is not available (database migration failed)"
        );
    }

    fn identity(user: &str, is_admin: bool) -> spur_core::auth::Identity {
        spur_core::auth::Identity {
            user: user.into(),
            uid: 1000,
            gid: 1000,
            is_admin,
            trusted_unix: false,
        }
    }

    /// The account/user/QOS mutations are admin-only for an identified caller: a verified admin
    /// passes, a verified non-admin is rejected — this closes the self-promotion path where
    /// `add_user(admin_level = Admin)` had no authorization. An unauthenticated caller is allowed,
    /// matching the auth model: `disabled`/`permissive`-with-no-credential trust the client, so the
    /// gate binds real users only in `required` mode.
    #[test]
    fn require_admin_gates_accounting_mutations() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let mut admin = Request::new(AddUserRequest::default());
        admin.extensions_mut().insert(identity("root", true));
        assert!(service.require_admin(&admin, "add user").is_ok());

        let mut non_admin = Request::new(AddUserRequest::default());
        non_admin
            .extensions_mut()
            .insert(identity("mallory", false));
        let err = service
            .require_admin(&non_admin, "add user")
            .expect_err("non-admin must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let anon = Request::new(AddUserRequest::default());
        assert!(service.require_admin(&anon, "add user").is_ok());
    }

    #[test]
    fn require_admin_follows_a_reconfigure_that_drops_the_operator() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let still_admin = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = still_admin.clone();
        service.attach_auth_source(move || {
            let mut auth = spur_core::config::AuthConfig::default();
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                auth.cluster_admins.push("op".into());
            }
            auth
        });

        let mut add = Request::new(AddUserRequest::default());
        add.extensions_mut().insert(identity("op", false));
        assert!(service.require_admin(&add, "add user").is_ok());

        let mut history = Request::new(GetJobHistoryRequest::default());
        history.extensions_mut().insert(identity("op", false));
        assert!(service.may_see_actors(&history));

        still_admin.store(false, std::sync::atomic::Ordering::Relaxed);
        let err = service
            .require_admin(&add, "add user")
            .expect_err("a reconfigure that drops the admin must deny the mutation");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(!service.may_see_actors(&history));
    }

    #[test]
    fn only_the_audit_log_audience_sees_who_ended_a_job() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let mut admin = Request::new(GetJobHistoryRequest::default());
        admin.extensions_mut().insert(identity("root", true));
        assert!(service.may_see_actors(&admin));

        let mut user = Request::new(GetJobHistoryRequest::default());
        user.extensions_mut().insert(identity("mallory", false));
        assert!(!service.may_see_actors(&user));

        assert!(service.may_see_actors(&Request::new(GetJobHistoryRequest::default())));
    }

    #[test]
    fn cluster_admins_and_accounting_operator_pass_without_jwt_admin_claim() {
        let service = AccountingService::unavailable("no DB needed for this test");
        fix_auth(
            &service,
            spur_core::config::AuthConfig {
                cluster_admins: vec!["erin".into()],
                ..Default::default()
            },
        );
        let mut named = Request::new(AddUserRequest::default());
        named.extensions_mut().insert(identity("erin", false));
        assert!(service.require_admin(&named, "add user").is_ok());

        let cache = std::sync::Arc::new(crate::association_cache::AssociationCache::new());
        cache.insert_admin_level("bob", "Operator");
        service.attach_association_cache(cache);
        let mut op = Request::new(AddUserRequest::default());
        op.extensions_mut().insert(identity("bob", false));
        assert!(service.require_admin(&op, "add user").is_ok());
    }

    /// Table test over every gated accounting RPC, enumerated so a handler that drops its
    /// `require_admin` call is caught even though it never reaches the DB in this test: with the
    /// pool unavailable, a non-admin's rejection can only come from the gate, not a query failure.
    #[tokio::test]
    async fn accounting_mutations_deny_identified_non_admin() {
        let service = AccountingService::unavailable("no DB needed for this test");

        macro_rules! assert_admin_gated {
            ($method:ident, $req:expr) => {{
                let mut r = Request::new($req);
                r.extensions_mut().insert(identity("mallory", false));
                let err = service.$method(r).await.expect_err(concat!(
                    stringify!($method),
                    " must reject a non-admin caller"
                ));
                assert_eq!(
                    err.code(),
                    tonic::Code::PermissionDenied,
                    concat!(stringify!($method), " (non-admin) must be PermissionDenied")
                );
            }};
        }

        assert_admin_gated!(create_account, CreateAccountRequest::default());
        assert_admin_gated!(delete_account, DeleteAccountRequest::default());
        assert_admin_gated!(add_user, AddUserRequest::default());
        assert_admin_gated!(remove_user, RemoveUserRequest::default());
        assert_admin_gated!(create_qos, CreateQosRequest::default());
        assert_admin_gated!(delete_qos, DeleteQosRequest::default());
        // A read, but gated with the mutations: it exposes every user's actions.
        assert_admin_gated!(get_transactions, GetTransactionsRequest::default());
    }

    /// The audit read must not over-block. Reaching the query is what proves the
    /// caller passed the gate: with no pool it can only fail `Unavailable`.
    #[tokio::test]
    async fn transactions_read_admits_operator_and_anonymous() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let cache = std::sync::Arc::new(crate::association_cache::AssociationCache::new());
        cache.insert_admin_level("bob", "Operator");
        service.attach_association_cache(cache);

        let mut operator = Request::new(GetTransactionsRequest::default());
        operator.extensions_mut().insert(identity("bob", false));

        let anonymous = Request::new(GetTransactionsRequest::default());

        for (who, request) in [("operator", operator), ("anonymous", anonymous)] {
            let err = service
                .get_transactions(request)
                .await
                .expect_err("no pool is installed");
            assert_eq!(
                err.code(),
                tonic::Code::Unavailable,
                "{who} must reach the query rather than be denied"
            );
        }
    }

    fn fix_auth(service: &AccountingService, auth: spur_core::config::AuthConfig) {
        service.attach_auth_source(move || auth.clone());
    }

    fn auth_hiding(categories: &[&str]) -> spur_core::config::AuthConfig {
        let mut auth = spur_core::config::AuthConfig::default();
        for category in categories {
            auth.private_data.insert((*category).to_string().into());
        }
        auth
    }

    #[test]
    fn usage_visible_accounts_names_a_coordinators_accounts_only_when_partial() {
        let scope = spur_core::rbac::ReadScope::Coordinated {
            user: "alice".into(),
            accounts: vec!["physics".into(), "ml".into()],
        };
        assert!(usage_visible_accounts(&scope, false).is_empty());
        assert_eq!(
            usage_visible_accounts(&scope, true),
            vec!["ml".to_string(), "physics".to_string()]
        );
        assert!(
            usage_visible_accounts(&spur_core::rbac::ReadScope::Caller("alice".into()), true)
                .is_empty()
        );
    }

    #[test]
    fn scoped_sql_user_pins_the_caller_and_skips_someone_else() {
        let scope = spur_core::rbac::ReadScope::Caller("alice".into());
        assert!(matches!(scoped_sql_user(&scope, "bob"), ScopedUser::None));
        match scoped_sql_user(&scope, "") {
            ScopedUser::Query(Some(user)) => assert_eq!(user, "alice"),
            other => panic!("own history must query the caller, got {other:?}"),
        }
    }

    #[test]
    fn scoped_sql_user_keeps_a_coordinators_requested_user() {
        let scope = spur_core::rbac::ReadScope::Coordinated {
            user: "alice".into(),
            accounts: vec!["physics".into()],
        };
        assert!(matches!(
            scoped_sql_user(&scope, ""),
            ScopedUser::Query(None)
        ));
        match scoped_sql_user(&scope, "bob") {
            ScopedUser::Query(Some(user)) => assert_eq!(user, "bob"),
            other => panic!("a coordinator query keeps the requested user, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_users_pins_an_ordinary_caller_when_users_are_private() {
        let service = AccountingService::unavailable("no DB needed for this test");

        let mut asking = Request::new(ListUsersRequest {
            user: "bob".into(),
            account: String::new(),
            associations: false,
        });
        asking.extensions_mut().insert(identity("alice", false));
        let err = service
            .list_users(asking)
            .await
            .expect_err("without private_data the query still runs");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        fix_auth(&service, auth_hiding(&["users"]));

        let mut other = Request::new(ListUsersRequest {
            user: "bob".into(),
            account: String::new(),
            associations: false,
        });
        other.extensions_mut().insert(identity("alice", false));
        let listed = service.list_users(other).await.unwrap().into_inner();
        assert!(listed.users.is_empty());
        assert_eq!(listed.scoped_to_user.as_deref(), Some("alice"));

        let mut own = Request::new(ListUsersRequest::default());
        own.extensions_mut().insert(identity("alice", false));
        let err = service
            .list_users(own)
            .await
            .expect_err("the caller's own rows still come from the database");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        let cache = std::sync::Arc::new(crate::association_cache::AssociationCache::new());
        cache.insert_admin_level("op", "Operator");
        service.attach_association_cache(cache);
        for (user, admin) in [("op", false), ("carol", true)] {
            let mut req = Request::new(ListUsersRequest {
                user: "bob".into(),
                account: String::new(),
                associations: false,
            });
            req.extensions_mut().insert(identity(user, admin));
            let err = service.list_users(req).await.expect_err("exempt caller");
            assert_eq!(err.code(), tonic::Code::Unavailable, "{user}");
        }
        let err = service
            .list_users(Request::new(ListUsersRequest::default()))
            .await
            .expect_err("anonymous");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn list_users_follows_a_reconfigure_of_private_data() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let users_private = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = users_private.clone();
        service.attach_auth_source(move || {
            let mut auth = spur_core::config::AuthConfig::default();
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                auth.private_data
                    .insert(spur_core::config::PrivateData::Users);
            }
            auth
        });

        let ask_for_bob = || {
            let mut req = Request::new(ListUsersRequest {
                user: "bob".into(),
                account: String::new(),
                associations: false,
            });
            req.extensions_mut().insert(identity("alice", false));
            req
        };
        let err = service.list_users(ask_for_bob()).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        users_private.store(true, std::sync::atomic::Ordering::Relaxed);
        let listed = service
            .list_users(ask_for_bob())
            .await
            .unwrap()
            .into_inner();
        assert!(listed.users.is_empty());
        assert_eq!(listed.scoped_to_user.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn list_accounts_pins_an_ordinary_caller_when_accounts_are_private() {
        let service = AccountingService::unavailable("no DB needed for this test");
        fix_auth(&service, auth_hiding(&["accounts"]));

        let mut alice = Request::new(ListAccountsRequest { shares: false });
        alice.extensions_mut().insert(identity("alice", false));
        let err = service.list_accounts(alice).await.unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::Unavailable,
            "a cold association cache cannot tell a coordinator from anyone else"
        );

        let cache = std::sync::Arc::new(crate::association_cache::AssociationCache::new());
        cache.insert_association("alice", "physics");
        service.attach_association_cache(cache.clone());
        let mut alice = Request::new(ListAccountsRequest { shares: false });
        alice.extensions_mut().insert(identity("alice", false));
        let err = service.list_accounts(alice).await.unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::PermissionDenied,
            "membership is not coordination"
        );

        let mut shares = Request::new(ListAccountsRequest { shares: true });
        shares.extensions_mut().insert(identity("alice", false));
        let err = service.list_accounts(shares).await.unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::Unavailable,
            "the share tree is not the account listing, so a non-coordinator is not denied"
        );

        cache.insert_coordinator("alice", "physics");
        let mut alice = Request::new(ListAccountsRequest { shares: false });
        alice.extensions_mut().insert(identity("alice", false));
        let err = service
            .list_accounts(alice)
            .await
            .expect_err("a coordinated account still has to be read from the database");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        assert!(!account_allowed(Some(&["physics".into()]), "ml"));
        assert!(account_allowed(Some(&["physics".into()]), "physics"));
        assert!(account_allowed(None, "ml"));

        let mut admin = Request::new(ListAccountsRequest { shares: false });
        admin.extensions_mut().insert(identity("carol", true));
        let err = service.list_accounts(admin).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    fn history_for(user: &str) -> Request<GetJobHistoryRequest> {
        let mut req = Request::new(GetJobHistoryRequest {
            user: user.into(),
            ..Default::default()
        });
        req.extensions_mut().insert(identity("alice", false));
        req
    }

    fn usage_for(user: &str) -> Request<GetUsageRequest> {
        let mut req = Request::new(GetUsageRequest {
            user: user.into(),
            ..Default::default()
        });
        req.extensions_mut().insert(identity("alice", false));
        req
    }

    #[tokio::test]
    async fn job_history_pins_an_ordinary_caller_when_jobs_are_private() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let err = service
            .get_job_history(history_for("bob"))
            .await
            .expect_err("without private_data the query still runs");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        fix_auth(&service, auth_hiding(&["jobs"]));
        let jobs = service
            .get_job_history(history_for("bob"))
            .await
            .unwrap()
            .into_inner()
            .jobs;
        assert!(jobs.is_empty());

        let err = service
            .get_job_history(history_for(""))
            .await
            .expect_err("the caller's own history still comes from the database");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        // `usage` does not hide job history.
        fix_auth(&service, auth_hiding(&["usage"]));
        let err = service
            .get_job_history(history_for("bob"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        fix_auth(&service, auth_hiding(&["jobs"]));
        let cache = std::sync::Arc::new(crate::association_cache::AssociationCache::new());
        cache.insert_admin_level("op", "Operator");
        service.attach_association_cache(cache);
        let mut operator = Request::new(GetJobHistoryRequest {
            user: "bob".into(),
            ..Default::default()
        });
        operator.extensions_mut().insert(identity("op", false));
        let err = service.get_job_history(operator).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
        let err = service
            .get_job_history(Request::new(GetJobHistoryRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn usage_pins_usage_and_fairshare_to_the_caller() {
        let service = AccountingService::unavailable("no DB needed for this test");
        let err = service
            .get_usage(usage_for("bob"))
            .await
            .expect_err("without private_data the query still runs");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        fix_auth(&service, auth_hiding(&["usage"]));
        let usage = service
            .get_usage(usage_for("bob"))
            .await
            .unwrap()
            .into_inner();
        assert!(usage.entries.is_empty());
        assert_eq!(usage.scoped_to_user.as_deref(), Some("alice"));

        let err = service
            .get_usage(usage_for(""))
            .await
            .expect_err("the caller's own usage still comes from the database");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        let err = service
            .get_fairshare_factors(Request::new(GetFairshareFactorsRequest::default()))
            .await
            .expect_err("anonymous fairshare still comes from the database");
        assert_eq!(err.code(), tonic::Code::Unavailable);

        let mut unnamed = Request::new(GetFairshareFactorsRequest {
            halflife_days: 7,
            job_scoped: false,
        });
        unnamed.extensions_mut().insert(identity("", false));
        let factors = service
            .get_fairshare_factors(unnamed)
            .await
            .unwrap()
            .into_inner();
        assert!(factors.entries.is_empty());
        assert_eq!(factors.effective_halflife_days, 7);
        assert_eq!(factors.scoped_to_user.as_deref(), Some(""));

        let mut named = Request::new(GetFairshareFactorsRequest::default());
        named.extensions_mut().insert(identity("alice", false));
        let err = service.get_fairshare_factors(named).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn fairshare_entries_drop_other_users_under_a_caller_scope() {
        let rows = vec![
            FairshareEntry {
                user: "alice".into(),
                account: "ml".into(),
                factor: 1.0,
            },
            FairshareEntry {
                user: "bob".into(),
                account: "ml".into(),
                factor: 2.0,
            },
        ];
        let kept = scoped_fairshare(
            &spur_core::rbac::ReadScope::Caller("alice".into()),
            rows.clone(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].user, "alice");
        assert_eq!(
            scoped_fairshare(&spur_core::rbac::ReadScope::All, rows).len(),
            2
        );
        assert_eq!(scoped_to_user(&spur_core::rbac::ReadScope::All), None);
        assert_eq!(
            scoped_to_user(&spur_core::rbac::ReadScope::Caller("alice".into())).as_deref(),
            Some("alice")
        );
    }

    /// Every spelling Slurm accepts must survive, and the admin ones must converge: `sacctmgr show
    /// user` prints `Administrator`, so rejecting it would break round-tripping Slurm's own output.
    #[test]
    fn normalize_admin_level_canonicalizes_slurm_spellings() {
        for (input, want) in [
            ("", ""),
            ("none", "None"),
            ("None", "None"),
            ("operator", "Operator"),
            ("Operator", "Operator"),
            ("admin", "Administrator"),
            ("Admin", "Administrator"),
            ("Administrator", "Administrator"),
            ("SuperUser", "Administrator"),
        ] {
            assert_eq!(
                normalize_admin_level(input).unwrap(),
                want,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn normalize_admin_level_rejects_non_levels() {
        for level in ["adminn", "root", "yes"] {
            let err = normalize_admin_level(level).expect_err("level must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(
                err.message().contains("Admin"),
                "denial must name the accepted levels: {}",
                err.message()
            );
        }
    }

    #[test]
    fn validate_tres_accepts_empty() {
        assert!(validate_tres("grptres", "").is_ok());
    }

    #[test]
    fn validate_tres_accepts_well_formed() {
        assert!(validate_tres("maxtresperjob", "cpu=8,mem=1024").is_ok());
    }

    #[test]
    fn validate_tres_rejects_unit_suffixed_value() {
        let err = validate_tres("grptres", "mem=1G").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("grptres"));
    }

    #[test]
    fn validate_tres_rejects_unknown_type() {
        let err = validate_tres("maxtresperuser", "bogus=5").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn fairshare_to_i32_accepts_whole_numbers() {
        assert_eq!(fairshare_to_i32(0.0).unwrap(), 0);
        assert_eq!(fairshare_to_i32(2.0).unwrap(), 2);
        assert_eq!(fairshare_to_i32(f64::from(i32::MAX)).unwrap(), i32::MAX);
    }

    #[test]
    fn fairshare_to_i32_rejects_fractional_instead_of_truncating() {
        // The bug this guards: 2.9 must not silently become 2.
        let err = fairshare_to_i32(2.9).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("whole number"));
    }

    #[test]
    fn fairshare_to_i32_rejects_negative_and_out_of_range() {
        assert_eq!(
            fairshare_to_i32(-1.0).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            fairshare_to_i32(f64::from(i32::MAX) + 1.0)
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn nullable_limit_maps_sentinels() {
        assert_eq!(nullable_limit(None, "x").unwrap(), None);
        assert_eq!(
            nullable_limit(Some(spur_core::accounting::INFINITE), "x").unwrap(),
            Some(None)
        );
        // 0 is a real value ("block all"), not a clear.
        assert_eq!(nullable_limit(Some(0), "x").unwrap(), Some(Some(0)));
        assert_eq!(nullable_limit(Some(5), "x").unwrap(), Some(Some(5)));
    }

    #[test]
    fn nullable_limit_rejects_over_i32_max() {
        let err = nullable_limit(Some(i32::MAX as u32 + 1), "maxsubmit").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("maxsubmit"));
    }

    #[test]
    fn grace_time_patch_keeps_when_absent_and_stores_literal_zero() {
        assert_eq!(grace_time_patch(None, false).unwrap(), None);
        assert_eq!(grace_time_patch(Some(0), false).unwrap(), Some(Some(0)));
        assert_eq!(grace_time_patch(Some(600), false).unwrap(), Some(Some(600)));
    }

    #[test]
    fn grace_time_patch_clear_writes_null() {
        assert_eq!(grace_time_patch(None, true).unwrap(), Some(None));
    }

    #[test]
    fn grace_time_patch_rejects_a_value_together_with_a_clear() {
        for value in [0, 600] {
            let err = grace_time_patch(Some(value), true).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert_eq!(
                err.message(),
                "grace_time and clear_grace_time cannot both be set"
            );
        }
    }

    #[test]
    fn grace_time_audit_records_a_value_a_clear_and_nothing() {
        assert_eq!(
            grace_time_audit(grace_time_patch(None, false).unwrap()),
            None
        );
        assert_eq!(
            grace_time_audit(grace_time_patch(Some(600), false).unwrap()),
            Some(serde_json::json!(600))
        );
        assert_eq!(
            grace_time_audit(grace_time_patch(None, true).unwrap()),
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn grace_time_patch_rejects_over_i32_max() {
        let err = grace_time_patch(Some(i32::MAX as u32 + 1), false).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("grace_time"));
    }

    #[test]
    fn qos_info_from_record_maps_grace_time_unset_zero_and_negative() {
        let record = |grace_time| db::QosRecord {
            name: "preemptible".into(),
            description: String::new(),
            priority: 0,
            preempt_mode: String::new(),
            preempt: String::new(),
            usage_factor: 1.0,
            max_jobs_per_user: None,
            max_wall_min: None,
            max_tres_per_job: None,
            max_submit_per_user: None,
            max_submit_per_account: None,
            grp_submit_jobs: None,
            max_tres_per_user: None,
            grp_tres: None,
            grp_wall_min: None,
            preempt_exempt_time: None,
            grace_time,
            idle_fill_preemptable: false,
            flags: String::new(),
        };
        assert_eq!(
            qos_info_from_record(record(Some(600))).grace_time,
            Some(600)
        );
        assert_eq!(qos_info_from_record(record(Some(0))).grace_time, Some(0));
        assert_eq!(qos_info_from_record(record(None)).grace_time, None);
        assert_eq!(qos_info_from_record(record(Some(-1))).grace_time, None);
    }

    #[test]
    fn canonicalize_qos_flags_normalizes_and_validates() {
        assert_eq!(canonicalize_qos_flags("").unwrap(), "");
        assert_eq!(
            canonicalize_qos_flags("denyonlimit").unwrap(),
            "DenyOnLimit"
        );
        assert_eq!(
            canonicalize_qos_flags(" DENYONLIMIT , ").unwrap(),
            "DenyOnLimit"
        );
        let err = canonicalize_qos_flags("bogus").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("bogus"));
    }

    #[test]
    fn timestamp_to_utc_passes_none_through() {
        assert_eq!(timestamp_to_utc(None).unwrap(), None);
    }

    #[test]
    fn timestamp_to_utc_accepts_valid() {
        let ts = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 123,
        };
        let dt = timestamp_to_utc(Some(ts)).unwrap().unwrap();
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    #[test]
    fn timestamp_to_utc_rejects_negative_nanos_instead_of_coercing_to_epoch() {
        let ts = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: -1,
        };
        let err = timestamp_to_utc(Some(ts)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn timestamp_to_utc_rejects_out_of_range_nanos() {
        let ts = prost_types::Timestamp {
            seconds: 0,
            nanos: 1_000_000_000,
        };
        assert_eq!(
            timestamp_to_utc(Some(ts)).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn usage_entry_returns_cpu_seconds_not_hours() {
        let now = chrono::Utc::now();
        let records = vec![
            db::UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                cpu_seconds: 240,
                gpu_seconds: 0,
                job_count: 1,
                period_start: now,
            },
            db::UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                cpu_seconds: 480,
                gpu_seconds: 100,
                job_count: 2,
                period_start: now,
            },
        ];

        let mut agg: std::collections::HashMap<(String, String), (f64, f64, u64)> =
            std::collections::HashMap::new();
        for r in &records {
            let e = agg
                .entry((r.user_name.clone(), r.account.clone()))
                .or_default();
            e.0 += r.cpu_seconds as f64;
            e.1 += r.gpu_seconds as f64;
            e.2 += r.job_count;
        }

        let entries: Vec<UsageEntry> = agg
            .into_iter()
            .map(|((user, account), (cpu, gpu, jobs))| UsageEntry {
                user,
                account,
                cpu_seconds: cpu,
                gpu_seconds: gpu,
                job_count: jobs,
            })
            .collect();

        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(
            e.cpu_seconds, 720.0,
            "must be raw seconds (240+480), not hours"
        );
        assert_eq!(e.gpu_seconds, 100.0);
        assert_eq!(e.job_count, 3);
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn get_usage_handler_returns_cpu_seconds_not_hours() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("DB connect");
        db::migrate(&pool).await.expect("migrate");

        let pid = std::process::id();
        let user = format!("spur_unit_{pid}");
        let acct = format!("spur_unitacct_{pid}");

        let now = chrono::Utc::now();
        let num_tasks: i32 = 1;
        let cpus_per_task: i32 = 8;
        let wall_secs: i64 = 30;

        sqlx::query(
            "INSERT INTO usage (user_name, account, period_start, period_end, cpu_seconds, gpu_seconds, job_count) \
             VALUES ($1, $2, $3, $4, $5, 0, 1) \
             ON CONFLICT (user_name, account, period_start) DO UPDATE SET \
               cpu_seconds = usage.cpu_seconds + $5, job_count = usage.job_count + 1",
        )
        .bind(&user)
        .bind(&acct)
        .bind(now)
        .bind(now + chrono::Duration::hours(1))
        .bind(wall_secs * num_tasks as i64 * cpus_per_task as i64)
        .execute(&pool)
        .await
        .expect("insert usage");

        let service = AccountingService::unavailable("test");
        service.install_pool(pool.clone());

        let resp = service
            .get_usage(tonic::Request::new(GetUsageRequest {
                user: user.clone(),
                account: acct.clone(),
                since: None,
                job_scoped: false,
            }))
            .await
            .expect("get_usage");

        let entries = &resp.into_inner().entries;
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(
            e.cpu_seconds,
            (wall_secs * num_tasks as i64 * cpus_per_task as i64) as f64,
            "handler must return raw cpu_seconds (240), not cpu_hours (0.067)"
        );

        sqlx::query("DELETE FROM usage WHERE user_name = $1 AND account = $2")
            .bind(&user)
            .bind(&acct)
            .execute(&pool)
            .await
            .ok();
    }
}
