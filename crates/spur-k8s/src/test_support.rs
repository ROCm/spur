// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Canned stand-ins for the Kubernetes API server and spurctld, so the operator
//! can be tested without either.

use std::convert::Infallible;
use std::future::{ready, Ready};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::header::CONTENT_TYPE;
use http::{Method, Request, Response, StatusCode};
use kube::client::Body;
use serde::Serialize;
use spur_proto::proto as pb;
use spur_proto::proto::slurm_controller_server::SlurmController;

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

/// An API server that answers each request with a canned status and JSON body,
/// the same one unless a route overrides it, and records what it was asked.
/// Cloning shares the request log.
#[derive(Clone)]
pub struct FakeApiServer {
    status: StatusCode,
    body: Arc<[u8]>,
    routes: Arc<Vec<Route>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

#[derive(Clone)]
struct Route {
    method: Method,
    path: String,
    status: StatusCode,
    body: Vec<u8>,
}

impl FakeApiServer {
    pub fn answering<T: Serialize>(status: StatusCode, body: &T) -> Self {
        let body = serde_json::to_vec(body).expect("test body serializes");
        Self {
            status,
            body: body.into(),
            routes: Arc::new(Vec::new()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Answer `method` on `path` with `status` and `body` instead of the default.
    pub fn route<T: Serialize>(
        mut self,
        method: Method,
        path: &str,
        status: StatusCode,
        body: &T,
    ) -> Self {
        let body = serde_json::to_vec(body).expect("test body serializes");
        Arc::make_mut(&mut self.routes).push(Route {
            method,
            path: path.to_string(),
            status,
            body,
        });
        self
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
        let (status, body) = self
            .routes
            .iter()
            .find(|route| route.method == seen.method && route.path == seen.path)
            .map(|route| (route.status, route.body.clone()))
            .unwrap_or_else(|| (self.status, self.body.to_vec()));
        self.seen
            .lock()
            .expect("request log is not poisoned")
            .push(seen);
        let response = Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("static response parts are valid");
        ready(Ok(response))
    }
}

/// A spurctld that records the cancels and status reports it receives.
#[derive(Clone, Default)]
pub struct ControllerStub {
    /// The error `cancel_job` answers with; `None` accepts it.
    pub cancel_reply: Option<tonic::Code>,
    pub cancelled: Arc<Mutex<Vec<u32>>>,
    pub reports: Arc<Mutex<Vec<pb::ReportJobStatusRequest>>>,
}

impl ControllerStub {
    /// Serve the stub on a loopback port and return its address.
    pub async fn serve(self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(spur_proto::controller_server(self))
                .serve_with_incoming(incoming),
        );
        addr
    }
}

/// Generates the impl so `async_trait` runs after expansion. Only `cancel_job`
/// and `report_job_status` have behavior; the rest just have to exist.
macro_rules! stub_controller {
    ($($name:ident($req:ty) -> $resp:ty;)*) => {
        #[tonic::async_trait]
        impl SlurmController for ControllerStub {
            async fn cancel_job(
                &self,
                request: tonic::Request<pb::CancelJobRequest>,
            ) -> Result<tonic::Response<()>, tonic::Status> {
                self.cancelled
                    .lock()
                    .expect("cancel log is not poisoned")
                    .push(request.into_inner().job_id);
                match self.cancel_reply {
                    None => Ok(tonic::Response::new(())),
                    Some(code) => Err(tonic::Status::new(code, "stub")),
                }
            }

            async fn report_job_status(
                &self,
                request: tonic::Request<pb::ReportJobStatusRequest>,
            ) -> Result<tonic::Response<()>, tonic::Status> {
                self.reports
                    .lock()
                    .expect("report log is not poisoned")
                    .push(request.into_inner());
                Ok(tonic::Response::new(()))
            }

            $(
                async fn $name(
                    &self,
                    _: tonic::Request<$req>,
                ) -> Result<tonic::Response<$resp>, tonic::Status> {
                    Err(tonic::Status::unimplemented(stringify!($name)))
                }
            )*
        }
    };
}

stub_controller! {
    submit_job(pb::SubmitJobRequest) -> pb::SubmitJobResponse;
    get_jobs(pb::GetJobsRequest) -> pb::GetJobsResponse;
    get_job(pb::GetJobRequest) -> pb::JobInfo;
    complete_job(pb::CompleteJobRequest) -> ();
    job_keepalive(pb::JobKeepaliveRequest) -> pb::JobKeepaliveResponse;
    suspend_job(pb::SuspendJobRequest) -> ();
    resume_job(pb::ResumeJobRequest) -> ();
    update_job(pb::UpdateJobRequest) -> ();
    requeue_job(pb::RequeueJobRequest) -> pb::RequeueJobResponse;
    get_nodes(pb::GetNodesRequest) -> pb::GetNodesResponse;
    get_node(pb::GetNodeRequest) -> pb::NodeInfo;
    get_fabric_topology(()) -> pb::GetFabricTopologyResponse;
    validate_block_file(pb::ValidateBlockFileRequest) -> pb::GetFabricTopologyResponse;
    update_node(pb::UpdateNodeRequest) -> ();
    drain_node(pb::DrainNodeRequest) -> pb::DrainNodeResponse;
    deregister_node(pb::DeregisterNodeRequest) -> pb::DeregisterNodeResponse;
    deregister_agent(pb::DeregisterAgentRequest) -> ();
    get_partitions(pb::GetPartitionsRequest) -> pb::GetPartitionsResponse;
    get_job_steps(pb::GetJobStepsRequest) -> pb::GetJobStepsResponse;
    create_job_step(pb::CreateJobStepRequest) -> pb::CreateJobStepResponse;
    complete_job_step(pb::CompleteJobStepRequest) -> ();
    create_partition(pb::CreatePartitionRequest) -> ();
    update_partition(pb::UpdatePartitionRequest) -> ();
    delete_partition(pb::DeletePartitionRequest) -> ();
    reconfigure(()) -> ();
    ping(()) -> pb::PingResponse;
    get_job_metrics(()) -> pb::JobMetrics;
    get_node_metrics(()) -> pb::NodeMetrics;
    get_rpc_stats(()) -> pb::RpcStats;
    reset_diag_stats(()) -> ();
    get_sched_stats(()) -> pb::SchedStats;
    get_assoc_mgr_info(pb::GetAssocMgrInfoRequest) -> pb::GetAssocMgrInfoResponse;
    register_agent(pb::RegisterAgentRequest) -> pb::RegisterAgentResponse;
    heartbeat(pb::HeartbeatRequest) -> pb::HeartbeatResponse;
    create_token(pb::CreateTokenRequest) -> pb::CreateTokenResponse;
    list_tokens(pb::ListTokensRequest) -> pb::ListTokensResponse;
    revoke_token(pb::RevokeTokenRequest) -> pb::RevokeTokenResponse;
    report_stepd_recovery(pb::StepdRecoveryRequest) -> pb::StepdRecoveryResponse;
    create_reservation(pb::CreateReservationRequest) -> ();
    update_reservation(pb::UpdateReservationRequest) -> ();
    delete_reservation(pb::DeleteReservationRequest) -> ();
    list_reservations(pb::ListReservationsRequest) -> pb::ListReservationsResponse;
    exec_in_job(pb::ExecInJobRequest) -> pb::ExecInJobResponse;
    run_step(pb::RunStepRequest) -> pb::RunStepResponse;
    cluster_up(pb::ClusterUpRequest) -> pb::ClusterUpResponse;
    cluster_down(pb::ClusterDownRequest) -> pb::ClusterDownResponse;
    cluster_status(pb::ClusterStatusRequest) -> pb::ClusterStatusResponse;
    cluster_kubeconfig(pb::ClusterKubeconfigRequest) -> pb::ClusterKubeconfigResponse;
    cluster_add_nodes(pb::ClusterAddNodesRequest) -> pb::ClusterAddNodesResponse;
    cluster_remove_nodes(pb::ClusterRemoveNodesRequest) -> pb::ClusterRemoveNodesResponse;
}
