// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tower middleware that authenticates controller RPC callers.
//!
//! This is the single verification point for the control plane. It runs as a Tower layer rather
//! than a per-service tonic interceptor deliberately: the layer wraps everything served on the port,
//! so the accounting service — which has no authorization of its own, and whose `add_user` takes an
//! `admin_level` — is covered by the same gate as the controller.
//!
//! On success a verified [`Identity`] is inserted into the request extensions; handlers read it
//! instead of trusting a client-supplied `user`/`caller` field. Nothing else in the pipeline may
//! insert an `Identity`, so a handler that finds one knows it was verified here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tower::{Layer, Service};
use tracing::warn;

use spur_core::auth::BearerOutcome;
use spur_core::config::AuthMode;

/// Marker inserted alongside the identity so handlers can tell "verified" from "asserted" without
/// re-reading config. Absent in `disabled` mode and for unauthenticated calls under `permissive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified;

#[derive(Clone)]
pub struct AuthLayer {
    inner: Arc<AuthConfig>,
}

struct AuthConfig {
    mode: AuthMode,
    /// HS256 key. Empty disables verification regardless of mode (startup refuses that combination
    /// for `required`, so an empty key here can only mean `disabled`/`permissive`).
    jwt_key: Vec<u8>,
}

impl AuthLayer {
    pub fn new(mode: AuthMode, jwt_key: &str) -> Self {
        Self {
            inner: Arc::new(AuthConfig {
                mode,
                jwt_key: jwt_key.as_bytes().to_vec(),
            }),
        }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            config: self.inner.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    inner: S,
    config: Arc<AuthConfig>,
}

/// The ruling itself lives in `spur_core::auth` so the controller and the agent cannot drift apart
/// on a security decision; this module only supplies the Tower plumbing.
fn decide(config: &AuthConfig, header: Option<&str>) -> BearerOutcome {
    spur_core::auth::authenticate_bearer(
        config.mode,
        &config.jwt_key,
        header,
        "pass a token (see `spur token user`)",
    )
}

impl<S, B> Service<Request<B>> for AuthMiddleware<S>
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

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let config = self.config.clone();
        let header = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        match decide(&config, header.as_deref()) {
            BearerOutcome::Authenticated(identity) => {
                req.extensions_mut().insert(*identity);
                req.extensions_mut().insert(Verified);
            }
            BearerOutcome::Anonymous => {
                if config.mode == AuthMode::Permissive {
                    // Name the caller so an operator rolling out credentials can see exactly who is
                    // still unauthenticated instead of guessing.
                    warn!(
                        path = %req.uri().path(),
                        "unauthenticated request accepted (auth.mode = permissive); \
                         the caller's asserted identity is being trusted"
                    );
                }
            }
            BearerOutcome::Reject(msg) => {
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

    fn cfg(mode: AuthMode, key: &str) -> AuthConfig {
        AuthConfig {
            mode,
            jwt_key: key.as_bytes().to_vec(),
        }
    }

    fn token(key: &str) -> String {
        generate_token("alice", 1000, false, key.as_bytes(), 3600).unwrap()
    }

    #[test]
    fn required_rejects_a_missing_credential() {
        assert!(matches!(
            decide(&cfg(AuthMode::Required, "k"), None),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn permissive_allows_a_missing_credential() {
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "k"), None),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn a_valid_token_authenticates_and_carries_the_subject() {
        let t = token("k");
        let header = format!("Bearer {t}");
        match decide(&cfg(AuthMode::Required, "k"), Some(&header)) {
            BearerOutcome::Authenticated(id) => {
                assert_eq!(id.user, "alice");
                assert_eq!(id.uid, 1000);
                assert!(!id.is_admin);
            }
            _ => panic!("valid token must authenticate"),
        }
    }

    #[test]
    fn permissive_still_rejects_an_invalid_token() {
        // Permissive tolerates the absence of a credential, never a bad one — otherwise forging a
        // token would be strictly better for an attacker than sending none.
        let forged = format!("Bearer {}", token("attacker-key"));
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "real-key"), Some(&forged)),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn a_malformed_header_is_rejected_not_downgraded() {
        for h in ["", "Basic abc", "Bearer", "Bearer    "] {
            assert!(
                matches!(
                    decide(&cfg(AuthMode::Permissive, "k"), Some(h)),
                    BearerOutcome::Reject(_)
                ),
                "header {h:?} must be rejected"
            );
        }
    }

    #[test]
    fn disabled_ignores_even_a_valid_token() {
        let t = token("k");
        let header = format!("Bearer {t}");
        assert!(matches!(
            decide(&cfg(AuthMode::Disabled, "k"), Some(&header)),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn a_token_without_a_configured_key_is_rejected() {
        let header = format!("Bearer {}", token("k"));
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, ""), Some(&header)),
            BearerOutcome::Reject(_)
        ));
    }
}
