// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tonic::{Request, Response, Status};

use spur_core::accounting::TresRecord;
use spur_proto::proto::slurm_accounting_server::{SlurmAccounting, SlurmAccountingServer};
use spur_proto::proto::*;

use super::{db, fairshare};

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

/// Map an optional TRES/text proto field to a nullable-column patch: unset ->
/// keep, empty -> clear (SQL NULL), otherwise -> set.
fn nullable_str(field: &Option<String>) -> Option<Option<&str>> {
    field
        .as_deref()
        .map(|s| if s.is_empty() { None } else { Some(s) })
}

/// Map an optional u32 limit proto field to a nullable-int patch: unset ->
/// keep, 0 -> clear (no limit), n -> set. Errors if `n` overflows i32.
fn nullable_limit(field: Option<u32>, what: &str) -> Result<Option<Option<i32>>, Status> {
    match field {
        None => Ok(None),
        Some(0) => Ok(Some(None)),
        Some(n) => Ok(Some(Some(i32::try_from(n).map_err(|_| {
            Status::invalid_argument(format!("{what} exceeds i32::MAX"))
        })?))),
    }
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

pub(crate) enum AccountingService {
    Available(PgPool),
    Unavailable { reason: &'static str },
}

impl AccountingService {
    pub(crate) fn available(pool: PgPool) -> Self {
        Self::Available(pool)
    }

    pub(crate) fn unavailable(reason: &'static str) -> Self {
        Self::Unavailable { reason }
    }

    fn pool(&self) -> Result<&PgPool, Status> {
        match self {
            Self::Available(pool) => Ok(pool),
            Self::Unavailable { reason } => Err(Status::unavailable(format!(
                "accounting service is not available ({reason})"
            ))),
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
        let req = request.into_inner();
        let start_time = req
            .start_time
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or_else(Utc::now);
        let submit_time = req
            .submit_time
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or(start_time);

        let (memory_mb, cpus) = req
            .resources
            .as_ref()
            .map(|r| (r.memory_mb as i64, r.cpus as i32))
            .unwrap_or((0, 1));

        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        db::record_job_start(
            &mut conn,
            req.job_id as i32,
            &req.name,
            &req.user,
            &req.account,
            &req.partition,
            1, // num_nodes — simplified
            cpus,
            1,
            memory_mb,
            submit_time,
            start_time,
            &req.reservation,
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

        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        db::record_job_end(
            &mut conn,
            req.job_id as i32,
            state_str,
            req.exit_code,
            end_time,
            req.exit_signal,
            req.derived_exit_code,
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
    }

    async fn get_job_history(
        &self,
        request: Request<GetJobHistoryRequest>,
    ) -> Result<Response<GetJobHistoryResponse>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();

        let start_after = req
            .start_after
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default());
        let start_before = req
            .start_before
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default());

        let states: Vec<String> = req
            .states
            .iter()
            .filter_map(|s| match *s {
                3 => Some("COMPLETED".into()),
                4 => Some("FAILED".into()),
                5 => Some("CANCELLED".into()),
                6 => Some("TIMEOUT".into()),
                10 => Some("DEADLINE".into()),
                _ => None,
            })
            .collect();

        let user = if req.user.is_empty() {
            None
        } else {
            Some(req.user.as_str())
        };
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };

        let records = db::get_job_history(
            pool,
            user,
            account,
            start_after,
            start_before,
            &states,
            req.limit,
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        let jobs = records
            .iter()
            .map(|r| JobInfo {
                job_id: r.job_id as u32,
                name: r.name.clone(),
                user: r.user_name.clone(),
                uid: 0,
                partition: r.partition.clone(),
                account: r.account.clone(),
                state: match r.state.as_str() {
                    "COMPLETED" => JobState::JobCompleted as i32,
                    "FAILED" => JobState::JobFailed as i32,
                    "CANCELLED" => JobState::JobCancelled as i32,
                    "TIMEOUT" => JobState::JobTimeout as i32,
                    "DEADLINE" => JobState::JobDeadline as i32,
                    "RUNNING" => JobState::JobRunning as i32,
                    "PENDING" => JobState::JobPending as i32,
                    _ => JobState::JobCompleted as i32,
                },
                state_reason: String::new(),
                submit_time: Some(datetime_to_proto(r.submit_time)),
                start_time: r.start_time.map(datetime_to_proto),
                end_time: r.end_time.map(datetime_to_proto),
                time_limit: None,
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
                req_gpus: 0,
                req_gpus_detail: String::new(),
                run_attempt: 0,
            })
            .collect();

        Ok(Response::new(GetJobHistoryResponse { jobs }))
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();

        let since = req
            .since
            .map(|t| DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default())
            .unwrap_or_else(|| Utc::now() - chrono::Duration::days(30));

        let user = if req.user.is_empty() {
            None
        } else {
            Some(req.user.as_str())
        };
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };

        let records = db::get_usage(pool, user, account, since)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut agg: std::collections::HashMap<(String, String), (f64, f64, u64)> =
            std::collections::HashMap::new();
        for r in &records {
            let e = agg
                .entry((r.user_name.clone(), r.account.clone()))
                .or_default();
            e.0 += r.cpu_seconds as f64 / 3600.0;
            e.1 += r.gpu_seconds as f64 / 3600.0;
            e.2 += r.job_count;
        }

        let entries = agg
            .into_iter()
            .map(|((user, account), (cpu, gpu, jobs))| UsageEntry {
                user,
                account,
                cpu_hours: cpu,
                gpu_hours: gpu,
                job_count: jobs,
            })
            .collect();

        Ok(Response::new(GetUsageResponse { entries }))
    }

    // ============================================================
    // Account management
    // ============================================================

    async fn create_account(
        &self,
        request: Request<CreateAccountRequest>,
    ) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
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
        db::upsert_account(pool, &req.name, update)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn delete_account(
        &self,
        request: Request<DeleteAccountRequest>,
    ) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
        db::delete_account(pool, &req.name)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn list_accounts(
        &self,
        _request: Request<ListAccountsRequest>,
    ) -> Result<Response<ListAccountsResponse>, Status> {
        let pool = self.pool()?;
        let records = db::list_accounts(pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let accounts = records
            .into_iter()
            .map(|r| AccountInfo {
                name: r.name,
                description: r.description,
                organization: r.organization,
                parent_account: r.parent.unwrap_or_default(),
                fairshare_weight: r.fairshare_weight as f64,
                max_running_jobs: r.max_running_jobs.unwrap_or(0) as u32,
                grp_tres: r.grp_tres.unwrap_or_default(),
            })
            .collect();

        Ok(Response::new(ListAccountsResponse { accounts }))
    }

    async fn add_user(&self, request: Request<AddUserRequest>) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
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
            admin_level: req.admin_level.as_deref(),
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
            max_tres_per_job: nullable_str(&req.max_tres_per_job),
            grp_tres: nullable_str(&req.grp_tres),
            max_wall_min: nullable_limit(req.max_wall_minutes, "max_wall_minutes")?,
        };
        db::add_user(pool, &req.user, &req.account, update)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn remove_user(
        &self,
        request: Request<RemoveUserRequest>,
    ) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
        let deleted = db::remove_user(pool, &req.user, &req.account)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        if deleted == 0 {
            let target = if req.account.is_empty() {
                format!("user '{}'", req.user)
            } else {
                format!("user '{}' in account '{}'", req.user, req.account)
            };
            return Err(Status::not_found(format!("{target} does not exist")));
        }
        Ok(Response::new(()))
    }

    async fn list_users(
        &self,
        request: Request<ListUsersRequest>,
    ) -> Result<Response<ListUsersResponse>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };
        let user = if req.user.is_empty() {
            None
        } else {
            Some(req.user.as_str())
        };
        let records = db::list_users(pool, account, user)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let users = records
            .into_iter()
            .map(|r| UserInfo {
                name: r.name,
                account: r.account,
                admin_level: r.admin_level,
                default_account: r.default_account.unwrap_or_default(),
                default_qos: r.default_qos.unwrap_or_default(),
                allowed_qos: r.allowed_qos.unwrap_or_default(),
            })
            .collect();

        Ok(Response::new(ListUsersResponse { users }))
    }

    async fn create_qos(&self, request: Request<CreateQosRequest>) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
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
        let update = db::QosUpdate {
            description: req.description.as_deref(),
            priority: req.priority,
            preempt_mode: req.preempt_mode.as_deref(),
            usage_factor: req.usage_factor,
            max_jobs_per_user: nullable_limit(req.max_jobs_per_user, "max_jobs_per_user")?,
            max_wall_min: nullable_limit(req.max_wall_minutes, "max_wall_minutes")?,
            max_tres_per_job: nullable_str(&req.max_tres_per_job),
            max_submit_per_user: nullable_limit(
                req.max_submit_jobs_per_user,
                "max_submit_jobs_per_user",
            )?,
            max_tres_per_user: nullable_str(&req.max_tres_per_user),
            grp_tres: nullable_str(&req.grp_tres),
            grp_wall_min: nullable_limit(req.grp_wall_minutes, "grp_wall_minutes")?,
        };
        db::upsert_qos(pool, &req.name, update)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn delete_qos(&self, request: Request<DeleteQosRequest>) -> Result<Response<()>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
        db::delete_qos(pool, &req.name)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn list_qos(
        &self,
        _request: Request<ListQosRequest>,
    ) -> Result<Response<ListQosResponse>, Status> {
        let pool = self.pool()?;
        let records = db::list_qos(pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let qos_list = records
            .into_iter()
            .map(|r| QosInfo {
                name: r.name,
                description: r.description,
                priority: r.priority,
                preempt_mode: r.preempt_mode,
                usage_factor: r.usage_factor,
                max_jobs_per_user: r.max_jobs_per_user.unwrap_or(0) as u32,
                max_wall_minutes: r.max_wall_min.unwrap_or(0) as u32,
                max_tres_per_job: r.max_tres_per_job.unwrap_or_default(),
                max_submit_jobs_per_user: r.max_submit_per_user.unwrap_or(0) as u32,
                max_tres_per_user: r.max_tres_per_user.unwrap_or_default(),
                grp_tres: r.grp_tres.unwrap_or_default(),
                grp_wall_minutes: r.grp_wall_min.unwrap_or(0) as u32,
            })
            .collect();

        Ok(Response::new(ListQosResponse { qos_list }))
    }

    async fn get_fairshare_factors(
        &self,
        request: Request<GetFairshareFactorsRequest>,
    ) -> Result<Response<GetFairshareFactorsResponse>, Status> {
        let pool = self.pool()?;
        let req = request.into_inner();
        let halflife_days = if req.halflife_days == 0 {
            14
        } else {
            req.halflife_days.clamp(1, 365)
        };

        let now = Utc::now();
        let since = now - chrono::Duration::days(halflife_days as i64 * 4);

        let usage = db::get_usage(pool, None, None, since)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let accounts = db::list_accounts(pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let account_weights: std::collections::HashMap<String, f64> = accounts
            .into_iter()
            .map(|a| (a.name, a.fairshare_weight as f64))
            .collect();

        let raw_factors =
            fairshare::compute_fairshare(&usage, &account_weights, halflife_days, now);

        let entries = raw_factors
            .into_iter()
            .map(|((user, account), factor)| FairshareEntry {
                user,
                account,
                factor,
            })
            .collect();

        Ok(Response::new(GetFairshareFactorsResponse { entries }))
    }
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
}
