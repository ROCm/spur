// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Prometheus/OpenMetrics HTTP export for spurctld (default port 6822).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use spur_core::config::MetricsExpositionFormat;
use spur_metrics::{
    encode_job_metrics_with_format, encode_nodes_metrics, encode_partitions_metrics,
    encode_scheduler_metrics,
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
    let format = state.cluster.config.metrics.exposition_format;
    metrics_exposition_response(
        format,
        encode_job_metrics_with_format(&state.cluster.job_metrics(), format),
    )
}

async fn metrics_nodes(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    let format = state.cluster.config.metrics.exposition_format;
    metrics_exposition_response(format, encode_nodes_metrics(format))
}

async fn metrics_partitions(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    let format = state.cluster.config.metrics.exposition_format;
    metrics_exposition_response(format, encode_partitions_metrics(format))
}

async fn metrics_scheduler(State(state): State<Arc<MetricsState>>) -> Response {
    if !state.raft.is_leader() {
        return not_leader_response();
    }
    let format = state.cluster.config.metrics.exposition_format;
    metrics_exposition_response(format, encode_scheduler_metrics(format))
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

fn metrics_exposition_response(format: MetricsExpositionFormat, body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, format.content_type())],
        body,
    )
        .into_response()
}

/// Leader-gated metrics response (testable without a live Raft node).
#[cfg(test)]
fn leader_metrics_response(
    is_leader: bool,
    format: MetricsExpositionFormat,
    body: String,
) -> Response {
    if !is_leader {
        return not_leader_response();
    }
    metrics_exposition_response(format, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_metrics::job::JobMetricsSnapshot;

    #[test]
    fn leader_returns_slurm_content_type() {
        let format = MetricsExpositionFormat::Slurm_0_0_4;
        let body = encode_job_metrics_with_format(&JobMetricsSnapshot::default(), format);
        let response = leader_metrics_response(true, format, body);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            format.content_type()
        );
    }

    #[test]
    fn leader_returns_openmetrics_content_type() {
        let format = MetricsExpositionFormat::OpenMetrics_1_0;
        let body = encode_job_metrics_with_format(&JobMetricsSnapshot::default(), format);
        let response = leader_metrics_response(true, format, body);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/openmetrics-text; version=1.0.0; charset=utf-8"
        );
    }

    #[test]
    fn stub_nodes_endpoint_returns_200_on_leader() {
        let format = MetricsExpositionFormat::Slurm_0_0_4;
        let response = leader_metrics_response(true, format, encode_nodes_metrics(format));
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn follower_returns_503() {
        let response =
            leader_metrics_response(false, MetricsExpositionFormat::Slurm_0_0_4, String::new());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
