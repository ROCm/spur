// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request, Response, Status};
use tracing::{info, warn};

use spur_core::job::NodeCompleteError;
use spur_core::reservation::Reservation;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::slurm_controller_server::SlurmController;
use spur_proto::proto::*;

use crate::cluster::{ClusterManager, ReservationError};
use crate::raft::RaftHandle;
use crate::rpc_middleware::RpcStatsLayer;
use crate::rpc_stats::RpcStatsCollector;
use crate::sched_stats::SchedStatsCollector;

const FORWARDED_HEADER: &str = "x-spur-forwarded";
const LEADER_HEADER: &str = "x-spur-leader";

pub struct ControllerService {
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
    leader_proxy: LeaderProxy,
    /// Node ID → client API address (host:6817) for the x-spur-leader header.
    client_addrs: BTreeMap<u64, String>,
    rpc_stats: Arc<RpcStatsCollector>,
    sched_stats: Arc<SchedStatsCollector>,
}

struct LeaderProxy {
    raft: Arc<RaftHandle>,
    client_addrs: BTreeMap<u64, String>,
    cached_client: Mutex<Option<(u64, SlurmControllerClient<tonic::transport::Channel>)>>,
}

impl LeaderProxy {
    fn new(raft: Arc<RaftHandle>, client_addrs: BTreeMap<u64, String>) -> Self {
        Self {
            raft,
            client_addrs,
            cached_client: Mutex::new(None),
        }
    }

    async fn get_leader_client(
        &self,
    ) -> Result<SlurmControllerClient<tonic::transport::Channel>, Status> {
        let leader_id = self
            .raft
            .current_leader()
            .ok_or_else(|| Status::unavailable("no leader elected yet"))?;

        let mut cached = self.cached_client.lock().await;

        if let Some((id, ref client)) = *cached {
            if id == leader_id {
                return Ok(client.clone());
            }
        }

        let addr = self
            .client_addrs
            .get(&leader_id)
            .ok_or_else(|| Status::unavailable("leader address unknown"))?;

        let url = if addr.starts_with("http") {
            addr.clone()
        } else {
            format!("http://{}", addr)
        };

        let client = SlurmControllerClient::connect(url)
            .await
            .map_err(|e| Status::unavailable(format!("cannot reach leader: {e}")))?
            .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

        *cached = Some((leader_id, client.clone()));
        Ok(client)
    }
}

impl ControllerService {
    // tonic::Status is 176 bytes (over clippy's 128-byte threshold); fixed upstream in tonic 0.13+
    #[allow(clippy::result_large_err)]
    fn check_leader<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if self.raft.is_leader() {
            return Ok(());
        }

        if request.metadata().get(FORWARDED_HEADER).is_some() {
            return Err(self.not_leader_status());
        }

        Err(self.not_leader_status())
    }

    fn not_leader_status(&self) -> Status {
        let mut status = Status::unavailable("not the Raft leader");
        if let Some(leader_id) = self.raft.current_leader() {
            if let Some(addr) = self.client_addrs.get(&leader_id) {
                if let Ok(val) = addr.parse::<MetadataValue<tonic::metadata::Ascii>>() {
                    status.metadata_mut().insert(LEADER_HEADER, val);
                }
            }
        }
        status
    }

    fn forwarded_metadata() -> tonic::metadata::MetadataMap {
        let mut meta = tonic::metadata::MetadataMap::new();
        meta.insert(FORWARDED_HEADER, "true".parse().unwrap());
        meta
    }

    fn spawn_cancel_for_evicted(&self, evicted: &[crate::raft::JobFinalized]) {
        for fin in evicted {
            if let Some(job) = self.cluster.get_job(fin.job_id) {
                let cluster = self.cluster.clone();
                tokio::spawn(async move {
                    crate::scheduler_loop::send_cancel_to_agents(&cluster, &job, 9).await;
                });
            }
        }
    }

    /// Validate an admission token if token mode is enabled.
    /// Returns the node_token JWT to include in the registration response,
    /// or an empty string if admission mode is open.
    #[allow(clippy::result_large_err)]
    fn validate_admission(&self, join_token: &str, hostname: &str) -> Result<String, Status> {
        use spur_core::config::AdmissionMode;

        if !matches!(self.cluster.config.admission.mode, AdmissionMode::Token) {
            return Ok(String::new());
        }

        if join_token.is_empty() {
            return Err(Status::unauthenticated("admission token required"));
        }

        let (token_id, secret) = spur_core::admission::parse_token(join_token)
            .map_err(|e| Status::permission_denied(e.to_string()))?;

        let token_store = self.cluster.get_tokens();
        spur_core::admission::validate_token(token_id, secret, &token_store)
            .map_err(|e| Status::permission_denied(e.to_string()))?;

        let jwt_key = self
            .cluster
            .config
            .auth
            .jwt_key
            .as_deref()
            .unwrap_or("spur-default-key");

        spur_core::admission::generate_node_token(hostname, jwt_key.as_bytes())
            .map_err(|e| Status::internal(e.to_string()))
    }
}

