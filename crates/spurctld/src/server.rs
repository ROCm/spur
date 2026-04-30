use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tonic::metadata::MetadataValue;
use tonic::{Request, Response, Status};

use spur_core::reservation::Reservation;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::slurm_controller_server::{SlurmController, SlurmControllerServer};
use spur_proto::proto::*;

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

const FORWARDED_HEADER: &str = "x-spur-forwarded";
const LEADER_HEADER: &str = "x-spur-leader";

pub struct ControllerService {
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
    leader_proxy: LeaderProxy,
    /// Node ID → client API address (host:6817) for the x-spur-leader header.
    client_addrs: BTreeMap<u64, String>,
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
            .map_err(|e| Status::unavailable(format!("cannot reach leader: {e}")))?;

        *cached = Some((leader_id, client.clone()));
        Ok(client)
    }
}

impl ControllerService {
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
}

#[tonic::async_trait]
impl SlurmController for ControllerService {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.submit_job(fwd).await;
            }
            return Err(status);
        }

        let spec = request
            .into_inner()
            .spec
            .ok_or_else(|| Status::invalid_argument("missing job spec"))?;

        let core_spec = proto_to_job_spec(spec)?;
        let job_id = self
            .cluster
            .submit_job(core_spec)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(SubmitJobResponse { job_id }))
    }

    async fn get_jobs(
        &self,
        request: Request<GetJobsRequest>,
    ) -> Result<Response<GetJobsResponse>, Status> {
        if let Err(_) = self.check_leader(&request) {
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
            .filter_map(|s| proto_to_job_state(*s))
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

        let jobs = self
            .cluster
            .get_jobs(&states, user, partition, account, &req.job_ids);

        let proto_jobs: Vec<JobInfo> = jobs.iter().map(job_to_proto).collect();

        Ok(Response::new(GetJobsResponse { jobs: proto_jobs }))
    }

    async fn get_job(&self, request: Request<GetJobRequest>) -> Result<Response<JobInfo>, Status> {
        if let Err(_) = self.check_leader(&request) {
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
            .get_job(job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not found", job_id)))?;

        Ok(Response::new(job_to_proto(&job)))
    }

    async fn cancel_job(&self, request: Request<CancelJobRequest>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.cancel_job(fwd).await;
            }
            return Err(status);
        }

        let req = request.into_inner();
        self.cluster
            .cancel_job(req.job_id, &req.user)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn update_job(&self, request: Request<UpdateJobRequest>) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.update_job(fwd).await;
            }
            return Err(status);
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
        if let Err(_) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.get_nodes(fwd).await;
            }
        }

        let _req = request.into_inner();
        let nodes = self.cluster.get_nodes();
        let mut proto_nodes: Vec<NodeInfo> = nodes.iter().map(node_to_proto).collect();
        let reservations = self.cluster.get_reservations();
        annotate_nodes_with_reservations(&mut proto_nodes, &reservations, Utc::now());
        Ok(Response::new(GetNodesResponse { nodes: proto_nodes }))
    }

    async fn get_node(
        &self,
        request: Request<GetNodeRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        if let Err(_) = self.check_leader(&request) {
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
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.update_node(fwd).await;
            }
            return Err(status);
        }

        let req = request.into_inner();
        if let Some(state) = req.state {
            let node_state = proto_to_node_state(state)
                .ok_or_else(|| Status::invalid_argument("invalid node state"))?;
            self.cluster
                .update_node_state(&req.name, node_state, req.reason)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        Ok(Response::new(()))
    }

    async fn get_partitions(
        &self,
        request: Request<GetPartitionsRequest>,
    ) -> Result<Response<GetPartitionsResponse>, Status> {
        if let Err(_) = self.check_leader(&request) {
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

    async fn register_agent(
        &self,
        request: Request<RegisterAgentRequest>,
    ) -> Result<Response<RegisterAgentResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.register_agent(fwd).await;
            }
            return Err(status);
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

        self.cluster
            .register_node(
                req.hostname.clone(),
                resources,
                agent_addr,
                agent_port,
                req.wg_pubkey,
                req.version,
                spur_core::node::NodeSource::BareMetal,
            )
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(RegisterAgentResponse {
            accepted: true,
            message: "registered".into(),
        }))
    }

    async fn report_job_status(
        &self,
        request: Request<ReportJobStatusRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.report_job_status(fwd).await;
            }
            return Err(status);
        }

        let req = request.into_inner();
        let state = proto_to_job_state(req.state)
            .ok_or_else(|| Status::invalid_argument("invalid job state"))?;

        if state.is_terminal() {
            let exit_code = req.exit_code;
            self.cluster
                .complete_job(req.job_id, exit_code, state)
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        Ok(Response::new(()))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.heartbeat(fwd).await;
            }
            #[allow(unreachable_code)]
            return Err(status);
        }

        let req = request.into_inner();
        if self
            .cluster
            .update_heartbeat(&req.hostname, req.cpu_load, req.free_memory_mb)
        {
            Ok(Response::new(HeartbeatResponse {}))
        } else {
            Err(Status::not_found(format!(
                "node {} not found — is the node registered?",
                req.hostname
            )))
        }
    }

    async fn get_job_steps(
        &self,
        request: Request<GetJobStepsRequest>,
    ) -> Result<Response<GetJobStepsResponse>, Status> {
        if let Err(_) = self.check_leader(&request) {
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
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.create_job_step(fwd).await;
            }
            return Err(status);
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
            resources: spur_core::resource::ResourceSet::default(),
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
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.create_reservation(fwd).await;
            }
            return Err(status);
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

        let reservation = spur_core::reservation::Reservation {
            name: req.name,
            start_time,
            end_time,
            nodes: req.nodes,
            accounts: req.accounts,
            users: req.users,
        };

        self.cluster
            .create_reservation(reservation)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(()))
    }

    async fn update_reservation(
        &self,
        request: Request<UpdateReservationRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.update_reservation(fwd).await;
            }
            return Err(status);
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
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn delete_reservation(
        &self,
        request: Request<DeleteReservationRequest>,
    ) -> Result<Response<()>, Status> {
        if let Err(status) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.delete_reservation(fwd).await;
            }
            return Err(status);
        }

        let name = request.into_inner().name;
        self.cluster
            .delete_reservation(&name)
            .map_err(|e| Status::not_found(e.to_string()))?;
        Ok(Response::new(()))
    }

    async fn list_reservations(
        &self,
        request: Request<ListReservationsRequest>,
    ) -> Result<Response<ListReservationsResponse>, Status> {
        if let Err(_) = self.check_leader(&request) {
            {
                let proxy = &self.leader_proxy;
                let mut client = proxy.get_leader_client().await?;
                let mut fwd = Request::new(request.into_inner());
                *fwd.metadata_mut() = Self::forwarded_metadata();
                return client.list_reservations(fwd).await;
            }
        }
        let reservations = self.cluster.get_reservations();
        let infos: Vec<ReservationInfo> = reservations
            .iter()
            .map(|r| ReservationInfo {
                name: r.name.clone(),
                start_time: r.start_time.to_rfc3339(),
                end_time: r.end_time.to_rfc3339(),
                nodes: r.nodes.join(","),
                accounts: r.accounts.join(","),
                users: r.users.join(","),
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
        if let Err(_) = self.check_leader(&request) {
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
            })?;

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
        if let Err(_) = self.check_leader(&request) {
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
            })?;

        let agent_resp = agent
            .run_command(RunCommandRequest {
                command: req.command,
                uid: req.uid,
                gid: req.gid,
                work_dir: req.work_dir,
                environment: req.environment,
            })
            .await
            .map_err(|e| Status::internal(format!("run_command failed: {}", e)))?
            .into_inner();

        Ok(Response::new(RunStepResponse {
            exit_code: agent_resp.exit_code,
            stdout: agent_resp.stdout,
            stderr: agent_resp.stderr,
            node: node_name,
        }))
    }
}

