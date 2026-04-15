//! gRPC server for Raft internal RPCs.
//!
//! Runs on a dedicated port (default 6821) separate from the client API,
//! so Raft heartbeats are not affected by client request load.

use std::net::SocketAddr;

use tonic::{Request, Response, Status};
use tracing::info;

use spur_proto::raft_proto::raft_internal_server::{RaftInternal, RaftInternalServer};
use spur_proto::raft_proto::{RaftRequest, RaftResponse};

use crate::raft::{SpurRaft, SpurTypeConfig};

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

pub async fn serve_raft(addr: SocketAddr, raft: SpurRaft) -> anyhow::Result<()> {
    let service = RaftInternalService::new(raft);

    info!(%addr, "raft internal gRPC server listening");

    tonic::transport::Server::builder()
        .add_service(RaftInternalServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