#[tonic::async_trait]
impl SlurmController for ControllerService {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.submit_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward submit_job to leader: {e}");
                    return Err(status);
                }
            }
        }

        let spec = request
            .into_inner()
            .spec
            .ok_or_else(|| Status::invalid_argument("missing job spec"))?;

        let core_spec = proto_to_job_spec(spec)?;
        let job_id = self
            .cluster
            .submit_job(core_spec)
            .map_err(submit_rpc_status)?;

        Ok(Response::new(SubmitJobResponse { job_id }))
    }

    async fn get_jobs(
        &self,
        request: Request<GetJobsRequest>,
    ) -> Result<Response<GetJobsResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_jobs(fwd).await;
            }
        }

        let req = request.into_inner();

        let states: Vec<spur_core::job::JobState> = req
            .states
            .iter()
            .filter_map(|s| spur_core::job::JobState::from_proto_i32(*s))
            .collect();

        let user = if req.user.is_empty() {
            None
        } else {
            Some(req.user.as_str())
        };
        let partition = if req.partition.is_empty() {
            None
        } else {
            Some(req.partition.as_str())
        };
        let account = if req.account.is_empty() {
            None
        } else {
            Some(req.account.as_str())
        };
        let name = if req.name.is_empty() {
            None
        } else {
            Some(req.name.as_str())
        };

        let jobs = self
            .cluster
            .get_jobs(&states, user, partition, account, name, &req.job_ids);

        let proto_jobs: Vec<JobInfo> = jobs.iter().map(job_to_proto).collect();

        Ok(Response::new(GetJobsResponse { jobs: proto_jobs }))
    }

    async fn get_job(&self, request: Request<GetJobRequest>) -> Result<Response<JobInfo>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_job(fwd).await;
            }
        }

        let job_id = request.into_inner().job_id;
        let job = self
            .cluster
            .get_job_for_display(job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not found", job_id)))?;

        Ok(Response::new(job_to_proto(&job)))
    }

    async fn cancel_job(&self, request: Request<CancelJobRequest>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.cancel_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward cancel_job to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        let job_id = req.job_id;

        // Snapshot the job before cancelling so we have allocated_nodes
        let job = self.cluster.get_job(job_id);

        self.cluster
            .cancel_job(job_id, &req.user)
            .map_err(cluster_err_to_status)?;

        // Send cancel signal to agents so the process is actually killed
        if let Some(job) = job {
            let cluster = self.cluster.clone();
            tokio::spawn(async move {
                crate::scheduler_loop::send_cancel_to_agents(&cluster, &job, 0).await;
            });
        }

        Ok(Response::new(()))
    }

    async fn suspend_job(
        &self,
        request: Request<SuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.suspend_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward suspend_job to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let job_id = req.job_id;
        // Unknown job ids are NOT_FOUND (consistent with get_job), not a
        // precondition failure. Snapshot up-front for agent dispatch.
        let job = self
            .cluster
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {job_id} not found")))?;
        self.cluster
            .suspend_job(job_id, &req.user)
            .map_err(cluster_err_to_precondition_status)?;
        let cluster = self.cluster.clone();
        tokio::spawn(async move {
            crate::scheduler_loop::send_suspend_to_agents(&cluster, &job, false).await;
        });
        Ok(Response::new(()))
    }

    async fn resume_job(&self, request: Request<ResumeJobRequest>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.resume_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward resume_job to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let job_id = req.job_id;
        // Unknown job ids are NOT_FOUND (consistent with get_job), not a
        // precondition failure. Allocation is retained across resume, so this
        // up-front snapshot's allocated_nodes is still valid for agent dispatch.
        let job = self
            .cluster
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {job_id} not found")))?;
        self.cluster
            .resume_job(job_id, &req.user)
            .map_err(cluster_err_to_precondition_status)?;
        let cluster = self.cluster.clone();
        tokio::spawn(async move {
            crate::scheduler_loop::send_suspend_to_agents(&cluster, &job, true).await;
        });
        Ok(Response::new(()))
    }

    async fn update_job(&self, request: Request<UpdateJobRequest>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.update_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward update_job to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();

        // Handle hold/release via priority
        if let Some(hold) = req.hold {
            if hold {
                self.cluster
                    .hold_job(req.job_id)
                    .map_err(|e| Status::internal(e.to_string()))?;
            } else {
                self.cluster
                    .release_job(req.job_id)
                    .map_err(|e| Status::internal(e.to_string()))?;
            }
            return Ok(Response::new(()));
        }

        let time_limit = req.time_limit.map(|d| chrono::Duration::seconds(d.seconds));

        self.cluster
            .update_job(
                req.job_id,
                time_limit,
                req.priority,
                req.partition,
                req.comment,
                req.account,
                req.qos,
            )
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
    }

    async fn get_nodes(
        &self,
        request: Request<GetNodesRequest>,
    ) -> Result<Response<GetNodesResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_nodes(fwd).await;
            }
        }

        let req = request.into_inner();
        let nodes = self.cluster.get_nodes();

        let nodelist = req.nodelist.trim();
        let allowed_names: Option<HashSet<String>> = (!nodelist.is_empty()).then(|| {
            spur_sched::node_match::expand_hostlist_or_split(nodelist)
                .into_iter()
                .collect()
        });

        // Honour the request filters; without this, `scontrol show node X` and
        // `sinfo -n X` return the whole cluster.
        let mut proto_nodes: Vec<NodeInfo> = nodes
            .iter()
            .filter(|n| node_matches_filter(n, allowed_names.as_ref(), &req.partition))
            .map(node_to_proto)
            .filter(|n| req.states.is_empty() || req.states.contains(&n.state))
            .collect();

        let reservations = self.cluster.get_reservations();
        annotate_nodes_with_reservations(&mut proto_nodes, &reservations, Utc::now());
        Ok(Response::new(GetNodesResponse { nodes: proto_nodes }))
    }

    async fn get_node(
        &self,
        request: Request<GetNodeRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_node(fwd).await;
            }
        }

        let name = request.into_inner().name;
        let node = self
            .cluster
            .get_node(&name)
            .ok_or_else(|| Status::not_found(format!("node {} not found", name)))?;
        let mut proto_node = node_to_proto(&node);
        let reservations = self.cluster.get_reservations();
        annotate_nodes_with_reservations(
            std::slice::from_mut(&mut proto_node),
            &reservations,
            Utc::now(),
        );
        Ok(Response::new(proto_node))
    }

    async fn update_node(
        &self,
        request: Request<UpdateNodeRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.update_node(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward update_node to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        if let Some(state) = req.state {
            let node_state = spur_core::node::NodeState::from_proto_i32(state)
                .ok_or_else(|| Status::invalid_argument("invalid node state"))?;
            self.cluster
                .update_node_state(&req.name, node_state, req.reason)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        if !req.labels.is_empty() || !req.remove_labels.is_empty() {
            self.cluster
                .update_node_labels(&req.name, req.labels, &req.remove_labels)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        Ok(Response::new(()))
    }

    async fn drain_node(
        &self,
        request: Request<spur_proto::proto::DrainNodeRequest>,
    ) -> Result<Response<spur_proto::proto::DrainNodeResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.drain_node(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward drain_node to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let reason = if req.reason.is_empty() {
            None
        } else {
            Some(req.reason)
        };
        if self.cluster.get_node(&req.name).is_none() {
            return Err(Status::not_found(format!("node {} not found", req.name)));
        }
        let (actual_state, running_jobs) = self
            .cluster
            .drain_node(&req.name, reason)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(spur_proto::proto::DrainNodeResponse {
            actual_state: actual_state.to_string(),
            running_jobs,
        }))
    }

    async fn deregister_node(
        &self,
        request: Request<spur_proto::proto::DeregisterNodeRequest>,
    ) -> Result<Response<spur_proto::proto::DeregisterNodeResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.deregister_node(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward deregister_node to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        let reason = if req.reason.is_empty() {
            None
        } else {
            Some(req.reason)
        };
        if self.cluster.get_node(&req.name).is_none() {
            return Err(Status::not_found(format!("node {} not found", req.name)));
        }
        let evicted = self
            .cluster
            .remove_node(&req.name, req.force, reason)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        self.spawn_cancel_for_evicted(&evicted);
        self.cluster.complete_evicted_steps(&evicted);
        Ok(Response::new(spur_proto::proto::DeregisterNodeResponse {
            evicted_jobs_count: evicted.len() as u32,
        }))
    }

    async fn deregister_agent(
        &self,
        request: Request<spur_proto::proto::DeregisterAgentRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.deregister_agent(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward deregister_agent to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();

        if matches!(
            self.cluster.config.admission.mode,
            spur_core::config::AdmissionMode::Token
        ) {
            if req.node_token.is_empty() {
                return Err(Status::unauthenticated("node token required"));
            }
            let jwt_key = self
                .cluster
                .config
                .auth
                .jwt_key
                .as_deref()
                .unwrap_or("spur-default-key");
            let identity =
                spur_core::admission::verify_node_token(&req.node_token, jwt_key.as_bytes())
                    .map_err(|e| Status::unauthenticated(e.to_string()))?;
            if identity.hostname != req.hostname {
                return Err(Status::permission_denied("node token hostname mismatch"));
            }
        }

        if self.cluster.get_node(&req.hostname).is_none() {
            return Ok(Response::new(()));
        }
        let evicted = self
            .cluster
            .remove_node(
                &req.hostname,
                true,
                Some(req.reason.clone()).filter(|r| !r.is_empty()),
            )
            .map_err(|e| Status::internal(e.to_string()))?;
        self.spawn_cancel_for_evicted(&evicted);
        self.cluster.complete_evicted_steps(&evicted);
        Ok(Response::new(()))
    }

    async fn get_partitions(
        &self,
        request: Request<GetPartitionsRequest>,
    ) -> Result<Response<GetPartitionsResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_partitions(fwd).await;
            }
        }

        let partitions = self.cluster.get_partitions();
        let proto: Vec<PartitionInfo> = partitions.iter().map(partition_to_proto).collect();
        Ok(Response::new(GetPartitionsResponse { partitions: proto }))
    }

    async fn ping(&self, _request: Request<()>) -> Result<Response<PingResponse>, Status> {
        let hostname: String = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".into());

        let federation_peers: Vec<String> = self
            .cluster
            .config
            .federation
            .clusters
            .iter()
            .map(|p| format!("{}@{}", p.name, p.address))
            .collect();

        Ok(Response::new(PingResponse {
            hostname,
            server_time: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            version: env!("CARGO_PKG_VERSION").into(),
            federation_peers,
        }))
    }

    async fn get_job_metrics(&self, request: Request<()>) -> Result<Response<JobMetrics>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_job_metrics(fwd).await;
            }
        }

        let snap = self.cluster.job_metrics();
        Ok(Response::new(crate::metrics_proto::job_metrics_to_proto(
            &snap,
        )))
    }

    async fn get_node_metrics(
        &self,
        request: Request<()>,
    ) -> Result<Response<NodeMetrics>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_node_metrics(fwd).await;
            }
        }

        let snap = self.cluster.node_metrics();
        Ok(Response::new(crate::metrics_proto::node_metrics_to_proto(
            &snap,
        )))
    }

    async fn get_rpc_stats(&self, request: Request<()>) -> Result<Response<RpcStats>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_rpc_stats(fwd).await;
            }
        }

        Ok(Response::new(crate::metrics_proto::rpc_stats_to_proto(
            &self.rpc_stats.snapshot(),
        )))
    }

    async fn get_sched_stats(&self, request: Request<()>) -> Result<Response<SchedStats>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_sched_stats(fwd).await;
            }
        }

        Ok(Response::new(crate::metrics_proto::sched_stats_to_proto(
            &self.sched_stats.snapshot(),
        )))
    }

    async fn reset_diag_stats(&self, request: Request<()>) -> Result<Response<()>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.reset_diag_stats(fwd).await;
            }
        }

        self.rpc_stats.reset();
        self.sched_stats.reset();
        Ok(Response::new(()))
    }

    async fn register_agent(
        &self,
        request: Request<RegisterAgentRequest>,
    ) -> Result<Response<RegisterAgentResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.register_agent(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward register_agent to leader: {e}");
                    return Err(status);
                }
            }
        }

        // Extract the remote IP from the gRPC connection as fallback
        let remote_addr = request
            .remote_addr()
            .map(|a| {
                let ip = a.ip();
                match ip {
                    std::net::IpAddr::V6(v6) => {
                        if let Some(v4) = v6.to_ipv4_mapped() {
                            v4.to_string()
                        } else {
                            ip.to_string()
                        }
                    }
                    _ => ip.to_string(),
                }
            })
            .unwrap_or_default();

        let req = request.into_inner();
        let resources = req.resources.map(proto_to_resource_set).unwrap_or_default();

        let agent_addr = if !req.address.is_empty() {
            req.address.clone()
        } else {
            let is_loopback =
                remote_addr.is_empty() || remote_addr == "127.0.0.1" || remote_addr == "::1";
            if is_loopback {
                "127.0.0.1".to_string()
            } else {
                remote_addr
            }
        };

        let agent_port = if req.port > 0 { req.port as u16 } else { 6818 };

        let node_token_response = self.validate_admission(&req.join_token, &req.hostname)?;

        self.cluster
            .register_node(
                req.hostname.clone(),
                resources,
                agent_addr,
                agent_port,
                req.wg_pubkey,
                req.version,
                spur_core::node::NodeSource::NativeHost,
                req.labels,
            )
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(RegisterAgentResponse {
            accepted: true,
            message: "registered".into(),
            node_token: node_token_response,
        }))
    }

    async fn report_job_status(
        &self,
        request: Request<ReportJobStatusRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.report_job_status(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward report_job_status to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        let state = spur_core::job::JobState::from_proto_i32(req.state)
            .ok_or_else(|| Status::invalid_argument("invalid job state"))?;

        // Non-empty `reporting_node` means a per-node completion report. The final
        // job outcome is still derived from aggregated exit codes in
        // `Job::derived_completion`.
        let completion_result = if !req.reporting_node.is_empty() {
            validate_completion_report_state_for_rpc(state, req.exit_code)?;
            Some(self.cluster.node_complete(
                req.job_id,
                &req.reporting_node,
                req.exit_code,
                req.signal,
            ))
        } else {
            None
        };

        if req.drain_node && !req.reporting_node.is_empty() {
            warn!(
                node = %req.reporting_node,
                reason = %req.drain_reason,
                job_id = req.job_id,
                "agent requested node drain"
            );
            if let Err(e) = self.cluster.update_node_state(
                &req.reporting_node,
                spur_core::node::NodeState::Drain,
                Some(req.drain_reason),
            ) {
                warn!(
                    node = %req.reporting_node,
                    error = %e,
                    "failed to drain node on agent request"
                );
            }
        }

        use crate::cluster::NodeCompleteResult;

        match completion_result {
            Some(Ok(NodeCompleteResult::AllDone { .. })) => Ok(Response::new(())),
            Some(Ok(NodeCompleteResult::Completing)) => Ok(Response::new(())),
            Some(Ok(NodeCompleteResult::AlreadyTerminal)) => {
                warn!(
                    job_id = req.job_id,
                    node = %req.reporting_node,
                    "duplicate completion report for terminal job"
                );
                Ok(Response::new(()))
            }
            Some(Err(e)) => {
                warn!(
                    job_id = req.job_id,
                    node = %req.reporting_node,
                    error = %e,
                    "node_complete failed"
                );
                Err(node_complete_to_status(e))
            }
            None => Ok(Response::new(())),
        }
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.heartbeat(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward heartbeat to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();

        if matches!(
            self.cluster.config.admission.mode,
            spur_core::config::AdmissionMode::Token
        ) {
            if req.node_token.is_empty() {
                return Err(Status::unauthenticated("node token required"));
            }
            let jwt_key = self
                .cluster
                .config
                .auth
                .jwt_key
                .as_deref()
                .unwrap_or("spur-default-key");
            let identity =
                spur_core::admission::verify_node_token(&req.node_token, jwt_key.as_bytes())
                    .map_err(|e| Status::unauthenticated(e.to_string()))?;
            if identity.hostname != req.hostname {
                return Err(Status::permission_denied("node token hostname mismatch"));
            }
        }

        if self
            .cluster
            .update_heartbeat(&req.hostname, req.cpu_load, req.free_memory_mb)
        {
            // Learn a mesh key that appeared/changed after registration so the node joins ApplyMesh
            // without a restart. Only meaningful once the node is known (heartbeat accepted).
            if self
                .cluster
                .update_node_wg_pubkey(&req.hostname, &req.wg_pubkey)
            {
                info!(node = %req.hostname, "learned updated WireGuard mesh key from heartbeat");
            }
            Ok(Response::new(HeartbeatResponse {}))
        } else {
            Err(Status::not_found(format!(
                "node {} not found — is the node registered?",
                req.hostname
            )))
        }
    }

    async fn create_token(
        &self,
        request: Request<CreateTokenRequest>,
    ) -> Result<Response<CreateTokenResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    fwd.metadata_mut()
                        .insert("x-forwarded", "true".parse().unwrap());
                    return client.create_token(fwd).await;
                }
                Err(_) => return Err(status),
            }
        }

        let req = request.into_inner();
        let ttl_secs = req.ttl_secs.filter(|&v| v > 0);

        let (token, full_string) = self
            .cluster
            .create_token(ttl_secs)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(CreateTokenResponse {
            token: full_string,
            token_id: token.id,
        }))
    }

    async fn list_tokens(
        &self,
        _request: Request<ListTokensRequest>,
    ) -> Result<Response<ListTokensResponse>, Status> {
        use spur_proto::proto::TokenInfo;

        let tokens = self.cluster.list_tokens();
        let infos = tokens
            .into_iter()
            .map(|t| TokenInfo {
                id: t.id,
                created_at: t.created_at.to_rfc3339(),
                expires_at: t.expires_at.map(|e| e.to_rfc3339()).unwrap_or_default(),
                revoked: t.revoked,
            })
            .collect();

        Ok(Response::new(ListTokensResponse { tokens: infos }))
    }

    async fn revoke_token(
        &self,
        request: Request<RevokeTokenRequest>,
    ) -> Result<Response<RevokeTokenResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    fwd.metadata_mut()
                        .insert("x-forwarded", "true".parse().unwrap());
                    return client.revoke_token(fwd).await;
                }
                Err(_) => return Err(status),
            }
        }

        let req = request.into_inner();
        self.cluster
            .revoke_token(&req.token_id)
            .map_err(|e| Status::not_found(e.to_string()))?;

        Ok(Response::new(RevokeTokenResponse {}))
    }

    async fn get_job_steps(
        &self,
        request: Request<GetJobStepsRequest>,
    ) -> Result<Response<GetJobStepsResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_job_steps(fwd).await;
            }
        }

        let job_id = request.into_inner().job_id;
        let steps = self.cluster.get_steps(job_id);
        let step_infos: Vec<JobStepInfo> = steps
            .iter()
            .map(|s| JobStepInfo {
                job_id: s.job_id,
                step_id: s.step_id,
                name: s.name.clone(),
                state: s.state.display().to_string(),
                num_tasks: s.num_tasks,
            })
            .collect();
        Ok(Response::new(GetJobStepsResponse { steps: step_infos }))
    }

    async fn create_job_step(
        &self,
        request: Request<CreateJobStepRequest>,
    ) -> Result<Response<CreateJobStepResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.create_job_step(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward create_job_step to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        let job_id = req.job_id;

        let job = self
            .cluster
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not found", job_id)))?;

        if job.state != spur_core::job::JobState::Running {
            return Err(Status::failed_precondition(format!(
                "job {} is not running (state: {:?})",
                job_id, job.state
            )));
        }

        let existing_steps = self.cluster.get_steps(job_id);
        let step_id = existing_steps
            .iter()
            .filter(|s| s.step_id < 0xFFFF_FFF0)
            .count() as u32;

        let step = spur_core::step::JobStep {
            job_id,
            step_id,
            name: req.command.join(" "),
            state: spur_core::step::StepState::Running,
            num_tasks: req.num_tasks.max(1),
            cpus_per_task: req.cpus_per_task.max(1),
            resources: spur_core::resource::ResourceAllocations::default(),
            nodes: job.allocated_nodes.clone(),
            distribution: spur_core::step::TaskDistribution::Block,
            start_time: Some(chrono::Utc::now()),
            end_time: None,
            exit_code: None,
        };

        self.cluster.create_step(job_id, step_id, step);

        Ok(Response::new(CreateJobStepResponse { step_id }))
    }

    async fn create_reservation(
        &self,
        request: Request<CreateReservationRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.create_reservation(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward create_reservation to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();

        let start_time = if req.start_time.is_empty() || req.start_time.eq_ignore_ascii_case("now")
        {
            chrono::Utc::now()
        } else {
            req.start_time
                .parse::<chrono::DateTime<chrono::Utc>>()
                .map_err(|e| Status::invalid_argument(format!("invalid start_time: {}", e)))?
        };

        let end_time = start_time + chrono::Duration::minutes(req.duration_minutes as i64);

        let flags = spur_core::reservation::ReservationFlags::parse_list(&req.flags)
            .map_err(Status::invalid_argument)?;

        let reservation = spur_core::reservation::Reservation {
            name: req.name,
            start_time,
            end_time,
            nodes: req.nodes,
            accounts: req.accounts,
            users: req.users,
            flags,
        };

        self.cluster
            .create_reservation(reservation)
            .map_err(reservation_rpc_status)?;

        Ok(Response::new(()))
    }

    async fn update_reservation(
        &self,
        request: Request<UpdateReservationRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.update_reservation(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward update_reservation to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        self.cluster
            .update_reservation(
                &req.name,
                req.duration_minutes,
                &req.add_nodes,
                &req.remove_nodes,
                &req.add_users,
                &req.remove_users,
                &req.add_accounts,
                &req.remove_accounts,
            )
            .map_err(reservation_rpc_status)?;
        Ok(Response::new(()))
    }

    async fn delete_reservation(
        &self,
        request: Request<DeleteReservationRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.delete_reservation(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward delete_reservation to leader: {e}");
                    return Err(status);
                }
            }
        }

        let name = request.into_inner().name;
        self.cluster
            .delete_reservation(&name)
            .map_err(reservation_rpc_status)?;
        Ok(Response::new(()))
    }

    async fn list_reservations(
        &self,
        request: Request<ListReservationsRequest>,
    ) -> Result<Response<ListReservationsResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.list_reservations(fwd).await;
            }
        }
        let reservations = self.cluster.get_reservations();
        let now = Utc::now();
        let infos: Vec<ReservationInfo> = reservations
            .iter()
            .map(|r| ReservationInfo {
                name: r.name.clone(),
                start_time: r.start_time.to_rfc3339(),
                end_time: r.end_time.to_rfc3339(),
                nodes: r.nodes.join(","),
                accounts: r.accounts.join(","),
                users: r.users.join(","),
                flags: r.flags.display_csv(),
                state: r.state_label(now).into(),
            })
            .collect();
        Ok(Response::new(ListReservationsResponse {
            reservations: infos,
        }))
    }

    async fn exec_in_job(
        &self,
        request: Request<ExecInJobRequest>,
    ) -> Result<Response<ExecInJobResponse>, Status> {
        if self.check_leader(&request).is_err() {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.exec_in_job(fwd).await;
            }
        }

        use spur_proto::proto::slurm_agent_client::SlurmAgentClient;

        let req = request.into_inner();
        let job_id = req.job_id;

        let job = self
            .cluster
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not found", job_id)))?;

        if job.state != spur_core::job::JobState::Running {
            return Err(Status::failed_precondition(format!(
                "job {} is not running (state: {})",
                job_id, job.state
            )));
        }

        let node_name = job
            .allocated_nodes
            .first()
            .ok_or_else(|| Status::internal(format!("job {} has no allocated nodes", job_id)))?
            .clone();

        let node = self
            .cluster
            .get_node(&node_name)
            .ok_or_else(|| Status::not_found(format!("node {} not found", node_name)))?;
        let addr = node
            .address
            .as_ref()
            .ok_or_else(|| Status::internal(format!("node {} has no agent address", node_name)))?;
        let agent_addr = format!("http://{}:{}", addr, node.port);

        let mut agent = SlurmAgentClient::connect(agent_addr.clone())
            .await
            .map_err(|e| {
                Status::unavailable(format!("cannot reach agent at {}: {}", agent_addr, e))
            })?
            .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

        let resp = agent
            .exec_in_job(ExecInJobRequest {
                job_id,
                command: req.command,
            })
            .await
            .map_err(|e| Status::internal(format!("exec failed: {}", e)))?;

        Ok(resp)
    }

    /// #146: route a step from `srun-in-salloc` to one of the job's
    /// allocated nodes. Unlike ExecInJob, the job may not have a tracked
    /// process — salloc allocations only exist as scheduler bookkeeping.
    async fn run_step(
        &self,
        request: Request<RunStepRequest>,
    ) -> Result<Response<RunStepResponse>, Status> {
        if self.check_leader(&request).is_err() {
            let proxy = &self.leader_proxy;
            let mut client = proxy.get_leader_client().await?;
            let mut fwd = Request::new(request.into_inner());
            *fwd.metadata_mut() = Self::forwarded_metadata();
            return client.run_step(fwd).await;
        }

        use spur_proto::proto::slurm_agent_client::SlurmAgentClient;

        let req = request.into_inner();
        let job_id = req.job_id;

        let job = self
            .cluster
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not found", job_id)))?;

        if job.allocated_nodes.is_empty() {
            return Err(Status::failed_precondition(format!(
                "job {} has no allocated nodes — is the allocation still active?",
                job_id
            )));
        }

        let node_name = job.allocated_nodes[0].clone();
        let node = self
            .cluster
            .get_node(&node_name)
            .ok_or_else(|| Status::not_found(format!("node {} not found", node_name)))?;
        let addr = node
            .address
            .as_ref()
            .ok_or_else(|| Status::internal(format!("node {} has no agent address", node_name)))?;
        let agent_addr = format!("http://{}:{}", addr, node.port);

        let mut agent = SlurmAgentClient::connect(agent_addr.clone())
            .await
            .map_err(|e| {
                Status::unavailable(format!("cannot reach agent at {}: {}", agent_addr, e))
            })?
            .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

        let agent_resp = agent
            .run_command(RunCommandRequest {
                command: req.command,
                uid: req.uid,
                gid: req.gid,
                work_dir: req.work_dir,
                environment: req.environment,
                job_id: req.job_id,
            })
            .await
            .map_err(|e| Status::internal(format!("run_command failed: {}", e)))?
            .into_inner();

        // Record the step's exit code durably (Raft) so the job's live
        // DerivedExitCode (running max over steps) is consistent and survives
        // restart. Best-effort: a failure here doesn't fail the step itself.
        if let Err(e) =
            self.cluster
                .record_step_complete(req.job_id, req.step_id, agent_resp.exit_code)
        {
            warn!(
                job_id = req.job_id,
                step_id = req.step_id,
                error = %e,
                "failed to record step completion"
            );
        }

        Ok(Response::new(RunStepResponse {
            exit_code: agent_resp.exit_code,
            stdout: agent_resp.stdout,
            stderr: agent_resp.stderr,
            node: node_name,
        }))
    }

    // -- Native cluster (k0s) lifecycle. Leader-gated; record intent in the
    //    replicated k0s state and let the reconcile loop (cluster_k8s.rs) converge in 5b/5c. --
    async fn cluster_up(
        &self,
        request: Request<ClusterUpRequest>,
    ) -> Result<Response<ClusterUpResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            match self.leader_proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.cluster_up(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward cluster_up to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        // Reject a control-plane change once roles are assigned: provisioning skips already-assigned
        // nodes, so moving the control plane here would only rewrite the recorded CP and leave the
        // old controller a Controller and the new node a Worker (inconsistent topology). Tear the
        // cluster down (`spur k8s down --reset`) to re-elect a control plane.
        if let Some(new_cp) = req.control_plane_node.as_deref() {
            let state = self.cluster.k0s_state();
            let assigned = self
                .cluster
                .get_nodes()
                .iter()
                .any(|n| n.k0s_role.is_some());
            if assigned && state.control_plane_node.as_deref() != Some(new_cp) {
                return Err(Status::failed_precondition(format!(
                    "control-plane node is already assigned ({}); tear the cluster down \
                     (spur k8s down --reset) before changing it to {new_cp}",
                    state.control_plane_node.as_deref().unwrap_or("<none>"),
                )));
            }
        }
        self.cluster
            .set_k0s_phase(
                spur_core::k0s::K0sPhase::Provisioning,
                req.control_plane_node.clone(),
                false,
            )
            .map_err(|e| Status::internal(format!("set k0s phase: {e}")))?;
        Ok(Response::new(ClusterUpResponse {
            accepted: true,
            message: "k0s cluster provisioning requested".to_string(),
            nodes: crate::cluster_k8s::node_statuses(&self.cluster),
        }))
    }

    async fn cluster_down(
        &self,
        request: Request<ClusterDownRequest>,
    ) -> Result<Response<ClusterDownResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            match self.leader_proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.cluster_down(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward cluster_down to leader: {e}");
                    return Err(status);
                }
            }
        }
        let req = request.into_inner();
        self.cluster
            .set_k0s_phase(spur_core::k0s::K0sPhase::Down, None, req.reset)
            .map_err(|e| Status::internal(format!("set k0s phase: {e}")))?;
        Ok(Response::new(ClusterDownResponse {
            accepted: true,
            message: "k0s cluster teardown requested".to_string(),
        }))
    }

    async fn cluster_status(
        &self,
        request: Request<ClusterStatusRequest>,
    ) -> Result<Response<ClusterStatusResponse>, Status> {
        if self.check_leader(&request).is_err() {
            let mut client = self.leader_proxy.get_leader_client().await?;
            let mut fwd = Request::new(request.into_inner());
            *fwd.metadata_mut() = Self::forwarded_metadata();
            return client.cluster_status(fwd).await;
        }
        let state = self.cluster.k0s_state();
        Ok(Response::new(ClusterStatusResponse {
            phase: crate::cluster_k8s::phase_str(state.phase),
            control_plane_node: state.control_plane_node.unwrap_or_default(),
            nodes: crate::cluster_k8s::live_node_statuses(&self.cluster).await,
        }))
    }

    async fn cluster_kubeconfig(
        &self,
        request: Request<ClusterKubeconfigRequest>,
    ) -> Result<Response<ClusterKubeconfigResponse>, Status> {
        if self.check_leader(&request).is_err() {
            let mut client = self.leader_proxy.get_leader_client().await?;
            let mut fwd = Request::new(request.into_inner());
            *fwd.metadata_mut() = Self::forwarded_metadata();
            return client.cluster_kubeconfig(fwd).await;
        }
        let req = request.into_inner();
        if req.user.is_empty() {
            // Cluster-admin kubeconfig (`k0s kubeconfig admin` on the control-plane agent).
            return match crate::cluster_k8s::fetch_admin_kubeconfig(&self.cluster).await {
                Ok(kubeconfig) => Ok(Response::new(ClusterKubeconfigResponse { kubeconfig })),
                Err(e) => Err(Status::unavailable(format!(
                    "could not fetch admin kubeconfig from the control-plane agent: {e}"
                ))),
            };
        }
        // Scoped kubeconfig: resolve the user's account -> its namespace + per-user ServiceAccount,
        // then have the control-plane agent mint a bound token there.
        let (account, _qos) = self.cluster.association_cache().resolve(&req.user, None);
        let account = account.ok_or_else(|| {
            Status::not_found(format!(
                "user '{}' is not associated with any account",
                req.user
            ))
        })?;
        let namespace = spur_core::quota_names::account_namespace(&account);
        let sa = spur_core::quota_names::user_service_account(&req.user);
        match crate::cluster_k8s::fetch_user_kubeconfig(&self.cluster, &req.user, &namespace, &sa)
            .await
        {
            Ok(kubeconfig) => Ok(Response::new(ClusterKubeconfigResponse { kubeconfig })),
            Err(e) => Err(Status::unavailable(format!(
                "could not mint a scoped kubeconfig for user '{}': {e}",
                req.user
            ))),
        }
    }
}

