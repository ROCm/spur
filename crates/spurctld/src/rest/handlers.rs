// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::Json;

use super::convert::{job_to_json, node_to_json, parse_states_query, partition_to_json};
use super::types::*;
use super::RestState;

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
) -> Result<Json<ApiResponse<JobsData>>, RestError> {
    let states = match query.state.as_deref() {
        Some(s) => parse_states_query(s).map_err(|e| bad_request_response(&e))?,
        None => Vec::new(),
    };

    let user = query.user.as_deref();
    let partition = query.partition.as_deref();
    let account = query.account.as_deref();

    let jobs = state
        .cluster
        .get_jobs(&states, user, partition, account, &[]);
    let json_jobs: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();

    Ok(ApiResponse::ok(JobsData { jobs: json_jobs }))
}

pub async fn get_job(
    State(state): State<Arc<RestState>>,
    Path(job_id): Path<u32>,
) -> Result<Json<ApiResponse<JobsData>>, RestError> {
    let job = state
        .cluster
        .get_job_for_display(job_id)
        .ok_or_else(|| not_found_response(&format!("job {job_id} not found")))?;

    Ok(ApiResponse::ok(JobsData {
        jobs: vec![job_to_json(&job)],
    }))
}

pub async fn submit_job(
    State(state): State<Arc<RestState>>,
    Json(body): Json<SubmitRequest>,
) -> Result<Json<ApiResponse<SubmitResponse>>, RestError> {
    if !state.raft.is_leader() {
        return Err(unavailable_response("not the Raft leader"));
    }

    let time_limit = body
        .job
        .time_limit
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_minutes(t))
        .map(|mins| chrono::Duration::minutes(mins as i64));

    let spec = spur_core::job::JobSpec {
        name: body.job.name.unwrap_or_default(),
        partition: body.job.partition,
        account: body.job.account,
        num_nodes: body.job.nodes.unwrap_or(1),
        num_tasks: body.job.ntasks.unwrap_or(1),
        cpus_per_task: body.job.cpus_per_task.unwrap_or(1),
        time_limit,
        script: body.job.script,
        environment: body.job.environment,
        ..Default::default()
    };

    let job_id = state.cluster.submit_job(spec).map_err(submit_rest_error)?;

    Ok(ApiResponse::ok(SubmitResponse { job_id }))
}

fn submit_rest_error(err: crate::cluster::SubmitError) -> RestError {
    match err {
        crate::cluster::SubmitError::InvalidArgument(m) => bad_request_response(&m),
        crate::cluster::SubmitError::Internal(m) => error_response(&format!("submit failed: {m}")),
    }
}

pub async fn cancel_job(
    State(state): State<Arc<RestState>>,
    Path(job_id): Path<u32>,
) -> Result<Json<ApiResponse<serde_json::Value>>, RestError> {
    if !state.raft.is_leader() {
        return Err(unavailable_response("not the Raft leader"));
    }

    let job = state.cluster.get_job(job_id);

    state
        .cluster
        .cancel_job(job_id, "")
        .map_err(|e| error_response(&format!("cancel failed: {e}")))?;

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
    let json_nodes: Vec<serde_json::Value> = nodes.iter().map(node_to_json).collect();

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

    Ok(ApiResponse::ok(NodesData {
        nodes: vec![node_to_json(&node)],
    }))
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
