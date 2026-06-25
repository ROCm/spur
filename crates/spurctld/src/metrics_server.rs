// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics 1.0 HTTP export for spurctld (default port 6822).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use spur_metrics::{
    encode_job_metrics, encode_nodes_metrics, encode_partitions_metrics, encode_scheduler_metrics,
    CONTENT_TYPE,
};
use tracing::info;

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

struct MetricsState {
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
}

/// Start the metrics HTTP server. Runs until the listener is closed.
pub async fn serve(
    listen: SocketAddr,
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
) -> anyhow::Result<()> {
    let state = Arc::new(MetricsState { cluster, raft });

    let app = Router::new()
        .route("/metrics", get(metrics_jobs))
        .route("/metrics/jobs", get(metrics_jobs))
        .route("/metrics/nodes", get(metrics_nodes))
        .route("/metrics/partitions", get(metrics_partitions))
        .route("/metrics/scheduler", get(metrics_scheduler))
        .route("/metrics/jobs-users-accts", get(metrics_jobs_users_accts))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    info!(%bound, "metrics HTTP server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_jobs(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    metrics_response(encode_job_metrics(&state.cluster.job_metrics()))
}

async fn metrics_nodes(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    metrics_response(encode_nodes_metrics(&state.cluster.node_metrics()))
}

async fn metrics_partitions(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    metrics_response(encode_partitions_metrics())
}

async fn metrics_scheduler(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    metrics_response(encode_scheduler_metrics())
}

async fn metrics_jobs_users_accts(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.cluster.config.metrics.high_cardinality {
        return (
            StatusCode::NOT_FOUND,
            "jobs-users-accts metrics disabled (set metrics.high_cardinality = true)",
        )
            .into_response();
    }
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    (
        StatusCode::NOT_FOUND,
        "jobs-users-accts metrics deferred to a follow-up PR",
    )
        .into_response()
}

fn not_leader_response() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "not the Raft leader").into_response()
}

fn metrics_response(body: String) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response()
}

/// Leader-gated metrics response (testable without a live Raft node).
#[cfg(test)]
fn leader_metrics_response(is_leader: bool, body: String) -> Response {
    if !is_leader {
        return not_leader_response();
    }
    metrics_response(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_metrics::job::JobMetricsSnapshot;
    use spur_metrics::node::NodeMetricsSnapshot;

    #[test]
    fn leader_returns_openmetrics_content_type() {
        let body = encode_job_metrics(&JobMetricsSnapshot::default());
        let response = leader_metrics_response(true, body);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            CONTENT_TYPE
        );
    }

    #[test]
    fn nodes_endpoint_returns_200_on_leader() {
        let response =
            leader_metrics_response(true, encode_nodes_metrics(&NodeMetricsSnapshot::default()));
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn follower_returns_503() {
        let response = leader_metrics_response(false, String::new());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
