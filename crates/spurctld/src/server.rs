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
use spur_core::mpi::MPI_PMIX;
use spur_core::reservation::Reservation;
use spur_core::task_launch::{
    batch_script_uses_step_launch, build_step_task_plan, step_needs_pmix_prepare,
};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::slurm_controller_server::SlurmController;
use spur_proto::proto::*;

use crate::cluster::{ClusterManager, PartitionError, ReservationError};
use crate::pmix_dispatch::{self, PmixPrepareNode};
use crate::raft::RaftHandle;
use crate::rpc_middleware::RpcStatsLayer;
use crate::rpc_stats::RpcStatsCollector;
use crate::sched_stats::SchedStatsCollector;

const FORWARDED_HEADER: &str = "x-spur-forwarded";
const LEADER_HEADER: &str = "x-spur-leader";

/// Resolve the comm address for an agent registration.
///
/// Tries the advertised address first, then the gRPC peer IP (dynamic registration
/// fallback). Loopback or link-local results are deferred when a routable
/// candidate is also available.
fn resolve_registration_comm_addr(
    advertised: &str,
    remote_addr: &str,
    reject_loopback: bool,
) -> Result<String, Status> {
    let mut candidates = Vec::new();
    if !advertised.is_empty() {
        candidates.push(advertised);
    }
    if !remote_addr.is_empty() && remote_addr != advertised {
        candidates.push(remote_addr);
    }

    if candidates.is_empty() {
        return Err(Status::invalid_argument(
            "no comm address available for registration",
        ));
    }

    let mut last_err = None;
    let mut unusable_result = None;
    for candidate in candidates {
        match spur_net::validate_comm_address(candidate, reject_loopback) {
            Ok(addr) if spur_net::normalized_comm_addr_is_unusable(&addr) => {
                if unusable_result.is_none() {
                    unusable_result = Some((candidate.to_string(), addr));
                }
            }
            Ok(addr) => {
                if candidate != addr {
                    info!(
                        candidate = %candidate,
                        comm_addr = %addr,
                        "normalized comm address for registration"
                    );
                }
                return Ok(addr);
            }
            Err(e) => last_err = Some(e),
        }
    }

    if let Some((candidate, addr)) = unusable_result {
        warn!(
            comm_addr = %addr,
            candidate = %candidate,
            "node registered with loopback or link-local comm address"
        );
        return Ok(addr);
    }

    Err(Status::invalid_argument(match last_err {
        Some(e) => format!("invalid comm address: {e}"),
        None => "no comm address candidate resolved".into(),
    }))
}

fn node_comm_http_url(node: &spur_core::node::Node, node_name: &str) -> Result<String, Status> {
    let host = node
        .comm_addr()
        .ok_or_else(|| Status::unavailable(format!("node {node_name} has no comm address")))?;
    Ok(spur_net::format_comm_http_url(host, node.port))
}

fn node_comm_socket(node: &spur_core::node::Node, node_name: &str) -> Result<String, Status> {
    let host = node
        .comm_addr()
        .ok_or_else(|| Status::unavailable(format!("node {node_name} has no comm address")))?;
    Ok(spur_net::format_comm_socket(host, node.port))
}

/// Forwarding decision for read RPCs, split out so it's unit-testable.
fn read_forwarding_policy(is_leader: bool, is_forwarded: bool) -> bool {
    !is_leader && !is_forwarded
}

pub struct ControllerService {
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
    leader_proxy: LeaderProxy,
    /// Node ID → client API address (host:6817) for the x-spur-leader header.
    client_addrs: BTreeMap<u64, String>,
    rpc_stats: Arc<RpcStatsCollector>,
    sched_stats: Arc<SchedStatsCollector>,
    /// Default HA control-plane count (`[cluster] control_plane_replicas`) when `spur k8s up`
    /// requests neither `--replicas` nor an explicit node set.
    control_plane_replicas: u32,
    /// JWT signing key for node tokens, captured at startup. Deliberately NOT
    /// re-read on `scontrol reconfigure`: swapping it live would instantly fail
    /// verification of every outstanding node token (7-day TTL), silently
    /// partitioning healthy nodes. Like Slurm's AuthType, it is restart-only.
    jwt_key: String,
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

    /// `None` instead of an error when no leader is reachable, so read RPCs can
    /// fall back to local state rather than failing.
    async fn try_get_leader_client(
        &self,
    ) -> Option<SlurmControllerClient<tonic::transport::Channel>> {
        self.get_leader_client().await.ok()
    }
}

