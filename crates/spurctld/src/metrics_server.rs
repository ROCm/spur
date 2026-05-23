// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics HTTP export for spurctld (default port 6822).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use spur_metrics::encode_job_metrics;
use tracing::info;

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

/// OpenMetrics 1.0 text exposition (Slurm 25.11 compatible).
pub const OPENMETRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

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
        .route("/metrics/nodes", get(metrics_not_implemented))
        .route("/metrics/partitions", get(metrics_not_implemented))
        .route("/metrics/scheduler", get(metrics_not_implemented))
        .route("/metrics/jobs-users-accts", get(metrics_jobs_users_accts))
        .with_state(state);

    info!(%listen, "OpenMetrics metrics server listening");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_jobs(State(state): State<Arc<MetricsState>>) -> Response {
    let body = encode_job_metrics(&state.cluster.job_metrics());
    respond_job_metrics(state.raft.is_leader(), body)
}

async fn metrics_not_implemented() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "metric endpoint not implemented yet")
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
        "jobs-users-accts metrics not implemented yet",
    )
        .into_response()
}

fn not_leader_response() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "not the Raft leader").into_response()
}

fn openmetrics_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, OPENMETRICS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

/// Leader-gated job metrics response (testable without a live Raft node).
fn respond_job_metrics(is_leader: bool, body: String) -> Response {
    if !is_leader {
        return not_leader_response();
    }
    openmetrics_response(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_metrics::job::JobMetricsSnapshot;

    #[test]
    fn leader_returns_openmetrics_with_spur_jobs() {
        let body = encode_job_metrics(&JobMetricsSnapshot::default());
        let response = respond_job_metrics(true, body);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            OPENMETRICS_CONTENT_TYPE
        );
    }

    #[test]
    fn follower_returns_503() {
        let response = respond_job_metrics(false, String::new());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
