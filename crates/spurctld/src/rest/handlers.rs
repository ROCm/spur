// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::Json;
use axum::Extension;

use super::convert::{job_to_json, node_to_json, parse_states_query, partition_to_json};
use super::types::*;
use super::RestState;
use crate::server::{identified_user_may_view_job, identity_operates_jobs};

pub async fn ping(
    State(state): State<Arc<RestState>>,
) -> Result<Json<ApiResponse<PingData>>, RestError> {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());

    Ok(ApiResponse::ok(PingData {
        ping: vec![PingInfo {
            hostname,
            pinged: "UP".into(),
            latency: 0,
            mode: if state.raft.is_leader() {
                "primary"
            } else {
                "replica"
            }
            .into(),
        }],
    }))
}

pub async fn get_jobs(
    State(state): State<Arc<RestState>>,
    Query(query): Query<JobsQuery>,
    identity: Option<Extension<spur_core::auth::Identity>>,
) -> Result<Json<ApiResponse<JobsData>>, RestError> {
    let identity = identity.map(|Extension(id)| id);
    let states = match query.state.as_deref() {
        Some(s) => parse_states_query(s).map_err(|e| bad_request_response(&e))?,
        None => Vec::new(),
    };

    let scoped_user = rest_list_user(query.user.as_deref(), identity.as_ref(), &state.cluster);
    let partition = query.partition.as_deref();
    let account = query.account.as_deref();
    let name = query.name.as_deref();

    let jobs = state.cluster.get_jobs(&crate::cluster::JobFilter {
        states: &states,
        user: scoped_user.as_deref(),
        partition,
        account,
        name,
        ..Default::default()
    });
    let json_jobs: Vec<serde_json::Value> = jobs
        .iter()
        .filter_map(|job| rest_job_json(job, identity.as_ref(), &state.cluster))
        .collect();

    Ok(ApiResponse::ok(JobsData { jobs: json_jobs }))
}

pub async fn get_job(
    State(state): State<Arc<RestState>>,
    Path(job_id): Path<u32>,
    identity: Option<Extension<spur_core::auth::Identity>>,
) -> Result<Json<ApiResponse<JobsData>>, RestError> {
    let identity = identity.map(|Extension(id)| id);
    let job = state
        .cluster
        .get_job_for_display(job_id)
        .ok_or_else(|| not_found_response(&format!("job {job_id} not found")))?;
    let json = rest_job_json(&job, identity.as_ref(), &state.cluster)
        .ok_or_else(|| not_found_response(&format!("job {job_id} not found")))?;

    Ok(ApiResponse::ok(JobsData { jobs: vec![json] }))
}

/// Default an absent or zero REST `ntasks` to one task per requested node
/// (Slurm's default), so a multi-node request is not silently collapsed to one.
fn rest_effective_ntasks(ntasks: Option<u32>, nodes: Option<u32>) -> u32 {
    ntasks
        .filter(|&n| n > 0)
        .unwrap_or_else(|| nodes.unwrap_or(1).max(1))
}

pub async fn submit_job(
    State(state): State<Arc<RestState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    identity: Option<Extension<spur_core::auth::Identity>>,
    Json(body): Json<SubmitRequest>,
) -> Result<Json<ApiResponse<SubmitResponse>>, RestError> {
    // REST carries no uid, so an unauthenticated submission could only run as
    // uid 0. Refused here rather than attributed to root.
    let Some(Extension(identity)) = identity else {
        return Err(bad_request_response(
            "REST job submission requires an authenticated caller (Authorization: Bearer). \
             Submit via `sbatch`/`srun`, or pass a credential so the job can be attributed \
             to that user rather than uid 0.",
        ));
    };

    let time_limit = body
        .job
        .time_limit
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_minutes(t))
        .map(|mins| prost_types::Duration {
            seconds: mins as i64 * 60,
            nanos: 0,
        });

    let spec = spur_proto::proto::JobSpec {
        name: body.job.name.unwrap_or_default(),
        user: body.job.user.unwrap_or_default(),
        partition: body.job.partition.unwrap_or_default(),
        account: body.job.account.unwrap_or_default(),
        num_nodes: body.job.nodes.unwrap_or(1),
        num_tasks: rest_effective_ntasks(body.job.ntasks, body.job.nodes),
        cpus_per_task: body.job.cpus_per_task.unwrap_or(1),
        time_limit,
        script: body.job.script.unwrap_or_default(),
        environment: body.job.environment,
        gres: body.job.gres,
        gpus: proto_gpu(body.job.gpus.as_deref())?,
        gpus_per_node: proto_gpu(body.job.gpus_per_node.as_deref())?,
        gpus_per_task: proto_gpu(body.job.gpus_per_task.as_deref())?,
        ..Default::default()
    };

    let response = dispatch(
        &state,
        "SubmitJob",
        peer,
        identity,
        spur_proto::proto::SubmitJobRequest { spec: Some(spec) },
        |svc, req| async move {
            spur_proto::proto::slurm_controller_server::SlurmController::submit_job(&svc, req).await
        },
    )
    .await?
    .into_inner();

    Ok(ApiResponse::ok(SubmitResponse {
        job_id: response.job_id,
        warnings: response.warnings,
    }))
}