pub async fn serve(
    addr: SocketAddr,
    cluster: Arc<ClusterManager>,
    raft_handle: Arc<RaftHandle>,
    rpc_stats: Arc<RpcStatsCollector>,
    sched_stats: Arc<SchedStatsCollector>,
    accounting_pool: Option<sqlx::PgPool>,
) -> anyhow::Result<()> {
    let client_addrs: BTreeMap<u64, String> = raft_handle
        .peers
        .iter()
        .map(|(id, raft_addr)| {
            let client_addr = if let Some(host) = raft_addr.rsplit_once(':').map(|(h, _)| h) {
                format!("{}:6817", host)
            } else {
                format!("{}:6817", raft_addr)
            };
            (*id, client_addr)
        })
        .collect();

    let leader_proxy = LeaderProxy::new(raft_handle.clone(), client_addrs.clone());

    let service = ControllerService {
        cluster,
        client_addrs,
        raft: raft_handle.clone(),
        leader_proxy,
        rpc_stats: rpc_stats.clone(),
        sched_stats: sched_stats.clone(),
    };

    let stats_layer = RpcStatsLayer::new(rpc_stats, raft_handle);

    let mut builder = tonic::transport::Server::builder().layer(stats_layer);

    let mut router = builder.add_service(spur_proto::controller_server(service));
    if let Some(pool) = accounting_pool {
        router = router.add_service(crate::accounting::accounting_server(pool));
    }

    router.serve(addr).await?;

    Ok(())
}

