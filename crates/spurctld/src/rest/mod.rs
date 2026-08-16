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

fn routes() -> Router<Arc<RestState>> {
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

/// Authenticate a REST request, mirroring the gRPC policy in [`crate::auth_middleware`].
///
/// `/ping` is exempt so health checks keep working without a credential — it exposes no state.
async fn rest_auth(
    mode: spur_core::config::AuthMode,
    jwt_key: Vec<u8>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use spur_core::config::AuthMode;

    if req.uri().path().ends_with("/ping") || mode == AuthMode::Disabled {
        return next.run(req).await;
    }
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let deny = |msg: &str| (StatusCode::UNAUTHORIZED, msg.to_string()).into_response();
    use axum::response::IntoResponse;

    match header {
        None => {
            if mode == AuthMode::Required {
                return deny("authentication required: pass 'Authorization: Bearer <token>'");
            }
        }
        Some(h) => {
            let Some(token) = h
                .strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                return deny("malformed authorization header: expected 'Bearer <token>'");
            };
            if jwt_key.is_empty() {
                return deny("a token was presented but no auth.jwt_key is configured");
            }
            // As on the gRPC side, a bad credential is rejected even in permissive mode.
            if let Err(e) = spur_core::auth::verify_token(token, &jwt_key) {
                return deny(&format!("invalid credential: {e}"));
            }
        }
    }
    next.run(req).await
}

/// Start the REST API server. Runs until the listener is closed.
pub async fn serve(
    listen: SocketAddr,
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
) -> anyhow::Result<()> {
    // Same policy as gRPC: verify a presented credential, and under `required` refuse a request
    // without one. The REST surface has no per-user handling of its own, so this gate is what keeps
    // it from being a way around the authenticated gRPC path.
    let auth_mode = cluster.config().auth.mode;
    let jwt_key = cluster
        .config()
        .auth
        .jwt_key
        .clone()
        .unwrap_or_default()
        .into_bytes();
    let state = Arc::new(RestState { cluster, raft });

    let app = Router::new()
        .nest("/api/v1", routes())
        .nest("/slurm/v0.0.42", routes())
        .layer(axum::middleware::from_fn(move |req, next| {
            let key = jwt_key.clone();
            async move { rest_auth(auth_mode, key, req, next).await }
        }))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    info!(%bound, "REST API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
