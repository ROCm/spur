// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A `kube::Client` backed by a canned API server that records each request,
//! body included, so tests can assert what spurd sends to Kubernetes.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use super::{GpuSharing, KubeletLinks};
use http::header::CONTENT_TYPE;
use http::{Method, Request, Response, StatusCode};
use kube::client::Body;

#[derive(Debug, Clone)]
pub struct SeenRequest {
    pub method: Method,
    pub path: String,
    pub content_type: String,
    pub body: serde_json::Value,
}

type Route = dyn Fn(&SeenRequest) -> (StatusCode, serde_json::Value) + Send + Sync;

/// Answers each request from a route closure. Clones share the log.
#[derive(Clone)]
pub struct FakeApiServer {
    route: Arc<Route>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeApiServer {
    /// Answers every request with the same status and JSON body.
    pub fn answering(status: StatusCode, body: &serde_json::Value) -> Self {
        let body = body.clone();
        Self::routing(move |_| (status, body.clone()))
    }

    pub fn routing(
        route: impl Fn(&SeenRequest) -> (StatusCode, serde_json::Value) + Send + Sync + 'static,
    ) -> Self {
        Self {
            route: Arc::new(route),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl tower::Service<Request<Body>> for FakeApiServer {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let bytes = body.collect_bytes().await.unwrap_or_default();
            let seen = SeenRequest {
                method: parts.method,
                path: parts.uri.path().to_string(),
                content_type: parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            };
            let (status, body) = (this.route)(&seen);
            this.seen.lock().unwrap().push(seen);
            Ok(Response::builder()
                .status(status)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string().into_bytes()))
                .unwrap())
        })
    }
}

impl GpuSharing {
    /// A node the controller marks as shared, with `server` as its Kubernetes API.
    pub(crate) fn shared_for_test(server: &FakeApiServer) -> Arc<Self> {
        let sharing = Self::build(
            "GPU-Node-1",
            None,
            std::sync::Weak::new(),
            KubeletLinks::under(std::path::Path::new("/nonexistent")),
            Some(server.client()),
        );
        sharing.set_desired(true);
        Arc::new(sharing)
    }
}
