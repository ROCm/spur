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
    let qos = empty_to_none(&query.qos);
    let reservation = empty_to_none(&query.reservation);

    let jobs = state.cluster.get_jobs(&crate::cluster::JobFilter {
        states: &states,
        user: scoped_user.as_deref(),
        partition,
        account,
        name,
        qos: qos.as_deref(),
        reservation: reservation.as_deref(),
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

/// Treat an empty query value the same as an absent one, so `?qos=` does not
/// filter on the empty QOS. Mirrors the gRPC handler's empty-string normalization.
fn empty_to_none(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
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
    identity: Option<Extension<spur_core::auth::Identity>>,
    Json(body): Json<SubmitRequest>,
) -> Result<Json<ApiResponse<SubmitResponse>>, RestError> {
    if !state.raft.is_leader() {
        return Err(unavailable_response("not the Raft leader"));
    }

    let identity = identity.map(|Extension(id)| id);
    let time_limit = body
        .job
        .time_limit
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_minutes(t))
        .map(|mins| chrono::Duration::minutes(mins as i64));

    let mut spec = spur_core::job::JobSpec {
        name: body.job.name.unwrap_or_default(),
        user: body.job.user.unwrap_or_default(),
        partition: body.job.partition,
        account: body.job.account,
        num_nodes: body.job.nodes.unwrap_or(1),
        num_tasks: rest_effective_ntasks(body.job.ntasks, body.job.nodes),
        cpus_per_task: body.job.cpus_per_task.unwrap_or(1),
        time_limit,
        script: body.job.script,
        environment: body.job.environment,
        gres: body.job.gres,
        gpus: parse_rest_gpu(body.job.gpus.as_deref())?,
        gpus_per_node: parse_rest_gpu(body.job.gpus_per_node.as_deref())?,
        gpus_per_task: parse_rest_gpu(body.job.gpus_per_task.as_deref())?,
        ..Default::default()
    };
    spur_core::auth::bind_job_spec(&mut spec, identity.as_ref()).map_err(|e| {
        bad_request_response(&format!(
            "cannot resolve UNIX credentials for authenticated user: {e}"
        ))
    })?;

    if identity.is_none() && spec.uid == 0 {
        return Err(bad_request_response(
            "REST job submission requires an authenticated caller (Authorization: Bearer). \
             Submit via `sbatch`/`srun`, or pass a credential so the job can be attributed \
             to that user rather than uid 0.",
        ));
    }

    let outcome = state.cluster.submit_job(spec).map_err(submit_rest_error)?;

    Ok(ApiResponse::ok(SubmitResponse {
        job_id: outcome.job_id,
        warnings: outcome.warnings,
    }))
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

fn submit_rest_error(err: crate::cluster::SubmitError) -> RestError {
    match err {
        crate::cluster::SubmitError::InvalidArgument(m) => bad_request_response(&m),
        crate::cluster::SubmitError::Unavailable(m) => unavailable_response(&m),
        crate::cluster::SubmitError::Internal(m) => error_response(&format!("submit failed: {m}")),
    }
}

fn rest_cancel_map_err(err: crate::cluster::CancelError) -> RestError {
    let msg = format!("cancel failed: {err}");
    match err {
        crate::cluster::CancelError::NotFound(_) => not_found_response(&msg),
        crate::cluster::CancelError::NotOwner(_) => forbidden_response(&msg),
        crate::cluster::CancelError::AlreadyTerminal { .. }
        | crate::cluster::CancelError::Internal(_) => error_response(&msg),
    }
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
    request: axum::extract::Request,
) -> Result<Json<ApiResponse<serde_json::Value>>, RestError> {
    if !state.raft.is_leader() {
        return Err(unavailable_response("not the Raft leader"));
    }

    let Some(identity) = request.extensions().get::<spur_core::auth::Identity>() else {
        return Err(unauthorized_response(
            "REST job cancel requires an authenticated caller (Authorization: Bearer).",
        ));
    };
    let user = identity.user.as_str();

    let job = state.cluster.get_job(job_id);

    state
        .cluster
        .cancel_job_for(
            job_id,
            user,
            identity_operates_jobs(&state.cluster, Some(identity)),
        )
        .map_err(rest_cancel_map_err)?;

    if let Some(job) = job {
        let cluster = state.cluster.clone();
        tokio::spawn(async move {
            crate::scheduler_loop::send_cancel_to_agents(&cluster, &job, 0).await;
        });
    }

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
    fn empty_to_none_treats_blank_query_as_absent() {
        assert_eq!(empty_to_none(&None), None);
        // `?qos=` must not filter on the empty QOS.
        assert_eq!(empty_to_none(&Some(String::new())), None);
        // A comma-separated list is passed through verbatim for the matcher to split.
        assert_eq!(
            empty_to_none(&Some("high,low".to_string())),
            Some("high,low".to_string())
        );
    }

    #[test]
    fn jobs_query_deserializes_qos_and_reservation() {
        let q: JobsQuery =
            serde_urlencoded::from_str("qos=high,low&reservation=maint").expect("valid query");
        assert_eq!(q.qos.as_deref(), Some("high,low"));
        assert_eq!(q.reservation.as_deref(), Some("maint"));
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

    #[test]
    fn rest_cancel_not_owner_is_forbidden() {
        let err = crate::cluster::CancelError::NotOwner(spur_core::auth::AuthError::NotJobOwner {
            user: "bob".into(),
            owner: "alice".into(),
            action: "cancel".into(),
        });
        let (status, _) = rest_cancel_map_err(err);
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn rest_cancel_other_errors_are_internal() {
        let (status, _) = rest_cancel_map_err(crate::cluster::CancelError::Internal(
            anyhow::anyhow!("raft propose failed"),
        ));
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
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
