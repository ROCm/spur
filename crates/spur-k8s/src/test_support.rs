// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A `kube::Client` that talks to a canned API server, so the operator's reads
//! of the cluster can be tested without a cluster.

use std::convert::Infallible;
use std::future::{ready, Ready};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::header::CONTENT_TYPE;
use http::{Method, Request, Response, StatusCode};
use kube::client::Body;
use serde::Serialize;

/// One request the fake API server received, reduced to what tests assert on.
#[derive(Debug, Clone)]
pub struct SeenRequest {
    pub method: Method,
    pub path: String,
    pub query: String,
}

impl SeenRequest {
    /// The query with the percent-encoding `kube` applies to selectors undone,
    /// so an assertion can read `labelSelector=spur.amd.com/job-id=7`.
    pub fn decoded_query(&self) -> String {
        self.query.replace("%2F", "/").replace("%3D", "=")
    }
}

/// An API server that answers every request with the same status and JSON
/// body and records what it was asked. Cloning shares the request log.
#[derive(Clone)]
pub struct FakeApiServer {
    status: StatusCode,
    body: Arc<[u8]>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeApiServer {
    pub fn answering<T: Serialize>(status: StatusCode, body: &T) -> Self {
        let body = serde_json::to_vec(body).expect("test body serializes");
        Self {
            status,
            body: body.into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.seen
            .lock()
            .expect("request log is not poisoned")
            .clone()
    }
}

/// The body of a Kubernetes list call, as the API server sends it.
pub fn list_response<T: Serialize>(items: &[T]) -> serde_json::Value {
    serde_json::json!({ "metadata": {}, "items": items })
}

impl tower::Service<Request<Body>> for FakeApiServer {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let seen = SeenRequest {
            method: req.method().clone(),
            path: req.uri().path().to_string(),
            query: req.uri().query().unwrap_or_default().to_string(),
        };
        self.seen
            .lock()
            .expect("request log is not poisoned")
            .push(seen);
        let response = Response::builder()
            .status(self.status)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(self.body.to_vec()))
            .expect("static response parts are valid");
        ready(Ok(response))
    }
}
