// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! REST API server for spurctld (Slurm-compatible HTTP, default port 6820).

mod convert;
mod handlers;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

pub struct RestState {
    pub cluster: Arc<ClusterManager>,
    pub raft: Arc<RaftHandle>,
}

pub(crate) fn routes() -> Router<Arc<RestState>> {
    Router::new()
        .route("/ping", get(handlers::ping))
        .route("/jobs", get(handlers::get_jobs))
        .route("/jobs/", get(handlers::get_jobs))
        .route("/job/submit", post(handlers::submit_job))
        .route("/job/{job_id}", get(handlers::get_job))
        .route("/job/{job_id}", delete(handlers::cancel_job))
        .route("/nodes", get(handlers::get_nodes))
        .route("/nodes/", get(handlers::get_nodes))
        .route("/node/{name}", get(handlers::get_node))
        .route("/partitions", get(handlers::get_partitions))
        .route("/partitions/", get(handlers::get_partitions))
}

/// Start the REST API server. Runs until the listener is closed.
pub async fn serve(
    listen: SocketAddr,
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
) -> anyhow::Result<()> {
    let state = Arc::new(RestState { cluster, raft });

    let app = Router::new()
        .nest("/api/v1", routes())
        .nest("/slurm/v0.0.42", routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    info!(%bound, "REST API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
