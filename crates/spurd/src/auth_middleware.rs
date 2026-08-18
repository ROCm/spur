// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tower middleware that authenticates callers of the agent's gRPC surface.
//!
//! Without this, reaching a node's agent port is enough to ask it to run work — the controller's
//! authentication can simply be stepped around. The agent therefore verifies that a request carries
//! a credential signed with the cluster's `[auth] jwt_key`, which the controller attaches to every
//! agent call.
//!
//! The signing key is a cluster-wide shared secret, like Slurm's `munge.key`: any holder can mint a
//! credential, so a compromised node can present itself to another node as the controller. That is
//! the accepted HPC model and a large improvement on "any peer at all", but it is not the same as
//! asymmetric signing, where only the controller could ever mint. Moving to an asymmetric key so
//! nodes can verify but not sign is the natural hardening step.
//!
//! The ruling itself is `spur_core::auth::authenticate_bearer`, shared with the controller so the
//! two daemons cannot drift apart on a security decision.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tower::{Layer, Service};
use tracing::warn;

use spur_core::auth::BearerOutcome;
use spur_core::config::AuthMode;

#[derive(Clone)]
pub struct AgentAuthLayer {
    inner: Arc<AgentAuthConfig>,
}

struct AgentAuthConfig {
    mode: AuthMode,
    jwt_key: Vec<u8>,
}

impl AgentAuthLayer {
    pub fn new(mode: AuthMode, jwt_key: &str) -> Self {
        Self {
            inner: Arc::new(AgentAuthConfig {
                mode,
                jwt_key: jwt_key.as_bytes().to_vec(),
            }),
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
    config: Arc<AgentAuthConfig>,
}

fn decide(config: &AgentAuthConfig, header: Option<&str>) -> BearerOutcome {
    spur_core::auth::authenticate_bearer(
        config.mode,
        &config.jwt_key,
        header,
        "this agent only accepts calls carrying the cluster credential",
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
        let header = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        match decide(&self.config, header.as_deref()) {
            // The agent does not act *as* the caller — it runs what the controller allocated — so
            // the identity is not carried into handlers; verifying the credential is the point.
            BearerOutcome::Authenticated(_) => {}
            BearerOutcome::Anonymous => {
                if self.config.mode == AuthMode::Permissive {
                    warn!(
                        path = %req.uri().path(),
                        "unauthenticated agent request accepted (auth.mode = permissive): any peer \
                         that can reach this port can ask this node to run work"
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

    fn cfg(mode: AuthMode, key: &str) -> AgentAuthConfig {
        AgentAuthConfig {
            mode,
            jwt_key: key.as_bytes().to_vec(),
        }
    }

    fn controller_token(key: &str) -> String {
        generate_token("spurctld", 0, true, key.as_bytes(), 300).unwrap()
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
        // The migration window: controllers start presenting a credential before agents demand one.
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
}
