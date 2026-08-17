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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use spur_core::auth::generate_token;
    use spur_core::config::AuthMode;
    use tower::ServiceExt;

    const KEY: &str = "test-signing-key";

    fn valid_token() -> String {
        generate_token("alice", 1000, false, KEY.as_bytes(), 3600).unwrap()
    }

    /// Build a minimal Router with rest_auth wired, backed by a trivial handler that always 200s.
    fn app(mode: AuthMode, key: &str) -> Router {
        let jwt_key = key.as_bytes().to_vec();
        Router::new()
            .route("/api/v1/jobs", axum::routing::get(|| async { "ok" }))
            .route("/api/v1/ping", axum::routing::get(|| async { "pong" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                let k = jwt_key.clone();
                async move { rest_auth(mode, k, req, next).await }
            }))
    }

    async fn status(app: Router, req: Request<Body>) -> StatusCode {
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn ping_is_always_exempt() {
        for mode in [AuthMode::Required, AuthMode::Permissive, AuthMode::Disabled] {
            let s = status(
                app(mode, KEY),
                Request::get("/api/v1/ping").body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(
                s,
                StatusCode::OK,
                "{mode:?}: /ping must not require a token"
            );
        }
    }

    #[tokio::test]
    async fn required_rejects_missing_credential() {
        let s = status(
            app(AuthMode::Required, KEY),
            Request::get("/api/v1/jobs").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn permissive_allows_missing_credential() {
        let s = status(
            app(AuthMode::Permissive, KEY),
            Request::get("/api/v1/jobs").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_token_passes_in_required_mode() {
        let s = status(
            app(AuthMode::Required, KEY),
            Request::get("/api/v1/jobs")
                .header("authorization", format!("Bearer {}", valid_token()))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_rejected_even_in_permissive_mode() {
        let forged = generate_token("eve", 0, true, "wrong-key".as_bytes(), 3600).unwrap();
        let s = status(
            app(AuthMode::Permissive, KEY),
            Request::get("/api/v1/jobs")
                .header("authorization", format!("Bearer {forged}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_header_is_rejected() {
        for bad in ["Basic abc", "Bearer", "Bearer   "] {
            let s = status(
                app(AuthMode::Permissive, KEY),
                Request::get("/api/v1/jobs")
                    .header("authorization", bad)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(
                s,
                StatusCode::UNAUTHORIZED,
                "header {bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn disabled_mode_skips_verification_entirely() {
        // Even a forged token passes when auth is disabled.
        let forged = generate_token("eve", 0, true, "wrong-key".as_bytes(), 3600).unwrap();
        let s = status(
            app(AuthMode::Disabled, KEY),
            Request::get("/api/v1/jobs")
                .header("authorization", format!("Bearer {forged}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
}
