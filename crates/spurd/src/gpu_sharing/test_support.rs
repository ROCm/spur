// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A `kube::Client` backed by a canned API server that records each request,
//! body included, so tests can assert what spurd sends to Kubernetes.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

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

/// Answers every request with the same status and JSON body. Clones share the log.
#[derive(Clone)]
pub struct FakeApiServer {
    status: StatusCode,
    body: Arc<[u8]>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeApiServer {
    pub fn answering(status: StatusCode, body: &serde_json::Value) -> Self {
        Self {
            status,
            body: body.to_string().into_bytes().into(),
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
            this.seen.lock().unwrap().push(SeenRequest {
                method: parts.method,
                path: parts.uri.path().to_string(),
                content_type: parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            });
            Ok(Response::builder()
                .status(this.status)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(this.body.to_vec()))
                .unwrap())
        })
    }
}