pub async fn serve(
    addr: SocketAddr,
    cluster: Arc<ClusterManager>,
    raft_handle: Arc<RaftHandle>,
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
        raft: raft_handle,
        leader_proxy,
    };

    tonic::transport::Server::builder()
        .add_service(SlurmControllerServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

// ---- Proto conversion helpers ----

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

fn proto_to_job_state(s: i32) -> Option<spur_core::job::JobState> {
    match s {
        0 => Some(spur_core::job::JobState::Pending),
        1 => Some(spur_core::job::JobState::Running),
        2 => Some(spur_core::job::JobState::Completing),
        3 => Some(spur_core::job::JobState::Completed),
        4 => Some(spur_core::job::JobState::Failed),
        5 => Some(spur_core::job::JobState::Cancelled),
        6 => Some(spur_core::job::JobState::Timeout),
        7 => Some(spur_core::job::JobState::NodeFail),
        8 => Some(spur_core::job::JobState::Preempted),
        9 => Some(spur_core::job::JobState::Suspended),
        _ => None,
    }
}

fn proto_to_node_state(s: i32) -> Option<spur_core::node::NodeState> {
    match s {
        0 => Some(spur_core::node::NodeState::Idle),
        1 => Some(spur_core::node::NodeState::Allocated),
        2 => Some(spur_core::node::NodeState::Mixed),
        3 => Some(spur_core::node::NodeState::Down),
        4 => Some(spur_core::node::NodeState::Drain),
        5 => Some(spur_core::node::NodeState::Draining),
        6 => Some(spur_core::node::NodeState::Error),
        7 => Some(spur_core::node::NodeState::Unknown),
        8 => Some(spur_core::node::NodeState::Suspended),
        _ => None,
    }
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
        state: job_state_to_proto(job.state) as i32,
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
        stdout_path: job.resolved_stdout(),
        stderr_path: job.resolved_stderr(),
        resources: job.allocated_resources.as_ref().map(resource_to_proto),
        priority: job.priority,
        qos: job.spec.qos.clone().unwrap_or_default(),
        array_job_id: job.spec.array_job_id.unwrap_or(0),
        array_task_id: job.spec.array_task_id.unwrap_or(0),
    }
}