/// Hand a request to the controller handler that owns this RPC, so REST shares
/// its authorization, validation, leader forwarding and audit row.
async fn dispatch<Req, Resp, F, Fut>(
    state: &Arc<RestState>,
    method: &str,
    peer: std::net::SocketAddr,
    identity: spur_core::auth::Identity,
    message: Req,
    call: F,
) -> Result<tonic::Response<Resp>, RestError>
where
    F: FnOnce(crate::server::ControllerService, tonic::Request<Req>) -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
{
    let mut request = tonic::Request::new(message);
    // `rest_auth` inserts an identity only after verifying the credential, so
    // anything reaching here is verified.
    request.extensions_mut().insert(identity);
    request
        .extensions_mut()
        .insert(crate::auth_middleware::Verified);

    let service = state.controller.clone();
    let context = crate::audit::ControllerAudit::new(state.cluster.clone());
    crate::audit::recorded(&context, method, Some(peer.to_string()), request, |req| {
        call(service, req)
    })
    .await
    .map_err(status_to_rest)
}

/// Parse a REST GPU field ("4" or "mi300x:4") into a core GPU request.
#[allow(clippy::result_large_err)]
fn parse_rest_gpu(
    value: Option<&str>,
) -> Result<Option<spur_core::gpu_request::GpuRequest>, RestError> {
    match value {
        Some(v) if !v.is_empty() => spur_core::gpu_request::GpuRequest::parse_flag(v)
            .map_err(|e| bad_request_response(&e.to_string())),
        _ => Ok(None),
    }
}

/// Mutations run through the controller handlers, which speak gRPC codes.
fn status_to_rest(status: tonic::Status) -> RestError {
    let msg = status.message();
    match status.code() {
        tonic::Code::InvalidArgument => bad_request_response(msg),
        tonic::Code::NotFound => not_found_response(msg),
        tonic::Code::PermissionDenied => forbidden_response(msg),
        tonic::Code::Unauthenticated => unauthorized_response(msg),
        tonic::Code::Unavailable => unavailable_response(msg),
        // A job already in a terminal state, and the like: the caller's view is
        // stale rather than the request being malformed.
        tonic::Code::FailedPrecondition | tonic::Code::Aborted => {
            api_error_response(axum::http::StatusCode::CONFLICT, msg)
        }
        _ => error_response(msg),
    }
}

#[allow(clippy::result_large_err)]
fn proto_gpu(value: Option<&str>) -> Result<Option<spur_proto::proto::GpuRequest>, RestError> {
    Ok(
        parse_rest_gpu(value)?.map(|g| spur_proto::proto::GpuRequest {
            count: g.count,
            gpu_type: g.gpu_type.unwrap_or_default(),
        }),
    )
}

fn rest_list_user(
    query_user: Option<&str>,
    identity: Option<&spur_core::auth::Identity>,
    cluster: &crate::cluster::ClusterManager,
) -> Option<String> {
    rest_list_user_from_flags(
        query_user,
        identity.map(|id| id.user.as_str()),
        identity_operates_jobs(cluster, identity),
    )
}

fn rest_list_user_from_flags(
    query_user: Option<&str>,
    identity_user: Option<&str>,
    is_operator: bool,
) -> Option<String> {
    if is_operator {
        return query_user.filter(|u| !u.is_empty()).map(str::to_string);
    }
    if let Some(user) = identity_user {
        return Some(user.to_string());
    }
    query_user.filter(|u| !u.is_empty()).map(str::to_string)
}

fn rest_job_json(
    job: &spur_core::job::Job,
    identity: Option<&spur_core::auth::Identity>,
    cluster: &crate::cluster::ClusterManager,
) -> Option<serde_json::Value> {
    let operates = identity_operates_jobs(cluster, identity);
    identified_user_may_view_job(identity, &job.spec.user, operates).then(|| job_to_json(job))
}

pub async fn cancel_job(
    State(state): State<Arc<RestState>>,
    Path(job_id): Path<u32>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    request: axum::extract::Request,
) -> Result<Json<ApiResponse<serde_json::Value>>, RestError> {
    let Some(identity) = request
        .extensions()
        .get::<spur_core::auth::Identity>()
        .cloned()
    else {
        return Err(unauthorized_response(
            "REST job cancel requires an authenticated caller (Authorization: Bearer).",
        ));
    };

    // Ownership, agent fan-out and the audit row all live in the handler.
    dispatch(
        &state,
        "CancelJob",
        peer,
        identity,
        spur_proto::proto::CancelJobRequest {
            job_id,
            ..Default::default()
        },
        |svc, req| async move {
            spur_proto::proto::slurm_controller_server::SlurmController::cancel_job(&svc, req).await
        },
    )
    .await?;

    Ok(ApiResponse::ok(serde_json::json!({})))
}