// ---- Proto conversion helpers ----

// tonic::Status is 176 bytes (over clippy's 128-byte threshold); fixed upstream in tonic 0.13+
#[allow(clippy::result_large_err)]
fn proto_to_job_spec(spec: JobSpec) -> Result<spur_core::job::JobSpec, Status> {
    let mut gres = spec.gres;
    for lic in &spec.licenses {
        gres.push(format!("license:{}", lic));
    }

    Ok(spur_core::job::JobSpec {
        name: spec.name,
        partition: if spec.partition.is_empty() {
            None
        } else {
            Some(spec.partition)
        },
        account: if spec.account.is_empty() {
            None
        } else {
            Some(spec.account)
        },
        user: spec.user,
        uid: spec.uid,
        gid: spec.gid,
        num_nodes: spec.num_nodes.max(1),
        num_tasks: spec.num_tasks.max(1),
        tasks_per_node: if spec.tasks_per_node > 0 {
            Some(spec.tasks_per_node)
        } else {
            None
        },
        cpus_per_task: spec.cpus_per_task.max(1),
        memory_per_node_mb: if spec.memory_per_node_mb > 0 {
            Some(spec.memory_per_node_mb)
        } else {
            None
        },
        memory_per_cpu_mb: if spec.memory_per_cpu_mb > 0 {
            Some(spec.memory_per_cpu_mb)
        } else {
            None
        },
        gres,
        script: if spec.script.is_empty() {
            None
        } else {
            Some(spec.script)
        },
        argv: spec.argv,
        script_args: spec.script_args,
        work_dir: if spec.work_dir.is_empty() {
            "/tmp".into()
        } else {
            spec.work_dir
        },
        stdout_path: if spec.stdout_path.is_empty() {
            None
        } else {
            Some(spec.stdout_path)
        },
        stderr_path: if spec.stderr_path.is_empty() {
            None
        } else {
            Some(spec.stderr_path)
        },
        stdin_path: if spec.stdin_path.is_empty() {
            None
        } else {
            Some(spec.stdin_path)
        },
        environment: spec.environment,
        time_limit: spec
            .time_limit
            .map(|d| chrono::Duration::seconds(d.seconds)),
        time_min: spec.time_min.map(|d| chrono::Duration::seconds(d.seconds)),
        qos: if spec.qos.is_empty() {
            None
        } else {
            Some(spec.qos)
        },
        priority: if spec.priority > 0 {
            Some(spec.priority)
        } else {
            None
        },
        reservation: if spec.reservation.is_empty() {
            None
        } else {
            Some(spec.reservation)
        },
        dependency: spec.dependency,
        nodelist: if spec.nodelist.is_empty() {
            None
        } else {
            Some(spec.nodelist)
        },
        exclude: if spec.exclude.is_empty() {
            None
        } else {
            Some(spec.exclude)
        },
        constraint: if spec.constraint.is_empty() {
            None
        } else {
            Some(spec.constraint.clone())
        },
        mpi: if spec.mpi.is_empty() {
            None
        } else {
            Some(spec.mpi)
        },
        distribution: if spec.distribution.is_empty() {
            None
        } else {
            Some(spec.distribution)
        },
        het_group: if spec.het_group > 0 {
            Some(spec.het_group)
        } else {
            None
        },
        array_spec: if spec.array_spec.is_empty() {
            None
        } else {
            Some(spec.array_spec)
        },
        array_job_id: None,
        array_task_id: None,
        array_max_concurrent: None,
        requeue: spec.requeue,
        exclusive: spec.exclusive,
        hold: spec.hold,
        interactive: spec.interactive,
        mail_type: spec.mail_type,
        mail_user: if spec.mail_user.is_empty() {
            None
        } else {
            Some(spec.mail_user)
        },
        comment: if spec.comment.is_empty() {
            None
        } else {
            Some(spec.comment)
        },
        wckey: if spec.wckey.is_empty() {
            None
        } else {
            Some(spec.wckey)
        },
        container_image: if spec.container_image.is_empty() {
            None
        } else {
            Some(spec.container_image)
        },
        container_mounts: spec.container_mounts,
        container_workdir: if spec.container_workdir.is_empty() {
            None
        } else {
            Some(spec.container_workdir)
        },
        container_name: if spec.container_name.is_empty() {
            None
        } else {
            Some(spec.container_name)
        },
        container_readonly: spec.container_readonly,
        container_mount_home: spec.container_mount_home,
        container_env: spec.container_env,
        container_entrypoint: if spec.container_entrypoint.is_empty() {
            None
        } else {
            Some(spec.container_entrypoint)
        },
        container_remap_root: spec.container_remap_root,
        burst_buffer: if spec.burst_buffer.is_empty() {
            None
        } else {
            Some(spec.burst_buffer)
        },
        begin_time: spec.begin_time.map(|ts| {
            chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
                .unwrap_or_else(chrono::Utc::now)
        }),
        deadline: spec.deadline.map(|ts| {
            chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
                .unwrap_or_else(chrono::Utc::now)
        }),
        spread_job: spec.spread_job,
        topology: if spec.topology.is_empty() {
            None
        } else {
            Some(spec.topology)
        },
        host_network: spec.host_network,
        privileged: spec.privileged,
        host_ipc: spec.host_ipc,
        shm_size: if spec.shm_size.is_empty() {
            None
        } else {
            Some(spec.shm_size)
        },
        extra_resources: spec.extra_resources,
        open_mode: if spec.open_mode.is_empty() {
            None
        } else {
            Some(spec.open_mode)
        },
    })
}

