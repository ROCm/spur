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
use serde_json::{json, Value};
use spur_devices::cdi::SharingIdentity;

use super::{lock, GpuSharing, KubeletLinks};

pub const NODE: &str = "gpu-node-1";

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
    /// A node the controller marks as shared, with `server` as its
    /// Kubernetes API and `identities` as its GPUs.
    pub(crate) fn shared_for_test(
        server: &FakeApiServer,
        identities: Vec<SharingIdentity>,
    ) -> Arc<Self> {
        let sharing = Self::build(
            "GPU-Node-1",
            None,
            std::sync::Weak::new(),
            KubeletLinks::under(std::path::Path::new("/nonexistent")),
            Some(server.client()),
        );
        lock(&sharing.identities).1 = identities.into_iter().map(|i| (i.stable_id, i)).collect();
        sharing.set_desired(true);
        Arc::new(sharing)
    }
}

pub fn identity(stable_id: u64, card: u32, render_minor: u32, bdf: &str) -> SharingIdentity {
    SharingIdentity {
        stable_id,
        render_minor,
        card_id: Some(card),
        selector_bdf: bdf.to_string(),
    }
}

fn status(code: u16) -> (StatusCode, Value) {
    let status = StatusCode::from_u16(code).unwrap();
    let body = json!({"kind": "Status", "apiVersion": "v1", "metadata": {},
        "status": if status.is_success() { "Success" } else { "Failure" },
        "reason": status.canonical_reason(), "code": code});
    (status, body)
}

pub fn claim_json(name: &str, devices: &[&str]) -> Value {
    let results: Vec<Value> = devices
        .iter()
        .map(|d| json!({"request": "g0", "driver": "gpu.amd.com", "pool": NODE, "device": d}))
        .collect();
    json!({"apiVersion": "resource.k8s.io/v1", "kind": "ResourceClaim",
        "metadata": {"name": name, "namespace": "spur-system"}, "spec": {},
        "status": {"allocation": {"devices": {"results": results}}}})
}

/// An API server on which every placeholder is scheduled at once and its
/// claim holds `allocated`, and every delete succeeds. `lists` answers the
/// placeholder listings of the orphan check.
pub fn placeholder_api(allocated: &[&str], lists: &[(u32, u32)]) -> FakeApiServer {
    let allocated: Vec<String> = allocated.iter().map(|d| d.to_string()).collect();
    let items: Vec<Value> = lists
        .iter()
        .map(|(job, attempt)| {
            json!({"metadata": {"name": format!("spur-job-{job}-{attempt}-{NODE}"),
                "namespace": "spur-system", "labels": {
                "app.kubernetes.io/managed-by": "spurd", "spur.amd.com/node": NODE,
                "spur.amd.com/job-id": job.to_string(),
                "spur.amd.com/run-attempt": attempt.to_string()}}, "spec": {}})
        })
        .collect();
    FakeApiServer::routing(move |req| {
        let pods = "/api/v1/namespaces/spur-system/pods";
        let claims = "/apis/resource.k8s.io/v1/namespaces/spur-system/resourceclaims";
        let name = req.path.rsplit('/').next().unwrap_or_default();
        let devices: Vec<&str> = allocated.iter().map(String::as_str).collect();
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/api/v1/namespaces") => status(409),
            ("POST", _) => (StatusCode::CREATED, req.body.clone()),
            ("DELETE", _) => status(200),
            ("GET", p) if p == pods => (
                StatusCode::OK,
                json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": items}),
            ),
            ("GET", p) if p == claims => (
                StatusCode::OK,
                json!({"apiVersion": "resource.k8s.io/v1", "kind": "ResourceClaimList",
                    "metadata": {}, "items": items}),
            ),
            ("GET", p) if p.starts_with(claims) => (StatusCode::OK, claim_json(name, &devices)),
            ("GET", p) if p.starts_with(pods) => (
                StatusCode::OK,
                json!({"apiVersion": "v1", "kind": "Pod",
                    "metadata": {"name": name, "namespace": "spur-system"},
                    "status": {"conditions": [{"type": "PodScheduled", "status": "True"}]}}),
            ),
            _ => status(500),
        }
    })
}
