// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC server for Raft internal RPCs.
//!
//! Runs on a dedicated port (default 6821) separate from the client API,
//! so Raft heartbeats are not affected by client request load.

use std::net::SocketAddr;

use tonic::{Request, Response, Status};
use tracing::info;

use spur_proto::raft_proto::raft_internal_server::{RaftInternal, RaftInternalServer};
use spur_proto::raft_proto::{RaftRequest, RaftResponse};

use crate::raft::{SpurRaft, SpurTypeConfig, RAFT_MAX_MESSAGE_SIZE};

pub struct RaftInternalService {
    raft: SpurRaft,
}

impl RaftInternalService {
    pub fn new(raft: SpurRaft) -> Self {
        Self { raft }
    }
}

#[tonic::async_trait]
impl RaftInternal for RaftInternalService {
    async fn append_entries(
        &self,
        request: Request<RaftRequest>,
    ) -> Result<Response<RaftResponse>, Status> {
        let rpc: openraft::raft::AppendEntriesRequest<SpurTypeConfig> =
            serde_json::from_slice(&request.into_inner().payload)
                .map_err(|e| Status::invalid_argument(format!("bad payload: {e}")))?;

        let resp = self
            .raft
            .append_entries(rpc)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let payload = serde_json::to_vec(&resp)
            .map_err(|e| Status::internal(format!("serialize error: {e}")))?;

        Ok(Response::new(RaftResponse { payload }))
    }

    async fn vote(&self, request: Request<RaftRequest>) -> Result<Response<RaftResponse>, Status> {
        let rpc: openraft::raft::VoteRequest<crate::raft::NodeId> =
            serde_json::from_slice(&request.into_inner().payload)
                .map_err(|e| Status::invalid_argument(format!("bad payload: {e}")))?;

        let resp = self
            .raft
            .vote(rpc)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let payload = serde_json::to_vec(&resp)
            .map_err(|e| Status::internal(format!("serialize error: {e}")))?;

        Ok(Response::new(RaftResponse { payload }))
    }

    async fn install_snapshot(
        &self,
        request: Request<RaftRequest>,
    ) -> Result<Response<RaftResponse>, Status> {
        let rpc: openraft::raft::InstallSnapshotRequest<SpurTypeConfig> =
            serde_json::from_slice(&request.into_inner().payload)
                .map_err(|e| Status::invalid_argument(format!("bad payload: {e}")))?;

        let resp = self
            .raft
            .install_snapshot(rpc)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let payload = serde_json::to_vec(&resp)
            .map_err(|e| Status::internal(format!("serialize error: {e}")))?;

        Ok(Response::new(RaftResponse { payload }))
    }
}

/// Build the Raft gRPC service with message-size limits raised to
/// `RAFT_MAX_MESSAGE_SIZE`. Shared by `serve_raft` and tests so the exact
/// transport configuration is covered.
pub(crate) fn raft_server<T: RaftInternal>(service: T) -> RaftInternalServer<T> {
    RaftInternalServer::new(service)
        .max_decoding_message_size(RAFT_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(RAFT_MAX_MESSAGE_SIZE)
}

pub async fn serve_raft(addr: SocketAddr, raft: SpurRaft) -> anyhow::Result<()> {
    let service = RaftInternalService::new(raft);

    info!(%addr, "raft internal gRPC server listening");

    tonic::transport::Server::builder()
        .add_service(raft_server(service))
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::raft_proto::raft_internal_client::RaftInternalClient;

    /// Minimal `RaftInternal` that echoes the received payload length. The unit
    /// under test is the gRPC transport message-size configuration, not Raft
    /// logic, so a stub keeps the test deterministic and free of election timing.
    struct EchoRaft;

    #[tonic::async_trait]
    impl RaftInternal for EchoRaft {
        async fn append_entries(
            &self,
            request: Request<RaftRequest>,
        ) -> Result<Response<RaftResponse>, Status> {
            let len = request.into_inner().payload.len() as u64;
            Ok(Response::new(RaftResponse {
                payload: len.to_le_bytes().to_vec(),
            }))
        }

        async fn vote(
            &self,
            _request: Request<RaftRequest>,
        ) -> Result<Response<RaftResponse>, Status> {
            Ok(Response::new(RaftResponse {
                payload: Vec::new(),
            }))
        }

        async fn install_snapshot(
            &self,
            request: Request<RaftRequest>,
        ) -> Result<Response<RaftResponse>, Status> {
            let len = request.into_inner().payload.len() as u64;
            Ok(Response::new(RaftResponse {
                payload: len.to_le_bytes().to_vec(),
            }))
        }
    }

    async fn spawn(service: RaftInternalServer<EchoRaft>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(incoming)
                .await;
        });
        addr
    }

    async fn connect(addr: SocketAddr) -> tonic::transport::Channel {
        tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    /// The customer bug: an `install_snapshot` message larger than tonic's 4 MiB
    /// default must succeed once the server and client raise their limits. Uses
    /// the exact production constructors (`raft_server` / `raft_client`).
    #[tokio::test]
    async fn install_snapshot_accepts_message_over_4mb() {
        let addr = spawn(raft_server(EchoRaft)).await;
        let mut client = crate::raft::raft_client(connect(addr).await);

        let payload = vec![0u8; 8 * 1024 * 1024];
        let resp = client
            .install_snapshot(RaftRequest {
                payload: payload.clone(),
            })
            .await
            .expect("8 MiB install_snapshot should be accepted with raised limits");

        let echoed = u64::from_le_bytes(resp.into_inner().payload.try_into().unwrap());
        assert_eq!(echoed as usize, payload.len());
    }

    /// Negative control: with tonic's default 4 MiB decode limit the same
    /// message is rejected, proving the raised limit is what fixes the bug. The
    /// client encoding limit is raised so the failure is the server-side decode
    /// cap (the exact failure seen in the field), not a client encode cap.
    #[tokio::test]
    async fn default_limits_reject_message_over_4mb() {
        let addr = spawn(RaftInternalServer::new(EchoRaft)).await;
        let mut client = RaftInternalClient::new(connect(addr).await)
            .max_encoding_message_size(64 * 1024 * 1024);

        let payload = vec![0u8; 8 * 1024 * 1024];
        let err = client
            .install_snapshot(RaftRequest { payload })
            .await
            .expect_err("8 MiB should exceed tonic's default 4 MiB decode limit");

        assert_eq!(err.code(), tonic::Code::OutOfRange);
    }
}