fn proto_to_resource_set(r: spur_proto::proto::ResourceSet) -> spur_core::resource::ResourceSet {
    spur_core::resource::ResourceSet {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        gpus: r
            .gpus
            .into_iter()
            .map(|g| spur_core::resource::GpuResource {
                device_id: g.device_id,
                gpu_type: g.gpu_type,
                memory_mb: g.memory_mb,
                peer_gpus: g.peer_gpus,
                link_type: match g.link_type {
                    1 => spur_core::resource::GpuLinkType::XGMI,
                    2 => spur_core::resource::GpuLinkType::NVLink,
                    _ => spur_core::resource::GpuLinkType::PCIe,
                },
            })
            .collect(),
        generic: r.generic,
    }
}

fn job_to_proto(job: &spur_core::job::Job) -> JobInfo {
    use spur_core::hostlist;

    JobInfo {
        job_id: job.job_id,
        name: job.spec.name.clone(),
        user: job.spec.user.clone(),
        uid: job.spec.uid,
        partition: job.spec.partition.clone().unwrap_or_default(),
        account: job.spec.account.clone().unwrap_or_default(),
        state: job.state.to_proto_i32(),
        state_reason: job.pending_reason.display().to_string(),
        submit_time: Some(datetime_to_proto(job.submit_time)),
        start_time: job.start_time.map(datetime_to_proto),
        end_time: job.end_time.map(datetime_to_proto),
        time_limit: job.spec.time_limit.map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
        run_time: job.run_time().map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
        num_nodes: job.spec.num_nodes,
        num_tasks: job.spec.num_tasks,
        cpus_per_task: job.spec.cpus_per_task,
        nodelist: if job.allocated_nodes.is_empty() {
            String::new()
        } else {
            hostlist::compress(&job.allocated_nodes)
        },
        work_dir: job.spec.work_dir.clone(),
        command: job
            .spec
            .script
            .as_deref()
            .map(|s| {
                s.lines()
                    .find(|l| !l.starts_with('#') && !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default(),
        exit_code: job.exit_code.unwrap_or(0),
        exit_signal: job.exit_signal,
        derived_exit_code: job.derived_exit_code,
        stdout_path: job.resolved_stdout(),
        stderr_path: job.resolved_stderr(),
        stdin_path: job.resolved_stdin().unwrap_or_default(),
        resources: job.allocated_resources.as_ref().map(allocations_to_proto),
        priority: job.priority,
        qos: job.spec.qos.clone().unwrap_or_default(),
        array_job_id: job.spec.array_job_id.unwrap_or(0),
        array_task_id: job.spec.array_task_id.unwrap_or(0),
        reservation: job.spec.reservation.clone().unwrap_or_default(),
        comment: job.spec.comment.clone().unwrap_or_default(),
    }
}

fn node_to_proto(node: &spur_core::node::Node) -> NodeInfo {
    NodeInfo {
        name: node.name.clone(),
        state: node.state.to_proto_i32(),
        state_reason: node.state_reason.clone().unwrap_or_default(),
        partitions: node.partitions.clone(),
        total_resources: Some(resource_to_proto(&node.total_resources)),
        alloc_resources: Some(allocations_to_proto(&node.alloc_resources)),
        arch: node.arch.clone(),
        os: node.os.clone(),
        cpu_load: node.cpu_load,
        free_memory_mb: node.free_memory_mb,
        boot_time: node.boot_time.map(datetime_to_proto),
        last_busy: node.last_busy.map(datetime_to_proto),
        slurmd_start_time: node.agent_start_time.map(datetime_to_proto),
        switch_name: node.switch_name.clone().unwrap_or_default(),
        active_reservation: String::new(),
        labels: node.labels.clone(),
        reservation_maint: false,
    }
}

fn partition_to_proto(part: &spur_core::partition::Partition) -> PartitionInfo {
    PartitionInfo {
        name: part.name.clone(),
        state: part.state.display().to_string(),
        is_default: part.is_default,
        total_nodes: 0,
        total_cpus: 0,
        nodes: part.nodes.clone(),
        max_time: part.max_time_minutes.map(|m| prost_types::Duration {
            seconds: m as i64 * 60,
            nanos: 0,
        }),
        default_time: part.default_time_minutes.map(|m| prost_types::Duration {
            seconds: m as i64 * 60,
            nanos: 0,
        }),
        max_nodes: part.max_nodes.unwrap_or(0),
        min_nodes: part.min_nodes,
        allow_root: part.allow_root,
        exclusive_user: part.exclusive_user,
        allow_accounts: part.allow_accounts.join(","),
        allow_groups: part.allow_groups.join(","),
        allow_qos: part.allow_qos.join(","),
        deny_accounts: part.deny_accounts.join(","),
        preempt_mode: format!("{:?}", part.preempt_mode),
        priority_tier: part.priority_tier,
    }
}

pub(crate) fn allocations_to_proto(
    r: &spur_core::resource::ResourceAllocations,
) -> spur_proto::proto::ResourceAllocations {
    use std::collections::HashMap;
    spur_proto::proto::ResourceAllocations {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        devices: r
            .devices
            .iter()
            .map(|(name, devs)| {
                (
                    name.clone(),
                    spur_proto::proto::DeviceAllocations {
                        devices: devs
                            .iter()
                            .map(|d| spur_proto::proto::AllocatedDevice {
                                device_id: d.device_id,
                                count: d.count,
                            })
                            .collect(),
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
    }
}

#[allow(dead_code)]
pub(crate) fn proto_to_allocations(
    r: spur_proto::proto::ResourceAllocations,
) -> spur_core::resource::ResourceAllocations {
    use std::collections::HashMap;
    spur_core::resource::ResourceAllocations {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        devices: r
            .devices
            .into_iter()
            .map(|(name, devs)| {
                (
                    name,
                    devs.devices
                        .into_iter()
                        .map(|d| spur_core::resource::AllocatedDevice {
                            device_id: d.device_id,
                            count: d.count,
                        })
                        .collect(),
                )
            })
            .collect::<HashMap<_, _>>(),
    }
}

pub(crate) fn resource_to_proto(
    r: &spur_core::resource::ResourceSet,
) -> spur_proto::proto::ResourceSet {
    spur_proto::proto::ResourceSet {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        gpus: r
            .gpus
            .iter()
            .map(|g| spur_proto::proto::GpuResource {
                device_id: g.device_id,
                gpu_type: g.gpu_type.clone(),
                memory_mb: g.memory_mb,
                peer_gpus: g.peer_gpus.clone(),
                link_type: match g.link_type {
                    spur_core::resource::GpuLinkType::XGMI => {
                        spur_proto::proto::GpuLinkType::GpuLinkXgmi as i32
                    }
                    spur_core::resource::GpuLinkType::NVLink => {
                        spur_proto::proto::GpuLinkType::GpuLinkNvlink as i32
                    }
                    spur_core::resource::GpuLinkType::PCIe => {
                        spur_proto::proto::GpuLinkType::GpuLinkPcie as i32
                    }
                },
            })
            .collect(),
        generic: r.generic.clone(),
    }
}

pub(crate) fn datetime_to_proto(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Whether a node passes a GetNodes request's name/partition filters. A `None`
/// name set matches every name; an empty `partition` matches every partition.
/// `allowed_names` is the caller-expanded Slurm hostlist. `partition` accepts a
/// Slurm-style comma-separated list, matching if any token names a node partition.
fn node_matches_filter(
    node: &spur_core::node::Node,
    allowed_names: Option<&HashSet<String>>,
    partition: &str,
) -> bool {
    if let Some(allowed) = allowed_names {
        if !allowed.contains(&node.name) {
            return false;
        }
    }
    let mut tokens = partition
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if partition.trim().is_empty() {
        return true;
    }
    tokens.any(|req| node.partitions.iter().any(|p| p == req))
}

fn annotate_nodes_with_reservations(
    nodes: &mut [NodeInfo],
    reservations: &[Reservation],
    now: DateTime<Utc>,
) {
    for node_info in nodes.iter_mut() {
        node_info.reservation_maint = false;
        node_info.active_reservation.clear();
        for res in reservations {
            if res.is_active(now) && res.covers_node(&node_info.name) {
                if node_info.active_reservation.is_empty() {
                    node_info.active_reservation = res.name.clone();
                }
                if res.flags.maint {
                    node_info.reservation_maint = true;
                }
            }
        }
    }
}

fn reservation_rpc_status(err: ReservationError) -> Status {
    match err {
        ReservationError::InvalidArgument(m) => Status::invalid_argument(m),
        ReservationError::NotFound(m) => Status::not_found(m),
        ReservationError::AlreadyExists(m) => Status::already_exists(m),
        ReservationError::Raft(m) => Status::internal(m),
    }
}

fn submit_rpc_status(err: crate::cluster::SubmitError) -> Status {
    match err {
        crate::cluster::SubmitError::InvalidArgument(m) => Status::invalid_argument(m),
        crate::cluster::SubmitError::Internal(m) => Status::internal(m),
    }
}

fn cluster_err_to_status(err: anyhow::Error) -> Status {
    if err.downcast_ref::<spur_core::auth::AuthError>().is_some() {
        return Status::permission_denied(err.to_string());
    }
    Status::internal(err.to_string())
}

fn cluster_err_to_precondition_status(err: anyhow::Error) -> Status {
    if err.downcast_ref::<spur_core::auth::AuthError>().is_some() {
        return Status::permission_denied(err.to_string());
    }
    Status::failed_precondition(err.to_string())
}

fn node_complete_to_status(err: NodeCompleteError) -> Status {
    let message = err.to_string();
    let code = match err {
        NodeCompleteError::JobNotFound { .. } => Code::NotFound,
        NodeCompleteError::NodeNotAllocated { .. } => Code::InvalidArgument,
        NodeCompleteError::RaftPropose { .. } => Code::Unavailable,
    };
    Status::new(code, message)
}

#[allow(clippy::result_large_err)]
fn validate_completion_report_state_for_rpc(
    state: spur_core::job::JobState,
    exit_code: i32,
) -> Result<(), Status> {
    spur_core::job::JobState::validate_completion_report_state(state, exit_code)
        .map_err(|e| Status::invalid_argument(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use spur_core::job::{JobState, NodeCompleteError};
    use spur_core::reservation::ReservationFlags;
    use tonic::Code;

    #[test]
    fn submit_rpc_status_maps_invalid_argument() {
        use crate::cluster::SubmitError;

        let status = submit_rpc_status(SubmitError::invalid("partition 'gpu' not found"));
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "partition 'gpu' not found");
    }

    #[test]
    fn submit_rpc_status_maps_internal() {
        use crate::cluster::SubmitError;

        let status = submit_rpc_status(SubmitError::internal("raft propose failed"));
        assert_eq!(status.code(), Code::Internal);
    }

    fn make_node_info(name: &str) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            ..Default::default()
        }
    }

    fn make_reservation(
        name: &str,
        nodes: &[&str],
        start_offset_hours: i64,
        end_offset_hours: i64,
    ) -> Reservation {
        let now = Utc::now();
        Reservation {
            name: name.into(),
            start_time: now + Duration::hours(start_offset_hours),
            end_time: now + Duration::hours(end_offset_hours),
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            accounts: Vec::new(),
            users: Vec::new(),
            flags: Default::default(),
        }
    }

    fn core_node(name: &str, partition: &str) -> spur_core::node::Node {
        let mut n =
            spur_core::node::Node::new(name.into(), spur_core::resource::ResourceSet::default());
        n.partitions = vec![partition.into()];
        n
    }

    fn names(pattern: &str) -> Option<HashSet<String>> {
        let pattern = pattern.trim();
        (!pattern.is_empty()).then(|| {
            spur_sched::node_match::expand_hostlist_or_split(pattern)
                .into_iter()
                .collect()
        })
    }

    #[test]
    fn test_node_filter_empty_matches_all() {
        let n = core_node("gpu01", "batch");
        assert!(node_matches_filter(&n, names("").as_ref(), ""));
        assert!(node_matches_filter(&n, names("  ").as_ref(), "  "));
    }

    #[test]
    fn test_node_filter_by_name() {
        let n = core_node("gpu01", "batch");
        assert!(node_matches_filter(&n, names("gpu01").as_ref(), ""));
        assert!(!node_matches_filter(&n, names("gpu02").as_ref(), ""));
    }

    #[test]
    fn test_node_filter_hostlist_bracket() {
        let n = core_node("gpu03", "batch");
        assert!(node_matches_filter(&n, names("gpu[01-04]").as_ref(), ""));
        assert!(!node_matches_filter(&n, names("gpu[05-08]").as_ref(), ""));
    }

    #[test]
    fn test_node_filter_by_partition() {
        let n = core_node("gpu01", "batch");
        assert!(node_matches_filter(&n, names("").as_ref(), "batch"));
        assert!(!node_matches_filter(&n, names("").as_ref(), "debug"));
    }

    #[test]
    fn test_node_filter_name_and_partition_both_apply() {
        let n = core_node("gpu01", "batch");
        // Right name, wrong partition -> excluded.
        assert!(!node_matches_filter(&n, names("gpu01").as_ref(), "debug"));
        assert!(node_matches_filter(&n, names("gpu01").as_ref(), "batch"));
    }

    #[test]
    fn test_node_filter_partition_comma_list() {
        let n = core_node("gpu01", "gpu");
        assert!(node_matches_filter(&n, names("").as_ref(), "gpu,cpu"));
        assert!(!node_matches_filter(&n, names("").as_ref(), "cpu,fpga"));
    }

    #[test]
    fn test_annotate_no_reservations() {
        let mut nodes = vec![make_node_info("n1"), make_node_info("n2")];
        annotate_nodes_with_reservations(&mut nodes, &[], Utc::now());
        assert!(nodes[0].active_reservation.is_empty());
        assert!(nodes[1].active_reservation.is_empty());
    }

    #[test]
    fn test_annotate_active_reservation() {
        let mut nodes = vec![make_node_info("n1"), make_node_info("n2")];
        let reservations = vec![make_reservation("maint", &["n1"], -1, 1)];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert_eq!(nodes[0].active_reservation, "maint");
        assert!(nodes[1].active_reservation.is_empty());
    }

    #[test]
    fn test_annotate_expired_reservation() {
        let mut nodes = vec![make_node_info("n1")];
        let reservations = vec![make_reservation("old", &["n1"], -3, -1)];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert!(nodes[0].active_reservation.is_empty());
    }

    #[test]
    fn test_annotate_future_reservation() {
        let mut nodes = vec![make_node_info("n1")];
        let reservations = vec![make_reservation("future", &["n1"], 1, 3)];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert!(nodes[0].active_reservation.is_empty());
    }

    #[test]
    fn test_annotate_partial_coverage() {
        let mut nodes = vec![
            make_node_info("n1"),
            make_node_info("n2"),
            make_node_info("n3"),
        ];
        let reservations = vec![make_reservation("gpu-resv", &["n1", "n3"], -1, 1)];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert_eq!(nodes[0].active_reservation, "gpu-resv");
        assert!(nodes[1].active_reservation.is_empty());
        assert_eq!(nodes[2].active_reservation, "gpu-resv");
    }

    #[test]
    fn test_annotate_multiple_reservations_first_wins() {
        let mut nodes = vec![make_node_info("n1")];
        let reservations = vec![
            make_reservation("first", &["n1"], -1, 1),
            make_reservation("second", &["n1"], -1, 1),
        ];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert_eq!(nodes[0].active_reservation, "first");
    }

    #[test]
    fn test_annotate_maint_flag_with_overlapping_reservations() {
        let mut nodes = vec![make_node_info("n1")];
        let now = Utc::now();
        let plain = Reservation {
            name: "plain".into(),
            start_time: now - Duration::hours(1),
            end_time: now + Duration::hours(1),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags {
                overlap: true,
                ..Default::default()
            },
        };
        let maint = Reservation {
            name: "maint".into(),
            start_time: now - Duration::hours(1),
            end_time: now + Duration::hours(1),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags {
                maint: true,
                overlap: true,
                ..Default::default()
            },
        };
        let reservations = vec![plain, maint];
        annotate_nodes_with_reservations(&mut nodes, &reservations, Utc::now());
        assert_eq!(nodes[0].active_reservation, "plain");
        assert!(nodes[0].reservation_maint);
    }

    #[test]
    fn node_complete_error_status_mapping_covers_all_variants() {
        let cases: Vec<(NodeCompleteError, Code, bool)> = vec![
            (
                NodeCompleteError::JobNotFound { job_id: 1 },
                Code::NotFound,
                false,
            ),
            (
                NodeCompleteError::NodeNotAllocated {
                    job_id: 1,
                    node: "n1".into(),
                },
                Code::InvalidArgument,
                false,
            ),
            (
                NodeCompleteError::RaftPropose {
                    source: anyhow::anyhow!("test"),
                },
                Code::Unavailable,
                true,
            ),
        ];

        for (err, want_code, want_retry) in cases {
            assert_eq!(err.retryable(), want_retry, "{err:?}");
            let retry = err.retryable();
            let status = node_complete_to_status(err);
            assert_eq!(status.code(), want_code);
            let agent_retryable = matches!(
                status.code(),
                Code::Unavailable | Code::Internal | Code::DeadlineExceeded | Code::Unknown
            );
            assert_eq!(retry, agent_retryable, "{status:?}");
        }
    }

    #[test]
    fn completion_report_state_accepts_completed_zero() {
        assert!(validate_completion_report_state_for_rpc(JobState::Completed, 0).is_ok());
    }

    #[test]
    fn completion_report_state_accepts_failed_nonzero() {
        assert!(validate_completion_report_state_for_rpc(JobState::Failed, 42).is_ok());
    }

    // A signaled job is reported as (Completed, exit_code=0); the validator must
    // accept it (controller rederives Failed from the signal). See agent_server.rs.
    #[test]
    fn completion_report_state_accepts_signaled_completed_zero() {
        assert!(validate_completion_report_state_for_rpc(JobState::Completed, 0).is_ok());
    }

    #[test]
    fn completion_report_state_rejects_completed_nonzero() {
        let err = validate_completion_report_state_for_rpc(JobState::Completed, 1).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("does not match exit_code"));
    }

    #[test]
    fn completion_report_state_rejects_cancelled() {
        let err = validate_completion_report_state_for_rpc(JobState::Cancelled, 0).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("invalid completion state"));
    }

    #[test]
    fn completion_report_state_rejects_completing() {
        let err = validate_completion_report_state_for_rpc(JobState::Completing, 0).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("invalid completion state"));
    }

    #[test]
    fn completion_report_state_rejects_running() {
        let err = validate_completion_report_state_for_rpc(JobState::Running, 0).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("invalid completion state"));
    }
}