pub async fn get_nodes(
    State(state): State<Arc<RestState>>,
) -> Result<Json<ApiResponse<NodesData>>, RestError> {
    let nodes = state.cluster.get_nodes();
    let json_nodes: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| node_to_json(n, planned_reservation_if_idle(&state.cluster, n)))
        .collect();

    Ok(ApiResponse::ok(NodesData { nodes: json_nodes }))
}

pub async fn get_node(
    State(state): State<Arc<RestState>>,
    Path(name): Path<String>,
) -> Result<Json<ApiResponse<NodesData>>, RestError> {
    let node = state
        .cluster
        .get_node(&name)
        .ok_or_else(|| not_found_response(&format!("node {name} not found")))?;

    let planned = planned_reservation_if_idle(&state.cluster, &node);
    Ok(ApiResponse::ok(NodesData {
        nodes: vec![node_to_json(&node, planned)],
    }))
}

/// Only meaningful while idle — a node that has since gone
/// Allocated/Down must not report a stale planned reservation.
fn planned_reservation_if_idle(
    cluster: &crate::cluster::ClusterManager,
    node: &spur_core::node::Node,
) -> Option<(spur_core::job::JobId, chrono::DateTime<chrono::Utc>)> {
    if node.state != spur_core::node::NodeState::Idle {
        return None;
    }
    cluster.planned_reservation(&node.name)
}

pub async fn get_partitions(
    State(state): State<Arc<RestState>>,
) -> Result<Json<ApiResponse<PartitionsData>>, RestError> {
    let partitions = state.cluster.get_partitions();
    let json_parts: Vec<serde_json::Value> = partitions.iter().map(partition_to_json).collect();

    Ok(ApiResponse::ok(PartitionsData {
        partitions: json_parts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_effective_ntasks_defaults_absent_to_nodes() {
        // C1 (REST): absent ntasks -> one task per node, not 1.
        assert_eq!(rest_effective_ntasks(None, Some(4)), 4);
        // Zero is treated as unset.
        assert_eq!(rest_effective_ntasks(Some(0), Some(4)), 4);
        // Explicit ntasks is preserved (reduces the job to one node later).
        assert_eq!(rest_effective_ntasks(Some(1), Some(4)), 1);
        // No nodes given falls back to a single task.
        assert_eq!(rest_effective_ntasks(None, None), 1);
    }

    #[test]
    fn parse_rest_gpu_valid() {
        let req = parse_rest_gpu(Some("mi300x:4"));
        assert!(req.is_ok());
        let req = req.ok().flatten().unwrap();
        assert_eq!(req.count, 4);
        assert_eq!(req.gpu_type, Some("mi300x".into()));
    }

    #[test]
    fn parse_rest_gpu_zero_is_none() {
        let res = parse_rest_gpu(Some("0"));
        assert!(res.is_ok());
        assert!(res.ok().flatten().is_none());
    }

    #[test]
    fn parse_rest_gpu_invalid_returns_error() {
        let err = parse_rest_gpu(Some("::bad"));
        assert!(err.is_err());
    }

    #[test]
    fn conflict_error_text_is_neutral() {
        let msg = spur_core::gpu_request::GpuRequestError::Conflict.to_string();
        assert!(
            !msg.contains("--"),
            "error message should not contain CLI flags"
        );
        assert!(msg.contains("gres"));
    }

    /// Mutations now come back as gRPC codes, so the HTTP status a client sees
    /// is decided here. A denial must not surface as a server fault.
    #[test]
    fn status_to_rest_maps_handler_codes() {
        use axum::http::StatusCode;
        use tonic::{Code, Status};

        for (code, want) in [
            (Code::PermissionDenied, StatusCode::FORBIDDEN),
            (Code::Unauthenticated, StatusCode::UNAUTHORIZED),
            (Code::NotFound, StatusCode::NOT_FOUND),
            (Code::InvalidArgument, StatusCode::BAD_REQUEST),
            (Code::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
            // Cancelling an already-finished job: a stale client view.
            (Code::FailedPrecondition, StatusCode::CONFLICT),
            (Code::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let (status, _) = status_to_rest(Status::new(code, "x"));
            assert_eq!(status, want, "{code:?}");
        }
    }

    #[test]
    fn rest_list_user_pins_identified_non_operator() {
        assert_eq!(
            rest_list_user_from_flags(Some("mallory"), Some("alice"), false).as_deref(),
            Some("alice")
        );
        assert_eq!(
            rest_list_user_from_flags(None, Some("alice"), false).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn rest_list_user_lets_operator_honor_query() {
        assert_eq!(
            rest_list_user_from_flags(Some("bob"), Some("erin"), true).as_deref(),
            Some("bob")
        );
        assert_eq!(rest_list_user_from_flags(None, Some("erin"), true), None);
    }

    #[test]
    fn rest_list_user_anonymous_keeps_query() {
        assert_eq!(
            rest_list_user_from_flags(Some("alice"), None, false).as_deref(),
            Some("alice")
        );
        assert_eq!(rest_list_user_from_flags(None, None, false), None);
    }
}