fn node_to_proto(node: &spur_core::node::Node) -> NodeInfo {
    NodeInfo {
        name: node.name.clone(),
        state: node_state_to_proto(node.state) as i32,
        state_reason: node.state_reason.clone().unwrap_or_default(),
        partition: node.partitions.first().cloned().unwrap_or_default(),
        total_resources: Some(resource_to_proto(&node.total_resources)),
        alloc_resources: Some(resource_to_proto(&node.alloc_resources)),
        arch: node.arch.clone(),
        os: node.os.clone(),
        cpu_load: node.cpu_load,
        free_memory_mb: node.free_memory_mb,
        boot_time: node.boot_time.map(datetime_to_proto),
        last_busy: node.last_busy.map(datetime_to_proto),
        slurmd_start_time: node.agent_start_time.map(datetime_to_proto),
        switch_name: node.switch_name.clone().unwrap_or_default(),
        active_reservation: String::new(),
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
        preempt_mode: format!("{:?}", part.preempt_mode),
        priority_tier: part.priority_tier,
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

pub(crate) fn job_state_to_proto(s: spur_core::job::JobState) -> spur_proto::proto::JobState {
    match s {
        spur_core::job::JobState::Pending => spur_proto::proto::JobState::JobPending,
        spur_core::job::JobState::Running => spur_proto::proto::JobState::JobRunning,
        spur_core::job::JobState::Completing => spur_proto::proto::JobState::JobCompleting,
        spur_core::job::JobState::Completed => spur_proto::proto::JobState::JobCompleted,
        spur_core::job::JobState::Failed => spur_proto::proto::JobState::JobFailed,
        spur_core::job::JobState::Cancelled => spur_proto::proto::JobState::JobCancelled,
        spur_core::job::JobState::Timeout => spur_proto::proto::JobState::JobTimeout,
        spur_core::job::JobState::NodeFail => spur_proto::proto::JobState::JobNodeFail,
        spur_core::job::JobState::Preempted => spur_proto::proto::JobState::JobPreempted,
        spur_core::job::JobState::Suspended => spur_proto::proto::JobState::JobSuspended,
    }
}

fn node_state_to_proto(s: spur_core::node::NodeState) -> spur_proto::proto::NodeState {
    match s {
        spur_core::node::NodeState::Idle => spur_proto::proto::NodeState::NodeIdle,
        spur_core::node::NodeState::Allocated => spur_proto::proto::NodeState::NodeAllocated,
        spur_core::node::NodeState::Mixed => spur_proto::proto::NodeState::NodeMixed,
        spur_core::node::NodeState::Down => spur_proto::proto::NodeState::NodeDown,
        spur_core::node::NodeState::Drain => spur_proto::proto::NodeState::NodeDrain,
        spur_core::node::NodeState::Draining => spur_proto::proto::NodeState::NodeDraining,
        spur_core::node::NodeState::Error => spur_proto::proto::NodeState::NodeError,
        spur_core::node::NodeState::Unknown => spur_proto::proto::NodeState::NodeUnknown,
        spur_core::node::NodeState::Suspended => spur_proto::proto::NodeState::NodeSuspended,
    }
}

pub(crate) fn datetime_to_proto(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

fn annotate_nodes_with_reservations(
    nodes: &mut [NodeInfo],
    reservations: &[Reservation],
    now: DateTime<Utc>,
) {
    for node_info in nodes.iter_mut() {
        for res in reservations {
            if res.is_active(now) && res.covers_node(&node_info.name) {
                node_info.active_reservation = res.name.clone();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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
        }
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
}
