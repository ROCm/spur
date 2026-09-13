// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-process `SlurmAgent` mock for exercising CLI code paths that tail a step's
//! output straight from a compute node.
//!
//! Same shape as [`crate::mock_controller`]: bind an ephemeral localhost port,
//! serve a hand-written service on it, and hand the caller back the address plus
//! a shared record of what the server observed. Only `StreamJobOutput` is
//! implemented; every other RPC reports `unimplemented` so a test that drifts
//! onto an unmocked call fails loudly instead of silently succeeding.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use spur_proto::proto::{self, slurm_agent_server};
use tonic::{Request, Response, Status};

/// A scripted reply to one `StreamJobOutput` call: the bytes to send, then
/// either a clean eof or a transport-style error standing in for the agent going
/// away mid-tail.
pub(crate) struct ScriptedStream {
    pub(crate) data: Vec<u8>,
    pub(crate) then_eof: bool,
}

/// What the mock agent received, shared with the test body.
#[derive(Clone, Default)]
pub(crate) struct StreamCapture {
    start_offsets: Arc<Mutex<Vec<u64>>>,
    script: Arc<Mutex<Vec<ScriptedStream>>>,
}

impl StreamCapture {
    /// `start_offset` of every `StreamJobOutput` call, in arrival order.
    pub(crate) fn start_offsets(&self) -> Vec<u64> {
        self.start_offsets.lock().expect("capture lock").clone()
    }

    /// Queue the replies successive `StreamJobOutput` calls get. A call past the
    /// end of the script eofs with no data.
    pub(crate) fn script(&self, steps: Vec<ScriptedStream>) {
        *self.script.lock().expect("capture lock") = steps;
    }

    fn next_reply(&self) -> Option<ScriptedStream> {
        let mut script = self.script.lock().expect("capture lock");
        if script.is_empty() {
            return None;
        }
        Some(script.remove(0))
    }
}

struct MockAgent {
    capture: StreamCapture,
}

/// Emit the whole `impl` block, including the `#[tonic::async_trait]`
/// attribute. The attribute has to be applied by the macro rather than written
/// above the invocation: `async_trait` rewrites `async fn` signatures, and it
/// only sees method bodies that already exist when it runs.
macro_rules! mock_agent_impl {
    (
        implemented { $($implemented:tt)* }
        unimplemented { $($method:ident($req:ty) -> $resp:ty;)* }
    ) => {
        #[tonic::async_trait]
        impl slurm_agent_server::SlurmAgent for MockAgent {
            $($implemented)*
            $(
                async fn $method(
                    &self,
                    _request: Request<$req>,
                ) -> Result<Response<$resp>, Status> {
                    Err(Status::unimplemented(stringify!($method)))
                }
            )*
        }
    };
}

mock_agent_impl! {
    implemented {
        type StreamJobOutputStream =
            tokio_stream::wrappers::ReceiverStream<Result<proto::StreamJobOutputChunk, Status>>;
        type InteractiveSessionStream =
            tokio_stream::wrappers::ReceiverStream<Result<proto::InteractiveOutput, Status>>;

        async fn stream_job_output(
            &self,
            request: Request<proto::StreamJobOutputRequest>,
        ) -> Result<Response<Self::StreamJobOutputStream>, Status> {
            let req = request.into_inner();
            self.capture
                .start_offsets
                .lock()
                .expect("capture lock")
                .push(req.start_offset);
            let reply = self.capture.next_reply();
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tokio::spawn(async move {
                let eof = proto::StreamJobOutputChunk { data: Vec::new(), eof: true };
                let Some(reply) = reply else {
                    let _ = tx.send(Ok(eof)).await;
                    return;
                };
                if !reply.data.is_empty() {
                    let _ = tx
                        .send(Ok(proto::StreamJobOutputChunk { data: reply.data, eof: false }))
                        .await;
                }
                if reply.then_eof {
                    let _ = tx.send(Ok(eof)).await;
                } else {
                    // What a client sees when the agent it was tailing goes away.
                    let _ = tx.send(Err(Status::unavailable("agent restarting"))).await;
                }
            });
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        async fn interactive_session(
            &self,
            _request: Request<tonic::Streaming<proto::InteractiveInput>>,
        ) -> Result<Response<Self::InteractiveSessionStream>, Status> {
            Err(Status::unimplemented("interactive_session"))
        }
    }
    unimplemented {
        launch_job(proto::LaunchJobRequest) -> proto::LaunchJobResponse;
        prepare_pmix(proto::PreparePmixRequest) -> proto::PreparePmixResponse;
        release_pmix(proto::ReleasePmixRequest) -> proto::ReleasePmixResponse;
        start_job(proto::AgentStartJobRequest) -> ();
        cancel_job(proto::AgentCancelJobRequest) -> ();
        suspend_job(proto::AgentSuspendJobRequest) -> ();
        get_node_resources(()) -> proto::NodeResourcesResponse;
        probe_stepd(proto::StepdProbeRequest) -> proto::StepdProbeResponse;
        exec_in_job(proto::ExecInJobRequest) -> proto::ExecInJobResponse;
        run_command(proto::RunCommandRequest) -> proto::RunCommandResponse;
        cancel_step(proto::CancelStepRequest) -> ();
        register_job_allocation(proto::RegisterJobAllocationRequest)
            -> proto::RegisterJobAllocationResponse;
        await_step(proto::AwaitStepRequest) -> proto::RunCommandResponse;
        start_cluster_component(proto::StartClusterComponentRequest)
            -> proto::StartClusterComponentResponse;
        stop_cluster_component(proto::StopClusterComponentRequest)
            -> proto::StopClusterComponentResponse;
        get_cluster_component_status(proto::GetClusterComponentStatusRequest)
            -> proto::GetClusterComponentStatusResponse;
        create_k0s_join_token(proto::CreateK0sJoinTokenRequest)
            -> proto::CreateK0sJoinTokenResponse;
        drain_k8s_node(proto::DrainK8sNodeRequest) -> proto::DrainK8sNodeResponse;
        delete_k8s_node(proto::DeleteK8sNodeRequest) -> proto::DeleteK8sNodeResponse;
        get_kubeconfig(proto::GetKubeconfigRequest) -> proto::GetKubeconfigResponse;
        apply_mesh(proto::MeshMembership) -> proto::ApplyMeshResponse;
    }
}

/// Serve a mock agent on an ephemeral localhost port.
pub(crate) async fn spawn() -> (SocketAddr, StreamCapture) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let capture = StreamCapture::default();
    let service = MockAgent {
        capture: capture.clone(),
    };
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(spur_proto::agent_server(service))
            .serve_with_incoming(incoming),
    );
    (addr, capture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unmocked_rpc_reports_unimplemented() {
        let (addr, _capture) = spawn().await;
        let status = crate::interactive::connect_agent(&format!("http://{addr}"))
            .await
            .expect("connect to mock agent")
            .get_node_resources(())
            .await
            .expect_err("get_node_resources is not mocked");
        assert_eq!(status.code(), tonic::Code::Unimplemented);
        assert_eq!(status.message(), "get_node_resources");
    }
}
