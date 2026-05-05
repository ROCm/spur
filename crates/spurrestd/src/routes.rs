use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;

use crate::AppState;

/// Spur REST API response envelope.
#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    meta: ApiMeta,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ApiError>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(flatten)]
    data: T,
}

#[derive(Serialize)]
struct ApiMeta {
    #[serde(rename = "Slurm")]
    slurm: ApiVersion,
}

#[derive(Serialize)]
struct ApiVersion {
    version: ApiVersionInfo,
    release: String,
}

#[derive(Serialize)]
struct ApiVersionInfo {
    major: u32,
    minor: u32,
    micro: u32,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_number: Option<i32>,
}

fn meta() -> ApiMeta {
    ApiMeta {
        slurm: ApiVersion {
            version: ApiVersionInfo {
                major: 0,
                minor: 0,
                micro: 42,
            },
            release: "spur 0.1.0".into(),
        },
    }
}

impl<T: Serialize> ApiResponse<T> {
    fn ok(data: T) -> Json<Self> {
        Json(Self {
            meta: meta(),
            errors: Vec::new(),
            warnings: Vec::new(),
            data,
        })
    }
}

fn error_response(msg: &str) -> (StatusCode, Json<ApiResponse<serde_json::Value>>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiResponse {
            meta: meta(),
            errors: vec![ApiError {
                error: msg.to_string(),
                error_number: None,
            }],
            warnings: Vec::new(),
            data: serde_json::json!({}),
        }),
    )
}

pub fn spur_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ping", get(ping))
        .route("/jobs", get(get_jobs))
        .route("/jobs/", get(get_jobs))
        .route("/job/submit", post(submit_job))
        .route("/job/{job_id}", get(get_job))
        .route("/job/{job_id}", delete(cancel_job))
        .route("/nodes", get(get_nodes))
        .route("/nodes/", get(get_nodes))
        .route("/node/{name}", get(get_node))
        .route("/partitions", get(get_partitions))
        .route("/partitions/", get(get_partitions))
}

// ---- Handlers ----

async fn ping(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<PingData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let resp = client
        .ping(())
        .await
        .map_err(|e| error_response(&format!("ping failed: {}", e)))?;

    let inner = resp.into_inner();
    Ok(ApiResponse::ok(PingData {
        ping: vec![PingInfo {
            hostname: inner.hostname,
            pinged: "UP".into(),
            latency: 0,
            mode: "primary".into(),
        }],
    }))
}

#[derive(Serialize)]
struct PingData {
    ping: Vec<PingInfo>,
}

#[derive(Serialize)]
struct PingInfo {
    hostname: String,
    pinged: String,
    latency: u64,
    mode: String,
}

#[derive(Deserialize)]
struct JobsQuery {
    user: Option<String>,
    partition: Option<String>,
    state: Option<String>,
    account: Option<String>,
}

async fn get_jobs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<JobsQuery>,
) -> Result<Json<ApiResponse<JobsData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let states: Vec<i32> = query
        .state
        .iter()
        .flat_map(|s| s.split(','))
        .filter_map(|s| parse_job_state(s.trim()))
        .map(|s| s as i32)
        .collect();

    let resp = client
        .get_jobs(spur_proto::proto::GetJobsRequest {
            states,
            user: query.user.unwrap_or_default(),
            partition: query.partition.unwrap_or_default(),
            account: query.account.unwrap_or_default(),
            job_ids: Vec::new(),
        })
        .await
        .map_err(|e| error_response(&format!("get_jobs failed: {}", e)))?;

    let jobs: Vec<serde_json::Value> = resp
        .into_inner()
        .jobs
        .iter()
        .map(|j| job_info_to_json(j))
        .collect();

    Ok(ApiResponse::ok(JobsData { jobs }))
}

#[derive(Serialize)]
struct JobsData {
    jobs: Vec<serde_json::Value>,
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<u32>,
) -> Result<Json<ApiResponse<JobsData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let resp = client
        .get_job(spur_proto::proto::GetJobRequest { job_id })
        .await
        .map_err(|e| error_response(&format!("get_job failed: {}", e)))?;

    let job = resp.into_inner();
    Ok(ApiResponse::ok(JobsData {
        jobs: vec![job_info_to_json(&job)],
    }))
}

