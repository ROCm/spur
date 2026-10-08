// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Authenticates callers of the operator's cluster-wide-pod-create agent surface via the shared
//! `spur_core::auth::authenticate_bearer` (mirrors spurd's `AgentAuthLayer`, duplicated since spurd is a binary crate).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tower::{Layer, Service};
use tracing::warn;

use spur_core::auth::{BearerAuth, BearerOutcome};
use spur_core::config::AuthMode;

/// Caller address for a request, `None` when the transport did not record one.
fn peer_addr(extensions: &http::Extensions) -> Option<String> {
    extensions
        .get::<tonic::transport::server::TcpConnectInfo>()
        .and_then(|info| info.remote_addr())
        .map(spur_core::peer::canonical_peer)
}

#[derive(Clone)]
pub struct AgentAuthLayer {
    inner: Arc<BearerAuth>,
}

impl AgentAuthLayer {
    pub fn from_bearer(auth: BearerAuth) -> Self {
        Self {
            inner: Arc::new(auth),
        }
    }
}

impl<S> Layer<S> for AgentAuthLayer {
    type Service = AgentAuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AgentAuthMiddleware {
            inner,
            config: self.inner.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AgentAuthMiddleware<S> {
    inner: S,
    config: Arc<BearerAuth>,
}

fn decide(config: &BearerAuth, header: Option<&str>) -> BearerOutcome {
    config.authenticate(
        header,
        "the k8s operator only accepts agent calls carrying the cluster credential",
    )
}

impl<S, B> Service<Request<B>> for AgentAuthMiddleware<S>
where
    S: Service<Request<B>, Response = Response<tonic::body::Body>> + Clone + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<tonic::body::Body>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        if spur_core::auth::is_unauthenticated_auth_handshake(req.uri().path()) {
            let mut inner = self.inner.clone();
            return Box::pin(async move { inner.call(req).await.map_err(Into::into) });
        }
        let header = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        match decide(&self.config, header.as_deref()) {
            // The operator does not act *as* the caller — it creates what the controller allocated —
            // so the identity is not carried into handlers; verifying the credential is the point.
            BearerOutcome::Authenticated(_) => {}
            BearerOutcome::Anonymous => {
                if self.config.mode == AuthMode::Permissive {
                    warn!(
                        path = %req.uri().path(),
                        "unauthenticated operator agent request accepted (auth.mode = permissive): \
                         any peer that can reach this port can ask the operator to create a pod"
                    );
                }
            }
            BearerOutcome::Reject(msg) => {
                // A forged credential aimed at pod creation is worth recording on
                // every cluster, so this is not gated behind any debug setting.
                warn!(
                    path = %req.uri().path(),
                    peer = peer_addr(req.extensions()).as_deref().unwrap_or("-"),
                    reason = %msg,
                    "rejected an operator RPC with an invalid credential"
                );
                let resp = tonic::Status::unauthenticated(msg).into_http();
                return Box::pin(async move { Ok(resp) });
            }
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await.map_err(Into::into) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::auth::generate_token;

    fn cfg(mode: AuthMode, key: &str) -> BearerAuth {
        BearerAuth::jwt(mode, key.as_bytes())
    }

    fn controller_token(key: &str) -> String {
        generate_token("spurctld", 0, true, key.as_bytes(), 300).unwrap()
    }

    /// Guards this copy of `peer_addr`: a dual-stack listener hands us the
    /// IPv4-mapped form, and every daemon must log the plain IPv4 form.
    #[test]
    fn peer_addr_unwraps_an_ipv4_mapped_client() {
        let mut ext = http::Extensions::new();
        ext.insert(tonic::transport::server::TcpConnectInfo {
            local_addr: None,
            remote_addr: Some("[::ffff:10.0.0.4]:51234".parse().unwrap()),
        });
        assert_eq!(peer_addr(&ext).as_deref(), Some("10.0.0.4:51234"));

        // No connection info at all (a non-TCP or test transport) is not an error.
        assert_eq!(peer_addr(&http::Extensions::new()), None);
    }

    #[test]
    fn required_refuses_an_uncredentialed_caller() {
        assert!(matches!(
            decide(&cfg(AuthMode::Required, "k"), None),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn permissive_still_accepts_an_uncredentialed_caller() {
        // Migration window: controllers start presenting a credential before agents demand one.
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "k"), None),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn a_controller_credential_is_accepted() {
        let header = format!("Bearer {}", controller_token("cluster-key"));
        assert!(matches!(
            decide(&cfg(AuthMode::Required, "cluster-key"), Some(&header)),
            BearerOutcome::Authenticated(_)
        ));
    }

    #[test]
    fn a_credential_signed_with_another_key_is_refused_even_in_permissive() {
        let forged = format!("Bearer {}", controller_token("attacker-key"));
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "cluster-key"), Some(&forged)),
            BearerOutcome::Reject(_)
        ));
    }

    // Exercises the real `Layer`/`Service` wiring (not just `decide()`), so a misplaced or
    // no-op `.layer(...)` call in main.rs would fail this rather than only the unit tests above.
    #[derive(Clone)]
    struct StubInner;

    impl Service<Request<()>> for StubInner {
        type Response = Response<tonic::body::Body>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<()>) -> Self::Future {
            std::future::ready(Ok(Response::new(tonic::body::Body::default())))
        }
    }

    #[tokio::test]
    async fn required_mode_rejects_an_uncredentialed_call_through_the_real_layer() {
        let mut svc =
            AgentAuthLayer::from_bearer(BearerAuth::jwt(AuthMode::Required, b"k")).layer(StubInner);
        let resp = svc.call(Request::new(())).await.unwrap();
        assert_eq!(resp.headers().get("grpc-status").unwrap(), "16");
    }

    #[tokio::test]
    async fn required_mode_forwards_a_valid_credential_through_the_real_layer() {
        let mut svc =
            AgentAuthLayer::from_bearer(BearerAuth::jwt(AuthMode::Required, b"cluster-key"))
                .layer(StubInner);
        let req = Request::builder()
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", controller_token("cluster-key")),
            )
            .body(())
            .unwrap();
        let resp = svc.call(req).await.unwrap();
        assert!(resp.headers().get("grpc-status").is_none());
    }

    /// Counts calls so a test can assert the inner service was never reached.
    #[derive(Clone, Default)]
    struct CountingInner(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Service<Request<()>> for CountingInner {
        type Response = Response<tonic::body::Body>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<()>) -> Self::Future {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(Response::new(tonic::body::Body::default())))
        }
    }

    /// A forged credential is refused here, so pod creation is never reached.
    /// That short-circuit is also why the refusal is logged in this module.
    #[tokio::test]
    async fn a_forged_credential_never_reaches_the_inner_service() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut svc =
            AgentAuthLayer::from_bearer(BearerAuth::jwt(AuthMode::Required, b"cluster-key"))
                .layer(CountingInner(calls.clone()));
        let req = Request::builder()
            .header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", controller_token("attacker-key")),
            )
            .body(())
            .unwrap();

        let resp = svc.call(req).await.unwrap();

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a forged credential must not reach the operator's agent surface"
        );
        let status = tonic::Status::from_header_map(resp.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }
}
