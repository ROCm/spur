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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use axum::body::Body;
    use http_body_util::BodyExt;
    use spur_core::config::SlurmConfig;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::cluster::ClusterManager;

    fn test_config() -> SlurmConfig {
        SlurmConfig {
            cluster_name: "test".into(),
            controller: spur_core::config::ControllerConfig {
                first_job_id: 1,
                ..Default::default()
            },
            accounting: Default::default(),
            scheduler: Default::default(),
            auth: Default::default(),
            partitions: vec![spur_core::config::PartitionConfig {
                name: "default".into(),
                default: true,
                state: "UP".into(),
                nodes: "ALL".into(),
                selector: Default::default(),
                max_time: None,
                default_time: None,
                max_nodes: None,
                min_nodes: 1,
                allow_accounts: Vec::new(),
                allow_groups: Vec::new(),
                priority_tier: 1,
                preempt_mode: String::new(),
            }],
            nodes: Vec::new(),
            network: Default::default(),
            logging: Default::default(),
            kubernetes: Default::default(),
            notifications: Default::default(),
            power: Default::default(),
            federation: Default::default(),
            topology: None,
            isolation: Default::default(),
            licenses: HashMap::new(),
            update: Default::default(),
            metrics: Default::default(),
            rest_api: Default::default(),
            hooks: Default::default(),
            devices: Default::default(),
            admission: Default::default(),
            burst_buffer: Default::default(),
        }
    }

    async fn test_app() -> (Router, TempDir) {
        let dir = TempDir::new().unwrap();
        let cm = Arc::new(ClusterManager::new(test_config(), dir.path()).unwrap());
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cm.clone())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .expect("single-node raft did not self-elect within 5s");
        let state = Arc::new(MetricsState {
            cluster: cm,
            raft: Arc::new(handle),
        });
        let app = Router::new()
            .route("/metrics/jobs", get(metrics_jobs))
            .route("/metrics/nodes", get(metrics_nodes))
            .route("/metrics/partitions", get(metrics_partitions))
            .route("/metrics/scheduler", get(metrics_scheduler))
            .route("/metrics/jobs-users-accts", get(metrics_jobs_users_accts))
            .with_state(state);
        (app, dir)
    }

    #[tokio::test]
    async fn metrics_jobs_returns_ok() {
        let (app, _dir) = test_app().await;
        let resp = app
            .oneshot(
                axum::http::Request::get("/metrics/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            CONTENT_TYPE
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("spur_jobs"));
    }

    #[tokio::test]
    async fn metrics_nodes_returns_ok() {
        let (app, _dir) = test_app().await;
        let resp = app
            .oneshot(
                axum::http::Request::get("/metrics/nodes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            CONTENT_TYPE
        );
    }

    #[tokio::test]
    async fn metrics_partitions_returns_ok() {
        let (app, _dir) = test_app().await;
        let resp = app
            .oneshot(
                axum::http::Request::get("/metrics/partitions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            CONTENT_TYPE
        );
    }

    #[tokio::test]
    async fn metrics_scheduler_returns_ok() {
        let (app, _dir) = test_app().await;
        let resp = app
            .oneshot(
                axum::http::Request::get("/metrics/scheduler")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            CONTENT_TYPE
        );
    }

    #[tokio::test]
    async fn metrics_jobs_users_accts_returns_404_when_disabled() {
        let (app, _dir) = test_app().await;
        let resp = app
            .oneshot(
                axum::http::Request::get("/metrics/jobs-users-accts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