#[derive(Deserialize)]
struct SubmitRequest {
    job: SubmitJobFields,
}

#[derive(Deserialize)]
struct SubmitJobFields {
    name: Option<String>,
    partition: Option<String>,
    account: Option<String>,
    nodes: Option<u32>,
    ntasks: Option<u32>,
    cpus_per_task: Option<u32>,
    time_limit: Option<String>,
    script: Option<String>,
    #[serde(default)]
    environment: std::collections::HashMap<String, String>,
}

async fn submit_job(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SubmitRequest>,
) -> Result<Json<ApiResponse<SubmitResponse>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let time_limit = body
        .job
        .time_limit
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_minutes(t))
        .map(|mins| prost_types::Duration {
            seconds: mins as i64 * 60,
            nanos: 0,
        });

    let resp = client
        .submit_job(spur_proto::proto::SubmitJobRequest {
            spec: Some(spur_proto::proto::JobSpec {
                name: body.job.name.unwrap_or_default(),
                partition: body.job.partition.unwrap_or_default(),
                account: body.job.account.unwrap_or_default(),
                num_nodes: body.job.nodes.unwrap_or(1),
                num_tasks: body.job.ntasks.unwrap_or(1),
                cpus_per_task: body.job.cpus_per_task.unwrap_or(1),
                time_limit,
                script: body.job.script.unwrap_or_default(),
                environment: body.job.environment,
                ..Default::default()
            }),
        })
        .await
        .map_err(|e| error_response(&format!("submit failed: {}", e)))?;

    Ok(ApiResponse::ok(SubmitResponse {
        job_id: resp.into_inner().job_id,
    }))
}

#[derive(Serialize)]
struct SubmitResponse {
    job_id: u32,
}

async fn cancel_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<u32>,
) -> Result<Json<ApiResponse<serde_json::Value>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)>
{
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    client
        .cancel_job(spur_proto::proto::CancelJobRequest {
            job_id,
            signal: 0,
            user: String::new(),
        })
        .await
        .map_err(|e| error_response(&format!("cancel failed: {}", e)))?;

    Ok(ApiResponse::ok(serde_json::json!({})))
}

async fn get_nodes(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<NodesData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let resp = client
        .get_nodes(spur_proto::proto::GetNodesRequest {
            states: Vec::new(),
            partition: String::new(),
            nodelist: String::new(),
        })
        .await
        .map_err(|e| error_response(&format!("get_nodes failed: {}", e)))?;

    let nodes: Vec<serde_json::Value> = resp
        .into_inner()
        .nodes
        .iter()
        .map(|n| node_info_to_json(n))
        .collect();

    Ok(ApiResponse::ok(NodesData { nodes }))
}

#[derive(Serialize)]
struct NodesData {
    nodes: Vec<serde_json::Value>,
}

async fn get_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ApiResponse<NodesData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let resp = client
        .get_node(spur_proto::proto::GetNodeRequest { name })
        .await
        .map_err(|e| error_response(&format!("get_node failed: {}", e)))?;

    let node = resp.into_inner();
    Ok(ApiResponse::ok(NodesData {
        nodes: vec![node_info_to_json(&node)],
    }))
}

async fn get_partitions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<PartitionsData>>, (StatusCode, Json<ApiResponse<serde_json::Value>>)> {
    let mut client = SlurmControllerClient::connect(state.controller_addr.clone())
        .await
        .map_err(|e| error_response(&format!("connection failed: {}", e)))?;

    let resp = client
        .get_partitions(spur_proto::proto::GetPartitionsRequest {
            name: String::new(),
        })
        .await
        .map_err(|e| error_response(&format!("get_partitions failed: {}", e)))?;

    let partitions: Vec<serde_json::Value> = resp
        .into_inner()
        .partitions
        .iter()
        .map(|p| partition_info_to_json(p))
        .collect();

    Ok(ApiResponse::ok(PartitionsData { partitions }))
}