/// Resolve the node-token signing key from config at startup. Captured once by
/// `serve` into `ControllerService::jwt_key`; deliberately not re-read on
/// `reconfigure` (see the field doc). Falls back to a shared default so
/// key-less dev clusters interoperate.
fn resolve_startup_jwt_key(config: &spur_core::config::SlurmConfig) -> String {
    if let Some(key) = &config.auth.jwt_key {
        return key.clone();
    }
    // Token admission signs/verifies node tokens with this key. A well-known
    // default is trivially forgeable by anyone who can reach the controller.
    if matches!(
        config.admission.mode,
        spur_core::config::AdmissionMode::Token
    ) {
        warn!(
            "admission.mode=Token but auth.jwt_key is unset: node tokens are signed with a \
             well-known default key and are forgeable. Set auth.jwt_key to a secret value."
        );
    }
    "spur-default-key".to_string()
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

    /// Reconcile `node`'s reported held jobs against the controller's record.
    /// Spawned, best-effort; called from the (leader-gated) heartbeat handler.
    fn reconcile_reported_allocations(&self, node: &str, reported: &[RunningJobStatus]) {
        let kill = reported
            .iter()
            .filter(|r| should_kill_reported_job(&self.cluster, r.job_id, node))
            .map(|r| r.job_id)
            .collect::<Vec<_>>();
        if !kill.is_empty() {
            let cluster = self.cluster.clone();
            let node_owned = node.to_string();
            tokio::spawn(async move {
                for job_id in kill {
                    // Re-check: a requeue or reattach since the snapshot above
                    // would otherwise send an unguarded cancel into a live job.
                    if !should_kill_reported_job(&cluster, job_id, &node_owned) {
                        continue;
                    }
                    warn!(
                        job_id,
                        node = %node_owned,
                        "agent holds a job the controller doesn't bind here — reclaiming its allocation"
                    );
                    // A requeue may have advanced the controller job to a newer
                    // run while this node still holds the cancelled one.
                    let run_attempt = cluster
                        .pending_kill_run_attempt(job_id, &node_owned)
                        .or_else(|| cluster.get_job(job_id).map(|j| j.run_attempt))
                        .unwrap_or(0);
                    crate::scheduler_loop::cancel_job_on_nodes(
                        &cluster,
                        job_id,
                        std::slice::from_ref(&node_owned),
                        0,
                        run_attempt,
                    )
                    .await;
                }
            });
        }

        let reported_ids: HashSet<u32> = reported.iter().map(|r| r.job_id).collect();
        {
            let cluster = self.cluster.clone();
            let node_owned = node.to_string();
            let reported_ids = reported_ids.clone();
            tokio::spawn(async move {
                if let Err(e) = cluster.clear_confirmed_pending_kills(&node_owned, &reported_ids) {
                    warn!(node = %node_owned, error = %e, "failed to persist pending-kill release confirmation");
                }
            });
        }
        let active = self.cluster.active_jobs_on_node(node);
        // Sweep entries for jobs that left the active set by another path,
        // else one that's never re-reported leaks its streak entry.
        let active_ids: HashSet<spur_core::job::JobId> = active.iter().map(|j| j.job_id).collect();
        self.cluster.prune_phantom_streaks_not_in(node, &active_ids);

        let mut phantom = Vec::new();
        for job in active {
            if reported_ids.contains(&job.job_id) {
                self.cluster.note_node_reported_job(job.job_id, node);
            } else if job.allocated_nodes.len() == 1 // evict_job fails the whole job
                && self.cluster.note_node_omitted_job(job.job_id, node)
            {
                phantom.push(job);
            }
        }
        if !phantom.is_empty() {
            let cluster = self.cluster.clone();
            let node_owned = node.to_string();
            tokio::spawn(async move {
                for job in phantom {
                    // Re-fetch: the snapshot may be stale by now — a fresh
                    // run_attempt means a legitimate requeue, not a phantom.
                    let Some(fresh) = cluster.get_job(job.job_id) else {
                        continue;
                    };
                    if fresh.state.is_terminal() || fresh.run_attempt != job.run_attempt {
                        continue;
                    }
                    warn!(
                        job_id = fresh.job_id,
                        node = %node_owned,
                        "node's heartbeat repeatedly omitted a job the controller binds here — evicting"
                    );
                    match cluster.evict_job(
                        fresh.job_id,
                        spur_core::job::PendingReason::NodeDown,
                        fresh.run_attempt,
                    ) {
                        Ok(finalized) if finalized.is_empty() => {}
                        Ok(finalized) => {
                            cluster.complete_evicted_steps(&finalized);
                            crate::scheduler_loop::send_cancel_to_agents(&cluster, &fresh, 9).await;
                        }
                        Err(e) => {
                            warn!(job_id = fresh.job_id, error = %e, "failed to evict phantom binding");
                        }
                    }
                }
            });
        }
    }

    /// Reads never require the leader (every node applies the committed log),
    /// so forwarding is only an optional freshness hop. Skipping already-
    /// forwarded requests avoids forward loops between non-leaders.
    fn read_should_forward<T>(&self, request: &Request<T>) -> bool {
        read_forwarding_policy(
            self.raft.is_leader(),
            request.metadata().get(FORWARDED_HEADER).is_some(),
        )
    }

    /// Best-effort forward of a read to the leader: `Some(payload)` forwards
    /// (clone lazily via `forward.then(|| ...)`), `None` serves local state. Any
    /// forward error is swallowed to local — safe only while read handlers return
    /// Ok/NotFound; a handler with a real error (e.g. InvalidArgument) masks it.
    async fn forward_read_optional<T, R, F, Fut>(
        &self,
        payload: Option<T>,
        rpc: &str,
        call: F,
    ) -> Option<Response<R>>
    where
        F: FnOnce(SlurmControllerClient<tonic::transport::Channel>, Request<T>) -> Fut,
        Fut: std::future::Future<Output = Result<Response<R>, Status>>,
    {
        let payload = payload?;
        let client = self.leader_proxy.try_get_leader_client().await?;
        let mut fwd = Request::new(payload);
        *fwd.metadata_mut() = Self::forwarded_metadata();
        match call(client, fwd).await {
            Ok(resp) => Some(resp),
            Err(e) => {
                warn!("forwarding {rpc} to leader failed, serving locally: {e}");
                None
            }
        }
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

        if !matches!(self.cluster.config().admission.mode, AdmissionMode::Token) {
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

        spur_core::admission::generate_node_token(hostname, self.jwt_key.as_bytes())
            .map_err(|e| Status::internal(e.to_string()))
    }
}

/// Resolve a user to the (namespace, ServiceAccount) its scoped kubeconfig must be bound to.
/// Fails closed if associations aren't loaded yet — the cache resolves fail-open, which would
/// otherwise mint an unscoped token — and rejects a user with no account.
fn resolve_user_namespace_sa(
    cache: &crate::association_cache::AssociationCache,
    user: &str,
) -> Result<(String, String), Status> {
    if !cache.is_loaded() {
        return Err(Status::unavailable(
            "associations not loaded yet; retry shortly",
        ));
    }
    let (account, _qos, _allowed_qos) = cache.resolve(user, None);
    let account = account.ok_or_else(|| {
        Status::not_found(format!("user '{user}' is not associated with any account"))
    })?;
    Ok((
        spur_core::quota_names::account_namespace(&account),
        spur_core::quota_names::user_service_account(user),
    ))
}

/// Whether `caller` may perform k0s cluster-admin ops: empty/root always, else accounting `Admin`.
/// Fails closed when accounting is off (cache reports no admins), leaving only root/internal.
fn is_k0s_admin(cache: &crate::association_cache::AssociationCache, caller: &str) -> bool {
    caller.is_empty() || caller == "root" || cache.is_admin(caller)
}

/// Whether a job `node` reports holding should be killed there: unknown,
/// terminal, or Running/Suspended/Completing but unbound — never fabricating.
fn should_kill_reported_job(cluster: &ClusterManager, job_id: u32, node: &str) -> bool {
    use spur_core::job::JobState;
    match cluster.job_state(job_id) {
        None => true,
        Some(state) if state.is_terminal() => true,
        Some(JobState::Running) | Some(JobState::Suspended) | Some(JobState::Completing) => {
            !cluster
                .get_job(job_id)
                .is_some_and(|j| j.allocated_nodes.iter().any(|n| n == node))
        }
        Some(JobState::Pending) => cluster.has_pending_kill(job_id, node),
        Some(_) => false,
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
        let outcome = self
            .cluster
            .submit_job(core_spec)
            .map_err(submit_rpc_status)?;

        Ok(Response::new(SubmitJobResponse {
            job_id: outcome.job_id,
            warnings: outcome.warnings,
        }))
    }

    async fn get_jobs(
        &self,
        request: Request<GetJobsRequest>,
    ) -> Result<Response<GetJobsResponse>, Status> {
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(
                forward.then(|| req.clone()),
                "get_jobs",
                |mut c, r| async move { c.get_jobs(r).await },
            )
            .await
        {
            return Ok(resp);
        }

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
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(forward.then_some(req), "get_job", |mut c, r| async move {
                c.get_job(r).await
            })
            .await
        {
            return Ok(resp);
        }

        let job_id = req.job_id;
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

        self.cluster
            .check_cancel_allowed(job_id, &req.user)
            .map_err(cluster_err_to_status)?;

        // Snapshot before cancelling so we still have allocated_nodes after.
        let job = self.cluster.get_job(job_id);

        if let Err(e) = self.cluster.cancel_job(job_id, &req.user) {
            return Err(cluster_err_to_status(e));
        }

        // Send cancel signal to agents so the process is actually killed
        if let Some(job) = job {
            let cluster = self.cluster.clone();
            tokio::spawn(async move {
                crate::scheduler_loop::send_cancel_to_agents(&cluster, &job, 0).await;
            });
        }

        Ok(Response::new(()))
    }

    async fn complete_job(
        &self,
        request: Request<CompleteJobRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.complete_job(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward complete_job to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();
        let job = self
            .cluster
            .finish_srun_job(req.job_id, req.exit_code, &req.user)
            .map_err(|e| match e {
                crate::cluster::SrunCompleteError::NotFound(id) => {
                    Status::not_found(format!("job {id} not found"))
                }
                crate::cluster::SrunCompleteError::NotSrunJob(id) => {
                    Status::failed_precondition(format!("job {id} is not an srun allocation"))
                }
                crate::cluster::SrunCompleteError::NotStepDispatch(id) => {
                    Status::failed_precondition(format!(
                        "job {id} does not use native step dispatch"
                    ))
                }
                crate::cluster::SrunCompleteError::AlreadyTerminal { job_id, state } => {
                    Status::failed_precondition(format!("job {job_id} is already {state:?}"))
                }
                crate::cluster::SrunCompleteError::NotOwner { job_id, user } => {
                    Status::permission_denied(format!(
                        "user {user} is not permitted to complete job {job_id}"
                    ))
                }
                crate::cluster::SrunCompleteError::Internal { job_id, message } => {
                    Status::internal(format!("job {job_id}: {message}"))
                }
            })?;

        let cluster = self.cluster.clone();
        tokio::spawn(async move {
            crate::scheduler_loop::release_srun_allocation_on_agents(&cluster, &job).await;
        });

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
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(
                forward.then(|| req.clone()),
                "get_nodes",
                |mut c, r| async move { c.get_nodes(r).await },
            )
            .await
        {
            return Ok(resp);
        }

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
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(
                forward.then(|| req.clone()),
                "get_node",
                |mut c, r| async move { c.get_node(r).await },
            )
            .await
        {
            return Ok(resp);
        }

        let name = req.name;
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
            self.cluster.config().admission.mode,
            spur_core::config::AdmissionMode::Token
        ) {
            if req.node_token.is_empty() {
                return Err(Status::unauthenticated("node token required"));
            }
            let identity =
                spur_core::admission::verify_node_token(&req.node_token, self.jwt_key.as_bytes())
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
        let forward = self.read_should_forward(&request);
        if let Some(resp) = self
            .forward_read_optional(
                forward.then(|| request.into_inner()),
                "get_partitions",
                |mut c, r| async move { c.get_partitions(r).await },
            )
            .await
        {
            return Ok(resp);
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
            .config()
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
        let forward = self.read_should_forward(&request);
        if let Some(resp) = self
            .forward_read_optional(
                forward.then_some(()),
                "get_job_metrics",
                |mut c, r| async move { c.get_job_metrics(r).await },
            )
            .await
        {
            return Ok(resp);
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
        let forward = self.read_should_forward(&request);
        if let Some(resp) = self
            .forward_read_optional(
                forward.then_some(()),
                "get_node_metrics",
                |mut c, r| async move { c.get_node_metrics(r).await },
            )
            .await
        {
            return Ok(resp);
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

        let reject_loopback = self.cluster.config().network.reject_loopback_comm_addr;
        let advertised = req.address.clone();
        let agent_addr = tokio::task::spawn_blocking(move || {
            resolve_registration_comm_addr(&advertised, &remote_addr, reject_loopback)
        })
        .await
        .map_err(|e| Status::internal(format!("comm address resolution task failed: {e}")))??;

        let agent_port = if req.port > 0 { req.port as u16 } else { 6818 };

        let node_token_response = self.validate_admission(&req.join_token, &req.hostname)?;

        let source = spur_core::node::node_source_from_registration(&req.version, &req.labels);
        self.cluster
            .register_node(
                // NodeName and NodeHostname are the same until agents can supply both.
                req.hostname.clone(),
                req.hostname.clone(),
                resources,
                agent_addr,
                agent_port,
                req.wg_pubkey,
                req.version,
                source,
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
                req.run_attempt,
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
            Some(Ok(NodeCompleteResult::Completing)) => {
                if let Some(job) = self.cluster.get_job(req.job_id) {
                    if job
                        .spec
                        .script
                        .as_deref()
                        .is_some_and(batch_script_uses_step_launch)
                    {
                        let missing: Vec<String> = job
                            .allocated_nodes
                            .iter()
                            .filter(|node| !job.node_completions.contains_key(*node))
                            .cloned()
                            .collect();
                        if !missing.is_empty() {
                            let cluster = self.cluster.clone();
                            let job_id = req.job_id;
                            tokio::spawn(async move {
                                crate::scheduler_loop::cancel_job_on_nodes(
                                    &cluster, job_id, &missing, 15,
                                )
                                .await;
                            });
                        }
                    }
                }
                Ok(Response::new(()))
            }
            Some(Ok(NodeCompleteResult::AlreadyTerminal)) => {
                warn!(
                    job_id = req.job_id,
                    node = %req.reporting_node,
                    "duplicate completion report for terminal job"
                );
                Ok(Response::new(()))
            }
            Some(Ok(NodeCompleteResult::StaleReport)) => {
                warn!(
                    job_id = req.job_id,
                    node = %req.reporting_node,
                    run_attempt = req.run_attempt,
                    "ignoring completion report from superseded run"
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
            self.cluster.config().admission.mode,
            spur_core::config::AdmissionMode::Token
        ) {
            if req.node_token.is_empty() {
                return Err(Status::unauthenticated("node token required"));
            }
            let identity =
                spur_core::admission::verify_node_token(&req.node_token, self.jwt_key.as_bytes())
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
            self.reconcile_reported_allocations(&req.hostname, &req.running_jobs);
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
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(
                forward.then_some(req),
                "get_job_steps",
                |mut c, r| async move { c.get_job_steps(r).await },
            )
            .await
        {
            return Ok(resp);
        }

        let job_id = req.job_id;
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

        spur_core::auth::check_job_owner(&req.user, &job.spec.user, "attach to")
            .map_err(|e| Status::permission_denied(e.to_string()))?;

        if job.state != spur_core::job::JobState::Running {
            return Err(Status::failed_precondition(format!(
                "job {} is not running (state: {:?})",
                job_id, job.state
            )));
        }

        let target_node =
            select_step_node(&job.allocated_nodes, &req.node).map_err(Status::invalid_argument)?;

        // Resolve the target agent address BEFORE creating the step: an unregistered node returns a
        // retryable Unavailable and the client retries the whole call, so resolving first keeps a
        // failed attempt from leaking a step per retry.
        let node = self.cluster.get_node(target_node).ok_or_else(|| {
            Status::unavailable(format!("node {target_node} is not currently registered"))
        })?;
        let node_addr = node_comm_socket(&node, target_node)?;

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

        self.cluster
            .create_step(step)
            .map_err(|e| Status::internal(format!("failed to create job step: {e}")))?;

        Ok(Response::new(CreateJobStepResponse { step_id, node_addr }))
    }

    async fn create_partition(
        &self,
        request: Request<CreatePartitionRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.create_partition(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward create_partition to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();

        if req.nodes.is_empty() && req.selector.is_empty() {
            return Err(Status::invalid_argument(
                "partition must specify at least one of nodes or selector",
            ));
        }

        let max_time_minutes = if req.max_time.is_empty()
            || req.max_time.eq_ignore_ascii_case("INFINITE")
            || req.max_time.eq_ignore_ascii_case("UNLIMITED")
        {
            None
        } else {
            Some(
                spur_core::config::parse_time_minutes(&req.max_time).ok_or_else(|| {
                    Status::invalid_argument(format!("invalid max_time: {}", req.max_time))
                })?,
            )
        };

        let default_time_minutes = if req.default_time.is_empty() {
            None
        } else {
            Some(
                spur_core::config::parse_time_minutes(&req.default_time).ok_or_else(|| {
                    Status::invalid_argument(format!("invalid default_time: {}", req.default_time))
                })?,
            )
        };

        let state = match req.state.to_uppercase().as_str() {
            "" | "UP" => spur_core::partition::PartitionState::Up,
            "DOWN" => spur_core::partition::PartitionState::Down,
            "DRAIN" => spur_core::partition::PartitionState::Drain,
            "INACTIVE" => spur_core::partition::PartitionState::Inactive,
            other => {
                return Err(Status::invalid_argument(format!(
                    "unknown partition state '{}'; expected UP, DOWN, DRAIN, or INACTIVE",
                    other
                )))
            }
        };

        let preempt_mode = match req.preempt_mode.to_uppercase().as_str() {
            "" | "OFF" => spur_core::partition::PreemptMode::Off,
            "CANCEL" => spur_core::partition::PreemptMode::Cancel,
            "REQUEUE" => spur_core::partition::PreemptMode::Requeue,
            "SUSPEND" => spur_core::partition::PreemptMode::Suspend,
            other => {
                return Err(Status::invalid_argument(format!(
                    "unknown preempt_mode '{}'; expected OFF, CANCEL, REQUEUE, or SUSPEND",
                    other
                )))
            }
        };

        let partition = spur_core::partition::Partition {
            name: req.name,
            state,
            is_default: req.is_default,
            nodes: req.nodes,
            selector: req.selector.into_iter().collect(),
            max_time_minutes,
            default_time_minutes,
            // A literal 0 means "no limit" (matches the update-partition
            // contract) rather than a partition that can never run a job.
            max_nodes: req.max_nodes.filter(|&n| n != 0),
            min_nodes: if req.min_nodes == 0 { 1 } else { req.min_nodes },
            allow_accounts: req.allow_accounts,
            allow_groups: req.allow_groups,
            deny_accounts: req.deny_accounts,
            deny_qos: req.deny_qos,
            allow_qos: req.allow_qos,
            priority_tier: req.priority_tier,
            preempt_mode,
            ..Default::default()
        };

        self.cluster
            .create_partition(partition)
            .map_err(partition_rpc_status)?;

        Ok(Response::new(()))
    }

    async fn update_partition(
        &self,
        request: Request<UpdatePartitionRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.update_partition(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward update_partition to leader: {e}");
                    return Err(status);
                }
            }
        }

        let req = request.into_inner();

        let state = req
            .state
            .map(|s| match s.to_uppercase().as_str() {
                v @ ("UP" | "DOWN" | "DRAIN" | "INACTIVE") => Ok(v.to_string()),
                other => Err(Status::invalid_argument(format!(
                    "unknown partition state '{}'; expected UP, DOWN, DRAIN, or INACTIVE",
                    other
                ))),
            })
            .transpose()?;

        let preempt_mode = req
            .preempt_mode
            .map(|pm| match pm.to_uppercase().as_str() {
                v @ ("OFF" | "CANCEL" | "REQUEUE" | "SUSPEND") => Ok(v.to_string()),
                other => Err(Status::invalid_argument(format!(
                    "unknown preempt_mode '{}'; expected OFF, CANCEL, REQUEUE, or SUSPEND",
                    other
                ))),
            })
            .transpose()?;

        let max_time = req.max_time;
        let allow_accounts = if req.set_allow_accounts {
            Some(req.allow_accounts)
        } else {
            None
        };
        let allow_groups = if req.set_allow_groups {
            Some(req.allow_groups)
        } else {
            None
        };
        let deny_accounts = if req.set_deny_accounts {
            Some(req.deny_accounts)
        } else {
            None
        };
        let deny_qos = if req.set_deny_qos {
            Some(req.deny_qos)
        } else {
            None
        };
        let allow_qos = if req.set_allow_qos {
            Some(req.allow_qos)
        } else {
            None
        };
        let (max_nodes, clear_max_nodes) =
            resolve_max_nodes_update(req.max_nodes_value, req.clear_max_nodes);

        let selector = if req.set_selector || !req.selector.is_empty() {
            Some(req.selector.into_iter().collect())
        } else {
            None
        };
        // Match create_partition's "0 means unset, not literally zero" rule.
        let min_nodes = req.min_nodes.map(|mn| if mn == 0 { 1 } else { mn });

        self.cluster
            .update_partition(
                &req.name,
                req.nodes,
                selector,
                state,
                req.is_default,
                max_time,
                req.default_time,
                max_nodes,
                clear_max_nodes,
                min_nodes,
                allow_accounts,
                allow_groups,
                deny_accounts,
                deny_qos,
                allow_qos,
                req.priority_tier,
                preempt_mode,
            )
            .map_err(partition_rpc_status)?;

        Ok(Response::new(()))
    }

    async fn delete_partition(
        &self,
        request: Request<DeletePartitionRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(request.into_inner());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.delete_partition(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward delete_partition to leader: {e}");
                    return Err(status);
                }
            }
        }

        let name = request.into_inner().name;
        self.cluster
            .delete_partition(&name)
            .map_err(partition_rpc_status)?;

        Ok(Response::new(()))
    }

    async fn reconfigure(&self, request: Request<()>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            let proxy = &self.leader_proxy;
            match proxy.get_leader_client().await {
                Ok(mut client) => {
                    let mut fwd = Request::new(());
                    *fwd.metadata_mut() = Self::forwarded_metadata();
                    return client.reconfigure(fwd).await;
                }
                Err(e) => {
                    warn!("failed to forward reconfigure to leader: {e}");
                    return Err(status);
                }
            }
        }

        self.cluster
            .reconfigure()
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
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
            owner: req.user,
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
                &req.user,
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

        let req = request.into_inner();
        self.cluster
            .delete_reservation(&req.name, &req.user)
            .map_err(reservation_rpc_status)?;
        Ok(Response::new(()))
    }

    async fn list_reservations(
        &self,
        request: Request<ListReservationsRequest>,
    ) -> Result<Response<ListReservationsResponse>, Status> {
        let forward = self.read_should_forward(&request);
        let req = request.into_inner();
        if let Some(resp) = self
            .forward_read_optional(
                forward.then(|| req.clone()),
                "list_reservations",
                |mut c, r| async move { c.list_reservations(r).await },
            )
            .await
        {
            return Ok(resp);
        }
        let name = req.name.trim();
        let reservations = self.cluster.get_reservations();
        let now = Utc::now();
        let infos: Vec<ReservationInfo> = reservations
            .iter()
            .filter(|r| name.is_empty() || r.name == name)
            .map(|r| ReservationInfo {
                name: r.name.clone(),
                start_time: r.start_time.to_rfc3339(),
                end_time: r.end_time.to_rfc3339(),
                nodes: r.nodes.join(","),
                accounts: r.accounts.join(","),
                users: r.users.join(","),
                flags: r.flags.display_csv(),
                state: r.state_label(now).into(),
                owner: r.owner.clone(),
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

        spur_core::auth::check_job_owner(&req.user, &job.spec.user, "exec into")
            .map_err(|e| Status::permission_denied(e.to_string()))?;

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
        let agent_addr = node_comm_http_url(&node, &node_name)?;

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
                user: req.user,
            })
            .await
            .map_err(|e| Status::internal(format!("exec failed: {}", e)))?;

        Ok(resp)
    }

    /// Route a step from srun to the job's allocated nodes. Unlike ExecInJob,
    /// the job may not have a tracked process — salloc allocations only exist
    /// as scheduler bookkeeping.
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

        let step = self
            .cluster
            .get_steps(job_id)
            .into_iter()
            .find(|s| s.step_id == req.step_id)
            .ok_or_else(|| {
                Status::not_found(format!("step {} not found for job {}", req.step_id, job_id))
            })?;

        let num_nodes = job.allocated_nodes.len() as u32;
        let plan = build_step_task_plan(step.num_tasks, num_nodes, step.distribution);
        if plan.is_empty() {
            return Err(Status::failed_precondition(format!(
                "step {} for job {} has no tasks to run",
                req.step_id, job_id
            )));
        }

        let step_num_tasks = step.num_tasks;
        let command = req.command.clone();
        let work_dir = req.work_dir.clone();
        let environment = req.environment.clone();
        let uid = req.uid;
        let gid = req.gid;
        let step_id = req.step_id;
        let label = req.label;
        let job_mpi = job.spec.mpi.as_deref().unwrap_or(spur_core::mpi::MPI_NONE);
        let mpi = spur_core::mpi::resolve_step_mpi(req.mpi.as_str(), job_mpi).to_string();
        let pmix_tmpdir = self.cluster.config().mpi.pmix_tmpdir.clone();
        let modex_connect_timeout_secs = self.cluster.config().mpi.modex_connect_timeout_secs;
        let modex_fence_timeout_secs = self.cluster.config().mpi.modex_fence_timeout_secs;
        let modex_verify_timeout_secs = self.cluster.config().mpi.modex_verify_timeout_secs;

        struct NodeDispatch {
            node_name: String,
            agent_addr: String,
            node_tasks: spur_core::task_launch::NodeStepTasks,
        }

        let mut dispatches = Vec::new();
        let mut dispatch_errors = Vec::new();

        for node_tasks in plan {
            let node_name = match job.allocated_nodes.get(node_tasks.node_index as usize) {
                Some(name) => name.clone(),
                None => {
                    dispatch_errors.push(format!(
                        "step plan references node index {} but job {} has {} nodes",
                        node_tasks.node_index,
                        job_id,
                        job.allocated_nodes.len()
                    ));
                    continue;
                }
            };
            let Some(node) = self.cluster.get_node(&node_name) else {
                dispatch_errors.push(format!("node {node_name} not found"));
                continue;
            };
            let agent_addr = match node_comm_http_url(&node, &node_name) {
                Ok(url) => url,
                Err(e) => {
                    dispatch_errors.push(format!("node {node_name}: {e}"));
                    continue;
                }
            };
            dispatches.push(NodeDispatch {
                node_name,
                agent_addr,
                node_tasks,
            });
        }

        if !dispatch_errors.is_empty() {
            return Err(Status::failed_precondition(format!(
                "srun step dispatch setup failed: {}",
                dispatch_errors.join("; ")
            )));
        }

        let pmix_peers = if mpi == MPI_PMIX {
            Some(
                spur_core::mpi::PmixStepPeers::from_participants(
                    dispatches
                        .iter()
                        .map(|dispatch| dispatch.node_tasks.node_index)
                        .collect(),
                    |idx| {
                        let name = job.allocated_nodes.get(idx as usize)?;
                        self.cluster
                            .get_node(name)
                            .and_then(|n| n.comm_addr().map(str::to_string))
                    },
                )
                .map_err(Status::failed_precondition)?,
            )
        } else {
            None
        };
        let needs_pmix_prepare = pmix_peers.as_ref().is_some_and(|peers| {
            step_needs_pmix_prepare(
                peers.num_nodes,
                job.spec.mpi.as_deref(),
                job.allocated_nodes.len() as u32,
                job.spec.script.as_deref(),
            )
        });
        let run_attempt = job.run_attempt;

        let dispatch_pmix_plans: Vec<Option<spur_proto::proto::PmixLaunchPlan>> =
            if let Some(peers) = pmix_peers.as_ref() {
                dispatches
                    .iter()
                    .map(|node_dispatch| {
                        let local = spur_core::mpi::pmix_local_dispatch_for_step(
                            peers,
                            node_dispatch.node_tasks.node_index,
                            job_id,
                            step_num_tasks,
                            node_dispatch.node_tasks.task_offset,
                            node_dispatch.node_tasks.tasks_on_node,
                            pmix_tmpdir.clone(),
                            uid,
                            gid,
                            modex_connect_timeout_secs,
                            modex_fence_timeout_secs,
                            modex_verify_timeout_secs,
                        )
                        .map_err(Status::failed_precondition)?;
                        spur_core::mpi::build_validated_pmix_plan_proto(mpi.as_str(), local)
                            .map_err(Status::failed_precondition)?
                            .ok_or_else(|| {
                                Status::failed_precondition("job is not configured for PMIx")
                            })
                            .map(Some)
                    })
                    .collect::<Result<Vec<_>, Status>>()?
            } else {
                vec![None; dispatches.len()]
            };

        let mut pmix_prepare_guard = None;

        if needs_pmix_prepare {
            if let Some(detail) = pmix_dispatch::multi_node_pmix_unsupported(
                dispatches.iter().filter_map(|dispatch| {
                    self.cluster
                        .get_node(&dispatch.node_name)
                        .map(|node| node.source.clone())
                }),
            ) {
                return Err(Status::failed_precondition(detail));
            }

            let prepare_nodes: Vec<PmixPrepareNode> = dispatches
                .iter()
                .zip(&dispatch_pmix_plans)
                .map(|(node_dispatch, pmix_plan)| {
                    Ok(PmixPrepareNode {
                        node_name: node_dispatch.node_name.clone(),
                        agent_addr: node_dispatch.agent_addr.clone(),
                        pmix_plan: pmix_plan.clone().ok_or_else(|| {
                            Status::failed_precondition("job is not configured for PMIx")
                        })?,
                    })
                })
                .collect::<Result<Vec<_>, Status>>()?;
            if prepare_nodes.len() != dispatches.len() {
                return Err(Status::failed_precondition(
                    "multi-node PMIx step missing launch plan for one or more nodes",
                ));
            }
            if let Err(detail) =
                pmix_dispatch::prepare_pmix_on_nodes(job_id, run_attempt, prepare_nodes).await
            {
                return Err(Status::failed_precondition(format!(
                    "PMIx prepare failed: {detail}"
                )));
            }
            let addrs: Vec<String> = dispatches.iter().map(|d| d.agent_addr.clone()).collect();
            pmix_prepare_guard = Some(pmix_dispatch::PmixPreparedReleaseGuard::new(job_id, addrs));
        }

        let mut set = tokio::task::JoinSet::new();
        for (dispatch, pmix_plan) in dispatches.iter().zip(dispatch_pmix_plans) {
            let node_name = dispatch.node_name.clone();
            let agent_addr = dispatch.agent_addr.clone();
            let node_tasks = dispatch.node_tasks.clone();
            let command = command.clone();
            let work_dir = work_dir.clone();
            let environment = environment.clone();
            let step_mpi = mpi.clone();
            set.spawn(async move {
                let mut agent = SlurmAgentClient::connect(agent_addr.clone())
                    .await
                    .map_err(|e| {
                        Status::unavailable(format!("cannot reach agent at {}: {}", agent_addr, e))
                    })?
                    .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
                    .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

                let agent_resp = agent
                    .run_command(RunCommandRequest {
                        command: command.clone(),
                        uid,
                        gid,
                        work_dir: work_dir.clone(),
                        environment: environment.clone(),
                        job_id,
                        num_tasks: node_tasks.tasks_on_node,
                        task_offset: node_tasks.task_offset,
                        step_id,
                        step_num_tasks,
                        label,
                        pmix_plan,
                        mpi: step_mpi.clone(),
                        pmix_prepared: needs_pmix_prepare,
                    })
                    .await
                    .map_err(|e| {
                        Status::internal(format!("run_command on {} failed: {}", node_name, e))
                    })?
                    .into_inner();

                Ok::<_, Status>((node_name, agent_resp))
            });
        }

        let mut max_exit = 0i32;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut ran_nodes = Vec::new();
        let step_node_names: Vec<String> = dispatches.iter().map(|d| d.node_name.clone()).collect();
        let mut step_abort_sent = false;

        while let Some(result) = set.join_next().await {
            match result {
                Ok(Ok((node_name, agent_resp))) => {
                    max_exit = max_exit.max(agent_resp.exit_code);
                    stdout.push_str(&agent_resp.stdout);
                    stderr.push_str(&agent_resp.stderr);
                    ran_nodes.push(node_name);
                }
                Ok(Err(e)) => {
                    warn!(job_id, step_id, error = %e, "step dispatch failed on one node");
                    dispatch_errors.push(e.to_string());
                    max_exit = max_exit.max(1);
                    if !step_abort_sent {
                        step_abort_sent = true;
                        crate::scheduler_loop::cancel_step_on_nodes(
                            &self.cluster,
                            job_id,
                            step_id,
                            &step_node_names,
                            15,
                        )
                        .await;
                    }
                }
                Err(e) => {
                    warn!(job_id, step_id, error = %e, "step dispatch task panicked");
                    dispatch_errors.push(format!("step dispatch task panicked: {e}"));
                    max_exit = max_exit.max(1);
                    if !step_abort_sent {
                        step_abort_sent = true;
                        crate::scheduler_loop::cancel_step_on_nodes(
                            &self.cluster,
                            job_id,
                            step_id,
                            &step_node_names,
                            15,
                        )
                        .await;
                    }
                }
            }
        }

        if !dispatch_errors.is_empty() {
            max_exit = max_exit.max(1);
            stderr.push_str(&format!(
                "srun step dispatch errors:\n{}\n",
                dispatch_errors.join("\n")
            ));
        }

        if needs_pmix_prepare {
            if let Some(guard) = pmix_prepare_guard.as_mut() {
                guard.disarm();
            }
            let addrs: Vec<String> = dispatches.iter().map(|d| d.agent_addr.clone()).collect();
            pmix_dispatch::release_pmix_on_agents(&addrs, job_id).await;
        }

        if let Err(e) = self.cluster.record_step_complete(job_id, step_id, max_exit) {
            warn!(
                job_id,
                step_id,
                error = %e,
                "failed to record step completion"
            );
        }

        Ok(Response::new(RunStepResponse {
            exit_code: max_exit,
            stdout,
            stderr,
            node: ran_nodes.join(","),
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
        if !is_k0s_admin(self.cluster.association_cache(), &req.caller) {
            return Err(Status::permission_denied(
                "k0s cluster up requires cluster admin",
            ));
        }
        let state = self.cluster.k0s_state();
        let nodes = self.cluster.get_nodes();
        let assigned = nodes.iter().any(|n| n.k0s_role.is_some());

        // Resolve the HA control-plane set fail-closed BEFORE recording intent: an explicit node
        // list wins, else `--replicas` (or the config default) picks the lowest-named nodes.
        let candidates: Vec<String> = nodes.into_iter().map(|n| n.name).collect();
        let explicit_override =
            !req.control_plane_nodes.is_empty() || req.control_plane_replicas.is_some();
        let replicas = req
            .control_plane_replicas
            .filter(|r| *r > 0)
            .unwrap_or(self.control_plane_replicas);
        let pinned = req
            .control_plane_node
            .clone()
            .or_else(|| state.control_plane_node.clone());
        // A bare re-up of an assigned cluster targets the recorded set, so it stays idempotent
        // regardless of the config default; an explicit list/replica count is resolved and enforced.
        let cp_set = if assigned && !explicit_override {
            state.controllers()
        } else {
            crate::cluster_k8s::resolve_control_plane_set(
                candidates,
                &req.control_plane_nodes,
                pinned.as_deref(),
                replicas,
            )
            .map_err(Status::invalid_argument)?
        };

        // A control-plane change after roles are assigned would leave an inconsistent topology
        // (provisioning skips assigned nodes); require `spur k8s down --reset` to re-elect.
        if assigned {
            let mut current = state.controllers();
            let mut want = cp_set.clone();
            current.sort();
            want.sort();
            if current != want {
                return Err(Status::failed_precondition(format!(
                    "control plane is already assigned ({}); tear the cluster down \
                     (spur k8s down --reset) before changing it to [{}]",
                    state.controllers().join(", "),
                    cp_set.join(", "),
                )));
            }
        }

        let bootstrap = cp_set.first().cloned();
        self.cluster
            .set_k0s_phase(
                spur_core::k0s::K0sPhase::Provisioning,
                bootstrap,
                cp_set,
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
        if !is_k0s_admin(self.cluster.association_cache(), &req.caller) {
            return Err(Status::permission_denied(
                "k0s cluster down requires cluster admin",
            ));
        }
        self.cluster
            .set_k0s_phase(spur_core::k0s::K0sPhase::Down, None, Vec::new(), req.reset)
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
        let control_plane_nodes = state.controllers();
        Ok(Response::new(ClusterStatusResponse {
            phase: crate::cluster_k8s::phase_str(state.phase),
            control_plane_node: state.control_plane_node.unwrap_or_default(),
            control_plane_nodes,
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
        let is_admin = is_k0s_admin(self.cluster.association_cache(), &req.caller);

        if req.admin {
            if !is_admin {
                return Err(Status::permission_denied(
                    "the cluster-admin kubeconfig requires cluster admin",
                ));
            }
            // Cluster-admin kubeconfig (`k0s kubeconfig admin` on the control-plane agent).
            return match crate::cluster_k8s::fetch_admin_kubeconfig(&self.cluster).await {
                Ok(kubeconfig) => Ok(Response::new(ClusterKubeconfigResponse { kubeconfig })),
                Err(e) => Err(Status::unavailable(format!(
                    "could not fetch admin kubeconfig from the control-plane agent: {e}"
                ))),
            };
        }

        // A non-admin caller may only mint their own scope; an admin may target any user. An empty
        // target means "the caller's own namespace" (an empty caller is internal/root).
        let target = if req.user.is_empty() {
            req.caller.clone()
        } else {
            req.user.clone()
        };
        if !is_admin && target != req.caller {
            return Err(Status::permission_denied(format!(
                "user '{}' may only request their own kubeconfig",
                req.caller
            )));
        }
        if target.is_empty() {
            return Err(Status::invalid_argument(
                "no target user for the scoped kubeconfig; pass --user or set a caller identity",
            ));
        }

        // Scoped kubeconfig: resolve the target's account -> its namespace + per-user ServiceAccount,
        // then have the control-plane agent mint a bound token there.
        let (namespace, sa) = resolve_user_namespace_sa(self.cluster.association_cache(), &target)?;
        match crate::cluster_k8s::fetch_user_kubeconfig(&self.cluster, &target, &namespace, &sa)
            .await
        {
            Ok(kubeconfig) => Ok(Response::new(ClusterKubeconfigResponse { kubeconfig })),
            Err(e) => Err(Status::unavailable(format!(
                "could not mint a scoped kubeconfig for user '{target}': {e}"
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
    accounting_service: Option<crate::accounting::AccountingService>,
    control_plane_replicas: u32,
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

    let jwt_key = resolve_startup_jwt_key(&cluster.config());

    let service = ControllerService {
        cluster,
        client_addrs,
        raft: raft_handle.clone(),
        leader_proxy,
        rpc_stats: rpc_stats.clone(),
        sched_stats: sched_stats.clone(),
        control_plane_replicas,
        jwt_key,
    };

    let stats_layer = RpcStatsLayer::new(rpc_stats, raft_handle);

    let mut builder = tonic::transport::Server::builder().layer(stats_layer);

    let mut router = builder.add_service(spur_proto::controller_server(service));
    if let Some(service) = accounting_service {
        router = router.add_service(crate::accounting::accounting_server(service));
    }

    router.serve(addr).await?;

    Ok(())
}

/// Resolve the target node for a job step. Empty = first allocated (legacy
/// default); a named node must be one the job holds, else it targets outside it.
fn select_step_node<'a>(allocated: &'a [String], requested: &str) -> Result<&'a str, String> {
    if requested.is_empty() {
        return allocated
            .first()
            .map(String::as_str)
            .ok_or_else(|| "job has no allocated nodes".to_string());
    }
    allocated
        .iter()
        .find(|n| n.as_str() == requested)
        .map(String::as_str)
        .ok_or_else(|| format!("node {requested} is not allocated to this job"))
}

// ---- Proto conversion helpers ----

// tonic::Status is 176 bytes (over clippy's 128-byte threshold); fixed upstream in tonic 0.13+
#[allow(clippy::result_large_err)]
fn proto_to_job_spec(spec: JobSpec) -> Result<spur_core::job::JobSpec, Status> {
    let mut gres = spec.gres;
    for lic in &spec.licenses {
        gres.push(format!("license:{}", lic));
    }

    let gpus = spur_core::gpu_request::GpuRequest::from_proto(spec.gpus);
    let gpus_per_node = spur_core::gpu_request::GpuRequest::from_proto(spec.gpus_per_node);
    let gpus_per_task = spur_core::gpu_request::GpuRequest::from_proto(spec.gpus_per_task);

    let job_spec = spur_core::job::JobSpec {
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
        // 0 means the caller did not set ntasks: default to one task per node
        // (Slurm's default) so it scales with num_nodes rather than collapsing.
        num_tasks: if spec.num_tasks > 0 {
            spec.num_tasks
        } else {
            spec.num_nodes.max(1)
        },
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
        gpus,
        gpus_per_node,
        gpus_per_task,
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
        srun_job: spec.srun_job,
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
        pty: spec.pty,
    };

    // GPU demand is validated in `Cluster::submit_job` after node-count
    // normalization, so `-N4 -n1 --gpus=2` (valid once reduced to one node)
    // is not rejected against the pre-normalization node count.
    Ok(job_spec)
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
        state_reason: job.state_reason(),
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
        stdout_path: job
            .actual_stdout_path
            .clone()
            .unwrap_or_else(|| job.resolved_stdout()),
        stderr_path: job
            .actual_stderr_path
            .clone()
            .unwrap_or_else(|| job.resolved_stderr()),
        stdin_path: job.resolved_stdin().unwrap_or_default(),
        resources: job.allocated_resources.as_ref().map(allocations_to_proto),
        priority: job.priority,
        qos: job.spec.qos.clone().unwrap_or_default(),
        array_job_id: job.spec.array_job_id.unwrap_or(0),
        array_task_id: job.spec.array_task_id.unwrap_or(0),
        reservation: job.spec.reservation.clone().unwrap_or_default(),
        comment: job.spec.comment.clone().unwrap_or_default(),
        srun_step_dispatch: job.srun_step_dispatch,
        req_gpus: spur_core::job::effective_gpus(&job.spec, job.spec.num_nodes) as u32,
        req_gpus_detail: requested_gpus_detail(&job.spec),
    }
}

/// Human-readable summary of a job's GPU request for display.
fn requested_gpus_detail(spec: &spur_core::job::JobSpec) -> String {
    let ty = |t: Option<&str>| t.map(|t| format!("{t}:")).unwrap_or_default();

    // Per-task form: render from the raw spec field (before resolution into PerNode).
    if let Some(ref r) = spec.gpus_per_task {
        return format!("gpu:{}{}/task", ty(r.gpu_type.as_deref()), r.count);
    }

    use spur_core::gpu_request::GpuDemand;
    let Ok(demand) = spur_core::gpu_request::resolve_gpu_demand(spec) else {
        return String::new();
    };
    match demand {
        GpuDemand::None => String::new(),
        GpuDemand::Total { count, gpu_type } => format!("gpu:{}{}", ty(gpu_type.as_deref()), count),
        GpuDemand::PerNode { counts, gpu_type } => {
            let first = counts.first().copied().unwrap_or(0);
            if counts.iter().all(|&c| c == first) {
                format!("gpu:{}{}/node", ty(gpu_type.as_deref()), first)
            } else {
                let list = counts
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("gpu:{}[{}]/node", ty(gpu_type.as_deref()), list)
            }
        }
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
        features: node.features.clone(),
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
        deny_qos: part.deny_qos.join(","),
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
        ReservationError::PermissionDenied(m) => Status::permission_denied(m),
        ReservationError::Raft(m) => Status::internal(m),
    }
}

fn submit_rpc_status(err: crate::cluster::SubmitError) -> Status {
    match err {
        crate::cluster::SubmitError::InvalidArgument(m) => Status::invalid_argument(m),
        crate::cluster::SubmitError::Internal(m) => Status::internal(m),
    }
}

fn partition_rpc_status(err: PartitionError) -> Status {
    match err {
        PartitionError::InvalidArgument(m) => Status::invalid_argument(m),
        PartitionError::NotFound(m) => Status::not_found(m),
        PartitionError::AlreadyExists(m) => Status::already_exists(m),
        PartitionError::Raft(m) => Status::internal(m),
    }
}

/// Resolve an `UpdatePartitionRequest`'s max-nodes intent into the
/// `(max_nodes, clear)` pair `ClusterManager::update_partition` expects.
///
/// `clear_max_nodes` and a literal `max_nodes_value == 0` both mean "no limit"
/// (0 is documented as "clear limit" in the proto); neither can express a real
/// 0-node cap. The two inputs must be collapsed into a single `clear` bool that
/// is passed through — forwarding the raw request flag would drop a `--max-nodes
/// 0` clear, since that arrives as `Some(0)` with the flag unset.
fn resolve_max_nodes_update(max_nodes_value: Option<u32>, clear_flag: bool) -> (Option<u32>, bool) {
    let clear = clear_flag || max_nodes_value == Some(0);
    let max_nodes = if clear { None } else { max_nodes_value };
    (max_nodes, clear)
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
    fn resolve_registration_comm_addr_normalizes_explicit_ip() {
        assert_eq!(
            resolve_registration_comm_addr("10.0.0.2", "", false).unwrap(),
            "10.0.0.2"
        );
    }

    #[test]
    fn resolve_registration_comm_addr_falls_back_to_peer_ip() {
        assert_eq!(
            resolve_registration_comm_addr("", "10.0.0.9", false).unwrap(),
            "10.0.0.9"
        );
    }

    #[test]
    fn resolve_registration_comm_addr_prefers_peer_over_loopback_advertised() {
        assert_eq!(
            resolve_registration_comm_addr("127.0.0.1", "10.245.159.30", false).unwrap(),
            "10.245.159.30"
        );
    }

    #[test]
    fn resolve_registration_comm_addr_rejects_loopback_when_configured() {
        assert!(resolve_registration_comm_addr("127.0.0.1", "", true).is_err());
    }

    #[test]
    fn resolve_registration_comm_addr_prefers_peer_when_rejecting_loopback() {
        assert_eq!(
            resolve_registration_comm_addr("127.0.0.1", "10.245.159.30", true).unwrap(),
            "10.245.159.30"
        );
    }

    #[test]
    fn node_comm_http_url_brackets_ipv6() {
        let mut node =
            spur_core::node::Node::new("n1".into(), spur_core::resource::ResourceSet::default());
        node.address = Some("2001:db8::1".into());
        node.port = 6818;
        assert_eq!(
            node_comm_http_url(&node, "n1").unwrap(),
            "http://[2001:db8::1]:6818"
        );
    }

    #[test]
    fn resolve_user_namespace_sa_fails_closed_on_cold_cache() {
        // A not-yet-loaded cache resolves fail-open, so the scoped-kubeconfig path
        // must reject rather than mint an unscoped (cluster-wide) token.
        let cache = crate::association_cache::AssociationCache::new();
        let err = resolve_user_namespace_sa(&cache, "alice").unwrap_err();
        assert_eq!(err.code(), Code::Unavailable);
    }

    #[test]
    fn resolve_user_namespace_sa_rejects_unassociated_user() {
        let cache = crate::association_cache::AssociationCache::new();
        cache.set_loaded_without_associations();
        let err = resolve_user_namespace_sa(&cache, "alice").unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[test]
    fn resolve_user_namespace_sa_derives_namespace_and_sa() {
        let cache = crate::association_cache::AssociationCache::new();
        cache.insert_default_account("alice", "physics");
        let (namespace, sa) = resolve_user_namespace_sa(&cache, "alice").unwrap();
        assert_eq!(namespace, "spur-acct-physics");
        assert_eq!(sa, "spur-user-alice");
    }

    #[test]
    fn is_k0s_admin_root_and_empty_always_admin_even_cold_cache() {
        let cache = crate::association_cache::AssociationCache::new();
        assert!(is_k0s_admin(&cache, ""));
        assert!(is_k0s_admin(&cache, "root"));
    }

    #[test]
    fn is_k0s_admin_named_user_denied_when_accounting_off() {
        // Cold cache = accounting disabled/not loaded: only root/internal are admin.
        let cache = crate::association_cache::AssociationCache::new();
        assert!(!is_k0s_admin(&cache, "alice"));
    }

    #[test]
    fn is_k0s_admin_honors_accounting_admin_level() {
        let cache = crate::association_cache::AssociationCache::new();
        cache.insert_admin_level("carol", "Admin");
        cache.insert_admin_level("dave", "Operator");
        assert!(is_k0s_admin(&cache, "carol"));
        // Operator is not full admin; a plain member isn't either.
        assert!(!is_k0s_admin(&cache, "dave"));
        assert!(!is_k0s_admin(&cache, "erin"));
    }

    #[test]
    fn read_forwards_only_when_follower_and_not_already_forwarded() {
        // Leader serves reads locally; never forwards.
        assert!(!read_forwarding_policy(true, false));
        assert!(!read_forwarding_policy(true, true));
        // Follower forwards a fresh read to the leader.
        assert!(read_forwarding_policy(false, false));
        // An already-forwarded read is served locally to avoid forward loops.
        assert!(!read_forwarding_policy(false, true));
    }

    fn test_slurm_config() -> spur_core::config::SlurmConfig {
        serde_json::from_str(r#"{"cluster_name":"test"}"#).unwrap()
    }

    /// A `ControllerService` on a node that can never elect a leader: three
    /// unreachable peers mean no quorum, so `current_leader` stays `None`.
    async fn no_leader_service(
        cluster: Arc<crate::cluster::ClusterManager>,
        dir: &std::path::Path,
    ) -> ControllerService {
        use crate::rpc_stats::RpcStatsCollector;
        use crate::sched_stats::SchedStatsCollector;

        let handle = crate::raft::start_raft(
            1,
            &[
                "[::1]:0".to_string(),
                "[::1]:0".to_string(),
                "[::1]:0".to_string(),
            ],
            dir,
            cluster.clone(),
        )
        .await
        .unwrap();
        assert!(!handle.is_leader());
        assert_eq!(handle.current_leader(), None);

        let raft = Arc::new(handle);
        let client_addrs: BTreeMap<u64, String> = BTreeMap::new();
        ControllerService {
            cluster,
            raft: raft.clone(),
            leader_proxy: LeaderProxy::new(raft.clone(), client_addrs.clone()),
            client_addrs,
            rpc_stats: Arc::new(RpcStatsCollector::new()),
            sched_stats: Arc::new(SchedStatsCollector::new("sched/backfill")),
            control_plane_replicas: 1,
            jwt_key: String::new(),
        }
    }

    /// A non-leader that can't reach a leader must still answer reads
    /// from local applied state, not fail with "no leader elected yet".
    #[tokio::test]
    async fn get_jobs_serves_locally_when_no_leader_elected() {
        use crate::raft::StateMachineApply;
        use spur_core::job::JobSpec;
        use spur_core::wal::WalOperation;

        let dir = tempfile::TempDir::new().unwrap();
        let cluster =
            Arc::new(crate::cluster::ClusterManager::new(test_slurm_config(), dir.path()).unwrap());

        // Mirrors a follower applying a committed entry, without a leader.
        let spec = JobSpec {
            name: "job-a".into(),
            user: "alice".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
            cluster.as_ref(),
            &WalOperation::JobSubmit {
                job_id: 1,
                spec: Box::new(spec),
            },
        );

        let service = no_leader_service(cluster, dir.path()).await;

        let resp = service
            .get_jobs(Request::new(GetJobsRequest::default()))
            .await;
        let jobs = resp
            .expect("get_jobs must serve local state, not fail with 'no leader elected yet'")
            .into_inner()
            .jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, 1);
    }

    /// The write side of the contract: with no leader, writes must fail rather
    /// than fall back to local state — guards against a refactor extending the
    /// read fallback to writes.
    #[tokio::test]
    async fn submit_job_fails_when_no_leader_elected() {
        let dir = tempfile::TempDir::new().unwrap();
        let cluster =
            Arc::new(crate::cluster::ClusterManager::new(test_slurm_config(), dir.path()).unwrap());
        let service = no_leader_service(cluster, dir.path()).await;

        let status = service
            .submit_job(Request::new(SubmitJobRequest::default()))
            .await
            .expect_err("submit_job must not serve locally when there is no leader");
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "not the Raft leader");
    }

    #[test]
    fn submit_rpc_status_maps_invalid_argument() {
        use crate::cluster::SubmitError;

        let status = submit_rpc_status(SubmitError::invalid("partition 'gpu' not found"));
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "partition 'gpu' not found");
    }

    /// Terminal and unknown ids are killed; Pending and a bound Running job are spared.
    #[tokio::test]
    async fn should_kill_reported_job_selects_only_terminal_or_unbound() {
        use crate::raft::StateMachineApply;
        use spur_core::job::{JobSpec, JobState};
        use spur_core::wal::{PendingKillReservation, WalOperation};

        let dir = tempfile::TempDir::new().unwrap();
        let cluster =
            Arc::new(crate::cluster::ClusterManager::new(test_slurm_config(), dir.path()).unwrap());

        let submit = |job_id: u32, name: &str| WalOperation::JobSubmit {
            job_id,
            spec: Box::new(JobSpec {
                name: name.into(),
                user: "alice".into(),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            }),
        };
        let apply = |op: &WalOperation| {
            <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
                cluster.as_ref(),
                op,
            );
        };

        // 10 Pending, 11 Running, 12 terminal, and the non-Running active states
        // that must also be spared: 13 Completing, 14 Suspended, 15 Preempted.
        apply(&submit(10, "pending"));
        for id in [11, 12, 13, 14, 15] {
            apply(&submit(id, "job"));
        }

        let res = spur_core::resource::ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices: std::collections::HashMap::new(),
        };
        let mut per_node = std::collections::HashMap::new();
        per_node.insert("n1".to_string(), res.clone());
        for id in [11, 12, 13, 14, 15] {
            apply(&WalOperation::job_start(
                id,
                vec!["n1".into()],
                res.clone(),
                per_node.clone(),
            ));
            apply(&WalOperation::job_state_change(
                id,
                JobState::Pending,
                JobState::Running,
            ));
        }
        apply(&WalOperation::job_state_change(
            12,
            JobState::Running,
            JobState::Cancelled,
        ));
        apply(&WalOperation::job_state_change(
            13,
            JobState::Running,
            JobState::Completing,
        ));
        apply(&WalOperation::job_state_change(
            14,
            JobState::Running,
            JobState::Suspended,
        ));
        apply(&WalOperation::job_state_change(
            15,
            JobState::Running,
            JobState::Preempted,
        ));

        assert_eq!(cluster.job_state(10), Some(JobState::Pending));
        assert_eq!(cluster.job_state(11), Some(JobState::Running));
        assert!(cluster.job_state(12).unwrap().is_terminal());
        assert_eq!(cluster.job_state(13), Some(JobState::Completing));
        assert_eq!(cluster.job_state(14), Some(JobState::Suspended));
        assert_eq!(cluster.job_state(15), Some(JobState::Preempted));

        apply(&WalOperation::PendingKillReserve {
            reservations: vec![PendingKillReservation {
                job_id: 10,
                node: "n1".into(),
                resources: res.clone(),
                attempt: 101,
                run_attempt: 1,
            }],
        });

        let stale: Vec<u32> = [10, 11, 12, 13, 14, 15, 999]
            .into_iter()
            .filter(|&job_id| should_kill_reported_job(&cluster, job_id, "n1"))
            .collect();
        assert_eq!(
            stale,
            vec![10, 12, 999],
            "a Pending job is reclaimed only while an unconfirmed prior-run hold exists"
        );

        // Same jobs, queried against a node NONE of them are bound to: Running/
        // Completing/Suspended must now be killed (unbound); the release hold
        // is scoped to n1, so Pending remains spared on n2.
        let unbound: Vec<u32> = [10, 11, 13, 14, 15]
            .into_iter()
            .filter(|&job_id| should_kill_reported_job(&cluster, job_id, "n2"))
            .collect();
        assert_eq!(
            unbound,
            vec![11, 13, 14],
            "Running/Completing/Suspended are killed when reported by a node they aren't bound to"
        );
    }

    /// GATE: a job requeued (Timeout -> Pending) between the reclaim snapshot
    /// and the spawned loop's send must fail the re-check, not just the snapshot.
    #[tokio::test]
    async fn should_kill_reported_job_false_after_requeue_race() {
        use crate::raft::StateMachineApply;
        use spur_core::job::{JobSpec, JobState};
        use spur_core::wal::WalOperation;

        let dir = tempfile::TempDir::new().unwrap();
        let cluster =
            Arc::new(crate::cluster::ClusterManager::new(test_slurm_config(), dir.path()).unwrap());
        let apply = |op: &WalOperation| {
            <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
                cluster.as_ref(),
                op,
            );
        };

        apply(&WalOperation::JobSubmit {
            job_id: 77,
            spec: Box::new(JobSpec {
                name: "interactive".into(),
                user: "alice".into(),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                requeue: true,
                ..Default::default()
            }),
        });
        let res = spur_core::resource::ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices: std::collections::HashMap::new(),
        };
        let mut per_node = std::collections::HashMap::new();
        per_node.insert("n1".to_string(), res.clone());
        apply(&WalOperation::job_start(
            77,
            vec!["n1".into()],
            res,
            per_node,
        ));
        apply(&WalOperation::job_state_change(
            77,
            JobState::Pending,
            JobState::Running,
        ));
        apply(&WalOperation::job_state_change(
            77,
            JobState::Running,
            JobState::Timeout,
        ));
        assert!(
            should_kill_reported_job(&cluster, 77, "n1"),
            "snapshot sees Timeout"
        );

        // Concurrent requeue lands before the reclaim loop's re-check.
        apply(&WalOperation::job_state_change(
            77,
            JobState::Timeout,
            JobState::Pending,
        ));

        assert!(
            !should_kill_reported_job(&cluster, 77, "n1"),
            "re-check must skip a job requeued since the snapshot"
        );
    }

    /// GATE: `auth.jwt_key` is captured at startup and must NOT change on
    /// `reconfigure`. Swapping it live would instantly invalidate every
    /// outstanding node token. This drives the real capture path
    /// (`resolve_startup_jwt_key`) and the real `reconfigure`, then proves the
    /// running controller still verifies a token minted with the startup key.
    #[tokio::test]
    async fn reconfigure_does_not_adopt_new_jwt_key() {
        use spur_core::admission::{generate_node_token, verify_node_token};

        let dir = tempfile::TempDir::new().unwrap();
        let conf_path = dir.path().join("spur.conf");
        std::fs::write(
            &conf_path,
            "cluster_name = \"test\"\n[auth]\nplugin = \"jwt\"\njwt_key = \"old-secret\"\n",
        )
        .unwrap();

        let config = spur_core::config::SlurmConfig::load_from_file(&conf_path).unwrap();
        let cluster = Arc::new(
            ClusterManager::new_with_config_path(config, dir.path(), Some(conf_path.clone()))
                .unwrap(),
        );
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cluster.clone())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .unwrap();
        cluster.set_raft(handle.raft);

        // The controller captures the signing key exactly here, at startup.
        let startup_key = resolve_startup_jwt_key(&cluster.config());
        assert_eq!(startup_key, "old-secret");
        let token = generate_node_token("node-1", startup_key.as_bytes()).unwrap();

        // Operator edits jwt_key and reconfigures.
        std::fs::write(
            &conf_path,
            "cluster_name = \"test\"\n[auth]\nplugin = \"jwt\"\njwt_key = \"new-secret\"\n",
        )
        .unwrap();
        cluster.reconfigure().unwrap();

        // The live config reflects the new key (proving reconfigure did swap
        // config)...
        assert_eq!(
            cluster.config().auth.jwt_key.as_deref(),
            Some("new-secret"),
            "reconfigure must swap the live config"
        );
        // ...but the controller's captured key is unchanged, so tokens minted
        // with the startup key still verify. A live-reloaded key would reject
        // this token.
        assert_eq!(startup_key, "old-secret", "captured key must not change");
        assert!(
            verify_node_token(&token, startup_key.as_bytes()).is_ok(),
            "outstanding node token must still verify against the startup key"
        );
        assert!(
            verify_node_token(&token, b"new-secret").is_err(),
            "sanity: the token would fail under the new key (proving the key matters)"
        );
    }

    #[test]
    fn submit_rpc_status_maps_internal() {
        use crate::cluster::SubmitError;

        let status = submit_rpc_status(SubmitError::internal("raft propose failed"));
        assert_eq!(status.code(), Code::Internal);
    }

    fn step_test_config() -> spur_core::config::SlurmConfig {
        spur_core::config::SlurmConfig::load_from_str(
            "cluster_name = \"test\"\n\
             [controller]\nfirst_job_id = 1\n\
             [[partitions]]\nname = \"default\"\ndefault = true\nnodes = \"ALL\"\n",
        )
        .unwrap()
    }

    async fn test_service(dir: &tempfile::TempDir) -> ControllerService {
        use crate::cluster::ClusterManager;
        let cluster =
            std::sync::Arc::new(ClusterManager::new(step_test_config(), dir.path()).unwrap());
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cluster.clone())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .unwrap();
        cluster.set_raft(handle.raft.clone());
        let raft = std::sync::Arc::new(handle);
        ControllerService {
            cluster,
            raft: raft.clone(),
            leader_proxy: LeaderProxy::new(raft, BTreeMap::new()),
            client_addrs: BTreeMap::new(),
            rpc_stats: std::sync::Arc::new(RpcStatsCollector::new()),
            sched_stats: std::sync::Arc::new(SchedStatsCollector::new("backfill")),
            control_plane_replicas: 1,
            jwt_key: String::new(),
        }
    }

    // evict_job is job-scoped, so one node's phantom must spare a multi-node job.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_spares_a_multi_node_job_on_one_nodes_phantom_report() {
        use crate::raft::StateMachineApply;
        use spur_core::job::{JobSpec, JobState};
        use spur_core::resource::ResourceAllocations;
        use spur_core::wal::WalOperation;

        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let apply = |op: &WalOperation| {
            <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
                &svc.cluster,
                op,
            );
        };

        apply(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(JobSpec {
                name: "multi-node".into(),
                user: "alice".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            }),
        });
        apply(&WalOperation::job_state_change(
            1,
            JobState::Pending,
            JobState::Running,
        ));
        let res = ResourceAllocations::with_scalar(1, 1000);
        let per_node: std::collections::HashMap<_, _> =
            [("n1".to_string(), res.clone()), ("n2".to_string(), res)].into();
        apply(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into()],
            resources: ResourceAllocations::with_scalar(2, 2000),
            per_node_alloc: per_node,
            srun_step_dispatch: false,
            run_attempt: 1,
        });

        // n1's heartbeat never reports job 1; cross the phantom threshold.
        for _ in 0..3 {
            svc.reconcile_reported_allocations("n1", &[]);
        }
        // Give the (would-be) spawned eviction a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            svc.cluster.get_job(1).unwrap().state,
            JobState::Running,
            "a multi-node job must not be evicted over one node's phantom report"
        );
    }

    // The single-node counterpart: a persistent phantom report must still
    // evict the job (this is the case D1 exists to fix).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_evicts_a_single_node_job_after_persistent_phantom_report() {
        use crate::raft::StateMachineApply;
        use spur_core::job::{JobSpec, JobState};
        use spur_core::resource::ResourceAllocations;
        use spur_core::wal::WalOperation;

        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let apply = |op: &WalOperation| {
            <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
                &svc.cluster,
                op,
            );
        };

        apply(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(JobSpec {
                name: "single-node".into(),
                user: "alice".into(),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            }),
        });
        apply(&WalOperation::job_state_change(
            1,
            JobState::Pending,
            JobState::Running,
        ));
        let res = ResourceAllocations::with_scalar(1, 1000);
        let per_node: std::collections::HashMap<_, _> = [("n1".to_string(), res)].into();
        apply(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: ResourceAllocations::with_scalar(1, 1000),
            per_node_alloc: per_node,
            srun_step_dispatch: false,
            run_attempt: 1,
        });

        for _ in 0..3 {
            svc.reconcile_reported_allocations("n1", &[]);
        }

        let mut evicted = false;
        for _ in 0..200 {
            if svc.cluster.get_job(1).unwrap().state.is_terminal() {
                evicted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            evicted,
            "a single-node job's persistent phantom report must evict it"
        );
    }

    // A heartbeat that stops reporting a job is the release confirmation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_clears_pending_kill_once_heartbeat_confirms_release() {
        use spur_core::resource::ResourceAllocations;

        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;

        svc.cluster
            .note_pending_kill(1, "n1", ResourceAllocations::with_scalar(2, 4000), 101);
        svc.cluster
            .note_pending_kill(2, "n1", ResourceAllocations::with_scalar(1, 1000), 102);
        assert_eq!(svc.cluster.pending_kill_reservations()["n1"].cpus, 3);

        // n1's heartbeat still reports job 2, but no longer job 1.
        svc.reconcile_reported_allocations(
            "n1",
            &[RunningJobStatus {
                job_id: 2,
                ..Default::default()
            }],
        );

        // The release confirmation is spawned off the heartbeat's critical path.
        let mut cpus = svc.cluster.pending_kill_reservations()["n1"].cpus;
        for _ in 0..200 {
            if cpus == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            cpus = svc.cluster.pending_kill_reservations()["n1"].cpus;
        }
        assert_eq!(
            cpus, 1,
            "job 1's reservation must clear once the heartbeat confirms it's gone"
        );
    }

    // An unauthorized cancel must not plant a reservation against a job it
    // never touched — that would let anyone withhold another job's resources.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unauthorized_cancel_reserves_nothing() {
        use crate::raft::StateMachineApply;
        use spur_core::job::{JobSpec, JobState};
        use spur_core::resource::ResourceAllocations;
        use spur_core::wal::WalOperation;

        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let apply = |op: &WalOperation| {
            <crate::cluster::ClusterManager as StateMachineApply>::apply_operation(
                &svc.cluster,
                op,
            );
        };

        apply(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(JobSpec {
                name: "owned-by-alice".into(),
                user: "alice".into(),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            }),
        });
        apply(&WalOperation::job_state_change(
            1,
            JobState::Pending,
            JobState::Running,
        ));
        let per_node: std::collections::HashMap<_, _> =
            [("n1".to_string(), ResourceAllocations::with_scalar(2, 4000))].into();
        apply(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: ResourceAllocations::with_scalar(2, 4000),
            per_node_alloc: per_node,
            srun_step_dispatch: false,
            run_attempt: 1,
        });

        let result = svc
            .cancel_job(Request::new(CancelJobRequest {
                job_id: 1,
                signal: 0,
                user: "mallory".into(),
            }))
            .await;
        assert!(result.is_err(), "an unauthorized cancel must be rejected");
        assert!(
            svc.cluster.pending_kill_reservations().is_empty(),
            "the rejected cancel must not have reserved n1's resources"
        );
        assert_eq!(svc.cluster.get_job(1).unwrap().state, JobState::Running);
    }

    // A step must NOT be created when the target node is allocated but unregistered: address
    // resolution runs first and returns a retryable Unavailable, so the client's retries can't
    // each leak a step.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_job_step_unregistered_target_creates_no_step() {
        use spur_core::resource::{ResourceAllocations, ResourceSet};
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;

        svc.cluster
            .register_node(
                "n1".into(),
                "n1".into(),
                ResourceSet {
                    cpus: 8,
                    memory_mb: 16000,
                    ..Default::default()
                },
                "127.0.0.1".into(),
                6818,
                String::new(),
                String::new(),
                spur_core::node::NodeSource::NativeHost,
                std::collections::HashMap::new(),
            )
            .unwrap();
        for _ in 0..200 {
            if svc.cluster.get_node("n1").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let mut spec = spur_core::job::JobSpec {
            name: "leak".into(),
            user: "u".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        spec.srun_job = true;
        let job_id = svc.cluster.submit_job(spec).unwrap().job_id;
        // Allocate to a real node plus a ghost node that is never registered.
        let res = ResourceAllocations::with_scalar(2, 4000);
        let per_node: std::collections::HashMap<_, _> = [
            ("n1".to_string(), res.clone()),
            ("ghost".to_string(), res.clone()),
        ]
        .into_iter()
        .collect();
        svc.cluster
            .start_job(job_id, vec!["n1".into(), "ghost".into()], res, per_node)
            .unwrap();
        for _ in 0..200 {
            if svc.cluster.get_job(job_id).map(|j| j.state) == Some(JobState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let steps_before = svc.cluster.get_steps(job_id).len();
        // Simulate the client's retry loop: each attempt returns retryable Unavailable and must
        // leave the step count unchanged (the pre-fix bug created one step per attempt).
        for _ in 0..5 {
            let err = svc
                .create_job_step(Request::new(CreateJobStepRequest {
                    job_id,
                    command: vec!["hostname".into()],
                    num_tasks: 1,
                    cpus_per_task: 1,
                    overlap: true,
                    pty: true,
                    winsize: None,
                    node: "ghost".into(),
                    user: "u".into(),
                }))
                .await
                .expect_err("unregistered target must fail");
            assert_eq!(err.code(), Code::Unavailable);
        }
        assert_eq!(
            svc.cluster.get_steps(job_id).len(),
            steps_before,
            "no step should be created when the target node is unregistered"
        );
    }

    // exec_in_job and create_job_step are what keep job_entry (backing
    // exec_in_job and interactive_session/attach) and the step-dispatch path
    // from ever reaching a node before its LaunchJob is confirmed: both
    // reject here, before any node is even resolved, for a job that hasn't
    // reached Running. Pin that both gates actually exist and fire, since
    // confirm_dispatch_on_nodes closing the race for those RPCs depends on
    // this check never being bypassed or accidentally dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_in_job_rejects_when_job_not_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;

        let spec = spur_core::job::JobSpec {
            name: "pending-exec".into(),
            user: "u".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        let job_id = svc.cluster.submit_job(spec).unwrap().job_id;

        let err = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id,
                command: vec!["hostname".into()],
                user: "u".into(),
            }))
            .await
            .expect_err("a still-Pending job must not be exec'd into");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_job_step_rejects_when_job_not_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;

        let mut spec = spur_core::job::JobSpec {
            name: "pending-step".into(),
            user: "u".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        spec.srun_job = true;
        let job_id = svc.cluster.submit_job(spec).unwrap().job_id;

        let err = svc
            .create_job_step(Request::new(CreateJobStepRequest {
                job_id,
                command: vec!["hostname".into()],
                user: "u".into(),
                num_tasks: 1,
                cpus_per_task: 1,
                overlap: true,
                pty: false,
                winsize: None,
                node: String::new(),
            }))
            .await
            .expect_err("a still-Pending job must not accept a new step");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    /// Submit and start a single-node job owned by `owner`, returning its id.
    async fn running_job_owned_by(svc: &ControllerService, owner: &str) -> u32 {
        use spur_core::resource::{ResourceAllocations, ResourceSet};

        svc.cluster
            .register_node(
                "n1".into(),
                "n1".into(),
                ResourceSet {
                    cpus: 8,
                    memory_mb: 16000,
                    ..Default::default()
                },
                "127.0.0.1".into(),
                6818,
                String::new(),
                String::new(),
                spur_core::node::NodeSource::NativeHost,
                std::collections::HashMap::new(),
            )
            .unwrap();
        for _ in 0..200 {
            if svc.cluster.get_node("n1").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let spec = spur_core::job::JobSpec {
            name: "owned".into(),
            user: owner.into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        let job_id = svc.cluster.submit_job(spec).unwrap().job_id;

        let res = ResourceAllocations::with_scalar(1, 1000);
        let per_node: std::collections::HashMap<_, _> =
            [("n1".to_string(), res.clone())].into_iter().collect();
        svc.cluster
            .start_job(job_id, vec!["n1".into()], res, per_node)
            .unwrap();
        for _ in 0..200 {
            if svc.cluster.get_job(job_id).map(|j| j.state) == Some(JobState::Running) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        job_id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_in_job_denies_non_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let job_id = running_job_owned_by(&svc, "ubuntu").await;

        let err = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id,
                command: vec!["whoami".into()],
                user: "rsikande".into(),
            }))
            .await
            .expect_err("a non-owner must not exec inside another user's job");

        assert_eq!(err.code(), Code::PermissionDenied);
    }

    /// A REST submission omitting `user` records no owner. Such a job runs as
    /// root, so non-root users must be denied to prevent privilege escalation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_in_job_denies_non_root_on_empty_owner_job() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let job_id = running_job_owned_by(&svc, "").await;

        let err = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id,
                command: vec!["whoami".into()],
                user: "alice".into(),
            }))
            .await
            .expect_err("empty-owner jobs run as root; non-root must be denied");

        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_job_step_denies_non_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let job_id = running_job_owned_by(&svc, "ubuntu").await;

        let steps_before = svc.cluster.get_steps(job_id).len();
        let err = svc
            .create_job_step(Request::new(CreateJobStepRequest {
                job_id,
                command: vec!["bash".into()],
                num_tasks: 1,
                cpus_per_task: 1,
                overlap: true,
                pty: true,
                winsize: None,
                node: String::new(),
                user: "rsikande".into(),
            }))
            .await
            .expect_err("a non-owner must not attach to another user's job");

        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(
            svc.cluster.get_steps(job_id).len(),
            steps_before,
            "a denied attach must not leave a step behind"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_job_step_allows_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let job_id = running_job_owned_by(&svc, "ubuntu").await;

        let resp = svc
            .create_job_step(Request::new(CreateJobStepRequest {
                job_id,
                command: vec!["bash".into()],
                num_tasks: 1,
                cpus_per_task: 1,
                overlap: true,
                pty: true,
                winsize: None,
                node: String::new(),
                user: "ubuntu".into(),
            }))
            .await
            .expect("the owner must be allowed to attach");

        assert_eq!(resp.into_inner().node_addr, "127.0.0.1:6818");
    }

    async fn assign_ha_control_plane(svc: &ControllerService) {
        use spur_core::k0s::{K0sPhase, K0sRole};
        use spur_core::node::NodeSource;
        for (i, name) in ["cp-a", "cp-b", "cp-c"].iter().enumerate() {
            svc.cluster
                .register_node(
                    (*name).into(),
                    (*name).into(),
                    spur_core::resource::ResourceSet {
                        cpus: 4,
                        memory_mb: 8000,
                        ..Default::default()
                    },
                    "127.0.0.1".into(),
                    6818 + i as u16,
                    String::new(),
                    String::new(),
                    NodeSource::NativeHost,
                    std::collections::HashMap::new(),
                )
                .unwrap();
            svc.cluster
                .assign_node_k0s(
                    name,
                    K0sRole::Controller,
                    &format!("10.44.0.{}", i + 1),
                    "10.42.0.0/24",
                )
                .unwrap();
        }
        svc.cluster
            .set_k0s_phase(
                K0sPhase::Ready,
                Some("cp-a".into()),
                vec!["cp-a".into(), "cp-b".into(), "cp-c".into()],
                false,
            )
            .unwrap();
    }

    // A bare `spur k8s up` (no flags) on an assigned 3-CP cluster is idempotent: it must target the
    // recorded set, not re-resolve against the replicas=1 config default and spuriously reject.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_up_bare_reup_on_ha_cluster_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        assign_ha_control_plane(&svc).await;

        let resp = svc
            .cluster_up(Request::new(ClusterUpRequest::default()))
            .await
            .expect("bare re-up of an assigned HA cluster must be accepted")
            .into_inner();
        assert!(resp.accepted);
    }

    // An explicit replica count that doesn't match the assigned set is still rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_up_mismatched_replicas_on_ha_cluster_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        assign_ha_control_plane(&svc).await;

        let err = svc
            .cluster_up(Request::new(ClusterUpRequest {
                control_plane_replicas: Some(1),
                ..Default::default()
            }))
            .await
            .expect_err("shrinking an assigned 3-CP cluster must be rejected");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_up_denied_for_non_admin_caller() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_association("alice", "physics");
        let err = svc
            .cluster_up(Request::new(ClusterUpRequest {
                caller: "alice".into(),
                ..Default::default()
            }))
            .await
            .expect_err("a non-admin caller must not bring the cluster up");
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_down_denied_for_non_admin_caller() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_association("alice", "physics");
        let err = svc
            .cluster_down(Request::new(ClusterDownRequest {
                caller: "alice".into(),
                ..Default::default()
            }))
            .await
            .expect_err("a non-admin caller must not tear the cluster down");
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_up_allowed_for_admin_level_caller() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .register_node(
                "cp-a".into(),
                "cp-a".into(),
                spur_core::resource::ResourceSet {
                    cpus: 4,
                    memory_mb: 8000,
                    ..Default::default()
                },
                "127.0.0.1".into(),
                6818,
                String::new(),
                String::new(),
                spur_core::node::NodeSource::NativeHost,
                std::collections::HashMap::new(),
            )
            .unwrap();
        svc.cluster
            .association_cache()
            .insert_admin_level("carol", "Admin");
        let resp = svc
            .cluster_up(Request::new(ClusterUpRequest {
                caller: "carol".into(),
                control_plane_replicas: Some(1),
                ..Default::default()
            }))
            .await
            .expect("an Admin-level caller may bring the cluster up")
            .into_inner();
        assert!(resp.accepted);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_kubeconfig_admin_flag_denied_for_non_admin() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_association("alice", "physics");
        let err = svc
            .cluster_kubeconfig(Request::new(ClusterKubeconfigRequest {
                caller: "alice".into(),
                admin: true,
                ..Default::default()
            }))
            .await
            .expect_err("a non-admin must not get the cluster-admin kubeconfig");
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_kubeconfig_other_user_denied_for_non_admin() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_default_account("alice", "physics");
        let err = svc
            .cluster_kubeconfig(Request::new(ClusterKubeconfigRequest {
                caller: "alice".into(),
                user: "bob".into(),
                ..Default::default()
            }))
            .await
            .expect_err("a non-admin must not mint another user's kubeconfig");
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_kubeconfig_bare_self_scope_clears_authz_for_non_admin() {
        // A non-admin bare request (own scope) clears authz; the absent control-plane agent then
        // yields Unavailable, never PermissionDenied/InvalidArgument.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_default_account("alice", "physics");
        let err = svc
            .cluster_kubeconfig(Request::new(ClusterKubeconfigRequest {
                caller: "alice".into(),
                ..Default::default()
            }))
            .await
            .expect_err("no control-plane agent is running in this test");
        assert_eq!(err.code(), Code::Unavailable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_kubeconfig_admin_may_target_other_user() {
        // An Admin caller clears authz for another user's scope; failure is the absent agent
        // (Unavailable), not PermissionDenied.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let cache = svc.cluster.association_cache();
        cache.insert_admin_level("carol", "Admin");
        cache.insert_default_account("bob", "chem");
        let err = svc
            .cluster_kubeconfig(Request::new(ClusterKubeconfigRequest {
                caller: "carol".into(),
                user: "bob".into(),
                ..Default::default()
            }))
            .await
            .expect_err("no control-plane agent is running in this test");
        assert_eq!(err.code(), Code::Unavailable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_kubeconfig_admin_flag_allowed_for_admin_level_caller() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        svc.cluster
            .association_cache()
            .insert_admin_level("carol", "Admin");
        let err = svc
            .cluster_kubeconfig(Request::new(ClusterKubeconfigRequest {
                caller: "carol".into(),
                admin: true,
                ..Default::default()
            }))
            .await
            .expect_err("no control-plane agent is running in this test");
        assert_eq!(err.code(), Code::Unavailable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_up_denied_for_named_caller_when_accounting_off() {
        // Cold cache = accounting disabled: a named caller is not admin, so up fails closed.
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;
        let err = svc
            .cluster_up(Request::new(ClusterUpRequest {
                caller: "alice".into(),
                ..Default::default()
            }))
            .await
            .expect_err("a named caller must be denied when accounting is off");
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[test]
    fn select_step_node_empty_request_uses_first_allocated() {
        let allocated = vec!["node001".to_string(), "node002".to_string()];
        assert_eq!(select_step_node(&allocated, ""), Ok("node001"));
    }

    #[test]
    fn select_step_node_named_request_targets_that_node() {
        let allocated = vec!["node001".to_string(), "node002".to_string()];
        assert_eq!(select_step_node(&allocated, "node002"), Ok("node002"));
    }

    #[test]
    fn select_step_node_rejects_node_outside_allocation() {
        let allocated = vec!["node001".to_string(), "node002".to_string()];
        let err = select_step_node(&allocated, "node999").unwrap_err();
        assert!(err.contains("node999"));
        assert!(err.contains("not allocated"));
    }

    #[test]
    fn select_step_node_empty_request_no_nodes_errors() {
        let allocated: Vec<String> = Vec::new();
        assert!(select_step_node(&allocated, "").is_err());
    }

    #[test]
    fn select_step_node_rejects_comma_joined_request() {
        let allocated = vec!["node001".to_string(), "node002".to_string()];
        assert!(select_step_node(&allocated, "node001,node002").is_err());
    }

    #[test]
    fn job_to_proto_output_path_prefers_actual_else_absolute_computed() {
        use spur_core::job::{Job, JobSpec};

        let mut job = Job::new(
            42,
            JobSpec {
                work_dir: "/home/alice".into(),
                ..Default::default()
            },
        );

        // Unset: absolute computed fallback.
        let info = job_to_proto(&job);
        assert_eq!(info.stdout_path, "/home/alice/spur-42.out");
        assert_eq!(info.stderr_path, "/home/alice/spur-42.out");

        // Set: reported path wins.
        job.actual_stdout_path = Some("/tmp/spur-42.out".into());
        job.actual_stderr_path = Some("/tmp/spur-42.out".into());
        let info = job_to_proto(&job);
        assert_eq!(info.stdout_path, "/tmp/spur-42.out");
        assert_eq!(info.stderr_path, "/tmp/spur-42.out");
    }

    #[test]
    fn resolve_max_nodes_update_maps_intents() {
        // `--max-nodes 0` (Some(0), flag unset) must resolve to a clear, not a
        // silent no-op: cluster.rs only clears when the passed bool is true.
        assert_eq!(resolve_max_nodes_update(Some(0), false), (None, true));
        // Explicit clear flag, regardless of value.
        assert_eq!(resolve_max_nodes_update(None, true), (None, true));
        assert_eq!(resolve_max_nodes_update(Some(4), true), (None, true));
        // A real positive cap is preserved and does not clear.
        assert_eq!(resolve_max_nodes_update(Some(4), false), (Some(4), false));
        // No value and no flag means "leave unchanged".
        assert_eq!(resolve_max_nodes_update(None, false), (None, false));
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
            owner: String::new(),
        }
    }

    fn core_node(name: &str, partition: &str) -> spur_core::node::Node {
        let mut n =
            spur_core::node::Node::new(name.into(), spur_core::resource::ResourceSet::default());
        n.partitions = vec![partition.into()];
        n
    }

    #[test]
    fn node_info_includes_available_features() {
        let mut node = core_node("gpu-node1", "gpu");
        node.features = vec!["mi350x".into(), "atl".into()];

        let info = node_to_proto(&node);

        assert_eq!(info.features, ["mi350x", "atl"]);
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
            owner: String::new(),
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
            owner: String::new(),
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

    #[test]
    fn requested_gpus_detail_per_task() {
        use spur_core::gpu_request::GpuRequest;
        use spur_core::job::JobSpec;

        let spec = JobSpec {
            num_nodes: 2,
            num_tasks: 4,
            cpus_per_task: 1,
            gpus_per_task: Some(GpuRequest::new(2, None)),
            ..Default::default()
        };
        assert_eq!(requested_gpus_detail(&spec), "gpu:2/task");
    }

    #[test]
    fn requested_gpus_detail_per_task_typed() {
        use spur_core::gpu_request::GpuRequest;
        use spur_core::job::JobSpec;

        let spec = JobSpec {
            num_nodes: 2,
            num_tasks: 4,
            cpus_per_task: 1,
            gpus_per_task: Some(GpuRequest::new(4, Some("mi300x".into()))),
            ..Default::default()
        };
        assert_eq!(requested_gpus_detail(&spec), "gpu:mi300x:4/task");
    }

    #[test]
    fn requested_gpus_detail_total() {
        use spur_core::gpu_request::GpuRequest;
        use spur_core::job::JobSpec;

        let spec = JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            cpus_per_task: 1,
            gpus: Some(GpuRequest::new(8, None)),
            ..Default::default()
        };
        assert_eq!(requested_gpus_detail(&spec), "gpu:8");
    }

    #[test]
    fn requested_gpus_detail_per_node() {
        use spur_core::gpu_request::GpuRequest;
        use spur_core::job::JobSpec;

        let spec = JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            cpus_per_task: 1,
            gpus_per_node: Some(GpuRequest::new(4, None)),
            ..Default::default()
        };
        assert_eq!(requested_gpus_detail(&spec), "gpu:4/node");
    }

    #[test]
    fn proto_to_job_spec_defaults_absent_ntasks_to_nodes() {
        // C1: num_tasks=0 (unset over the wire) defaults to one task per node,
        // not 1, so the node count is not silently collapsed.
        let spec = spur_proto::proto::JobSpec {
            num_nodes: 4,
            num_tasks: 0,
            ..Default::default()
        };
        let core = proto_to_job_spec(spec).unwrap();
        assert_eq!(core.num_tasks, 4);
        assert_eq!(core.effective_num_nodes(), 4);
    }

    #[test]
    fn proto_to_job_spec_respects_explicit_single_task() {
        // An explicit --ntasks=1 is preserved and reduces the job to one node.
        let spec = spur_proto::proto::JobSpec {
            num_nodes: 4,
            num_tasks: 1,
            ..Default::default()
        };
        let core = proto_to_job_spec(spec).unwrap();
        assert_eq!(core.num_tasks, 1);
        assert_eq!(core.effective_num_nodes(), 1);
    }

    #[test]
    fn proto_to_job_spec_defers_gpu_validation() {
        // C3: proto_to_job_spec no longer rejects -N4 -n1 --gpus=2. GPU demand
        // is validated in submit_job against the normalized (1) node count.
        let spec = spur_proto::proto::JobSpec {
            num_nodes: 4,
            num_tasks: 1,
            gpus: Some(spur_proto::proto::GpuRequest {
                count: 2,
                gpu_type: String::new(),
            }),
            ..Default::default()
        };
        assert!(proto_to_job_spec(spec).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_reservations_filters_by_name() {
        use spur_core::resource::ResourceSet;
        let dir = tempfile::TempDir::new().unwrap();
        let svc = test_service(&dir).await;

        for n in ["n1", "n2"] {
            svc.cluster
                .register_node(
                    n.into(),
                    n.into(),
                    ResourceSet {
                        cpus: 4,
                        memory_mb: 8000,
                        ..Default::default()
                    },
                    "127.0.0.1".into(),
                    6818,
                    String::new(),
                    String::new(),
                    spur_core::node::NodeSource::NativeHost,
                    std::collections::HashMap::new(),
                )
                .unwrap();
        }
        for _ in 0..200 {
            if svc.cluster.get_node("n1").is_some() && svc.cluster.get_node("n2").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        for (name, node) in [("rocm_patch", "n1"), ("other_resv", "n2")] {
            svc.create_reservation(Request::new(CreateReservationRequest {
                name: name.into(),
                start_time: "now".into(),
                duration_minutes: 60,
                nodes: vec![node.into()],
                accounts: Vec::new(),
                users: Vec::new(),
                flags: Vec::new(),
                user: String::new(),
            }))
            .await
            .unwrap();
        }

        let one = svc
            .list_reservations(Request::new(ListReservationsRequest {
                name: "rocm_patch".into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .reservations;
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "rocm_patch");

        let all = svc
            .list_reservations(Request::new(ListReservationsRequest {
                name: String::new(),
            }))
            .await
            .unwrap()
            .into_inner()
            .reservations;
        assert_eq!(all.len(), 2);

        // A whitespace-only name is trimmed to empty and treated as no filter.
        let blank = svc
            .list_reservations(Request::new(ListReservationsRequest { name: "   ".into() }))
            .await
            .unwrap()
            .into_inner()
            .reservations;
        assert_eq!(blank.len(), 2);

        let none = svc
            .list_reservations(Request::new(ListReservationsRequest {
                name: "does_not_exist".into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .reservations;
        assert!(none.is_empty());
    }
}