#[derive(Serialize)]
struct PartitionsData {
    partitions: Vec<serde_json::Value>,
}

// ---- JSON converters (match Spur REST API field names) ----

fn job_info_to_json(j: &spur_proto::proto::JobInfo) -> serde_json::Value {
    serde_json::json!({
        "job_id": j.job_id,
        "name": j.name,
        "user_name": j.user,
        "user_id": j.uid,
        "partition": j.partition,
        "account": j.account,
        "job_state": state_name(j.state),
        "state_reason": j.state_reason,
        "submit_time": j.submit_time.as_ref().map(|t| t.seconds),
        "start_time": j.start_time.as_ref().map(|t| t.seconds),
        "end_time": j.end_time.as_ref().map(|t| t.seconds),
        "time_limit": j.time_limit.as_ref().map(|t| t.seconds / 60),
        "node_count": j.num_nodes,
        "tasks": j.num_tasks,
        "cpus_per_task": j.cpus_per_task,
        "nodes": j.nodelist,
        "current_working_directory": j.work_dir,
        "command": j.command,
        "exit_code": j.exit_code,
        "standard_output": j.stdout_path,
        "standard_error": j.stderr_path,
        "priority": j.priority,
        "qos": j.qos,
    })
}

fn node_info_to_json(n: &spur_proto::proto::NodeInfo) -> serde_json::Value {
    serde_json::json!({
        "name": n.name,
        "state": node_state_name(n.state),
        "reason": n.state_reason,
        "partitions": [n.partition],
        "cpus": n.total_resources.as_ref().map(|r| r.cpus).unwrap_or(0),
        "alloc_cpus": n.alloc_resources.as_ref().map(|r| r.cpus).unwrap_or(0),
        "real_memory": n.total_resources.as_ref().map(|r| r.memory_mb).unwrap_or(0),
        "free_mem": n.free_memory_mb,
        "cpu_load": n.cpu_load,
        "architecture": n.arch,
        "operating_system": n.os,
    })
}

fn partition_info_to_json(p: &spur_proto::proto::PartitionInfo) -> serde_json::Value {
    serde_json::json!({
        "name": p.name,
        "state": p.state,
        "is_default": p.is_default,
        "total_nodes": p.total_nodes,
        "total_cpus": p.total_cpus,
        "nodes": p.nodes,
        "max_time": p.max_time.as_ref().map(|t| t.seconds / 60),
        "default_time": p.default_time.as_ref().map(|t| t.seconds / 60),
        "priority_tier": p.priority_tier,
    })
}

fn state_name(state: i32) -> &'static str {
    match state {
        0 => "PENDING",
        1 => "RUNNING",
        2 => "COMPLETING",
        3 => "COMPLETED",
        4 => "FAILED",
        5 => "CANCELLED",
        6 => "TIMEOUT",
        7 => "NODE_FAIL",
        8 => "PREEMPTED",
        9 => "SUSPENDED",
        _ => "UNKNOWN",
    }
}

fn node_state_name(state: i32) -> &'static str {
    match state {
        0 => "idle",
        1 => "allocated",
        2 => "mixed",
        3 => "down",
        4 => "drained",
        5 => "draining",
        6 => "error",
        _ => "unknown",
    }
}

fn parse_job_state(s: &str) -> Option<spur_proto::proto::JobState> {
    use spur_proto::proto::JobState;
    match s.to_uppercase().as_str() {
        "PD" | "PENDING" => Some(JobState::JobPending),
        "R" | "RUNNING" => Some(JobState::JobRunning),
        "CG" | "COMPLETING" => Some(JobState::JobCompleting),
        "CD" | "COMPLETED" => Some(JobState::JobCompleted),
        "F" | "FAILED" => Some(JobState::JobFailed),
        "CA" | "CANCELLED" => Some(JobState::JobCancelled),
        "TO" | "TIMEOUT" => Some(JobState::JobTimeout),
        "NF" | "NODE_FAIL" => Some(JobState::JobNodeFail),
        "PR" | "PREEMPTED" => Some(JobState::JobPreempted),
        "S" | "SUSPENDED" => Some(JobState::JobSuspended),
        _ => None,
    }
}
