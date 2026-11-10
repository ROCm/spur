// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::cgroup_cleanup::{PendingCleanup, DRAIN_REASON};
use crate::stepd::{StepdDescriptor, StepdObligation, StepdStore};
use spur_core::resource::ResourceSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct ControllerState {
    accept: AtomicBool,
    reports: std::sync::Mutex<Vec<ReportJobStatusRequest>>,
}

struct CleanupController(Arc<ControllerState>);

macro_rules! cleanup_controller_impl {
    ($($method:ident($req:ty) -> $resp:ty;)*) => {
        #[tonic::async_trait]
        impl spur_proto::proto::slurm_controller_server::SlurmController for CleanupController {
            async fn report_job_status(
                &self,
                request: Request<ReportJobStatusRequest>,
            ) -> Result<Response<()>, Status> {
                self.0.reports.lock().expect("reports").push(request.into_inner());
                if !self.0.accept.load(Ordering::SeqCst) {
                    return Err(Status::permission_denied("completion refused by fixture"));
                }
                Ok(Response::new(()))
            }

            async fn drain_node(
                &self,
                _request: Request<spur_proto::proto::DrainNodeRequest>,
            ) -> Result<Response<spur_proto::proto::DrainNodeResponse>, Status> {
                Err(Status::unimplemented("drain_node"))
            }

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

cleanup_controller_impl! {
        submit_job(spur_proto::proto::SubmitJobRequest) -> spur_proto::proto::SubmitJobResponse;
        get_jobs(spur_proto::proto::GetJobsRequest) -> spur_proto::proto::GetJobsResponse;
        get_job(spur_proto::proto::GetJobRequest) -> spur_proto::proto::JobInfo;
        cancel_job(spur_proto::proto::CancelJobRequest) -> ();
        complete_job(spur_proto::proto::CompleteJobRequest) -> ();
        job_keepalive(spur_proto::proto::JobKeepaliveRequest) -> spur_proto::proto::JobKeepaliveResponse;
        suspend_job(spur_proto::proto::SuspendJobRequest) -> ();
        resume_job(spur_proto::proto::ResumeJobRequest) -> ();
        update_job(spur_proto::proto::UpdateJobRequest) -> ();
        requeue_job(spur_proto::proto::RequeueJobRequest) -> spur_proto::proto::RequeueJobResponse;
        get_nodes(spur_proto::proto::GetNodesRequest) -> spur_proto::proto::GetNodesResponse;
        get_node(spur_proto::proto::GetNodeRequest) -> spur_proto::proto::NodeInfo;
        update_node(spur_proto::proto::UpdateNodeRequest) -> ();
        deregister_node(spur_proto::proto::DeregisterNodeRequest) -> spur_proto::proto::DeregisterNodeResponse;
        deregister_agent(spur_proto::proto::DeregisterAgentRequest) -> ();
        get_partitions(spur_proto::proto::GetPartitionsRequest) -> spur_proto::proto::GetPartitionsResponse;
        get_job_steps(spur_proto::proto::GetJobStepsRequest) -> spur_proto::proto::GetJobStepsResponse;
        create_job_step(spur_proto::proto::CreateJobStepRequest) -> spur_proto::proto::CreateJobStepResponse;
        complete_job_step(spur_proto::proto::CompleteJobStepRequest) -> ();
        ping(()) -> spur_proto::proto::PingResponse;
        get_job_metrics(()) -> spur_proto::proto::JobMetrics;
        get_node_metrics(()) -> spur_proto::proto::NodeMetrics;
        get_rpc_stats(()) -> spur_proto::proto::RpcStats;
        reset_diag_stats(()) -> ();
        get_sched_stats(()) -> spur_proto::proto::SchedStats;
        get_assoc_mgr_info(spur_proto::proto::GetAssocMgrInfoRequest) -> spur_proto::proto::GetAssocMgrInfoResponse;
        register_agent(spur_proto::proto::RegisterAgentRequest) -> spur_proto::proto::RegisterAgentResponse;
        heartbeat(spur_proto::proto::HeartbeatRequest) -> spur_proto::proto::HeartbeatResponse;
        report_stepd_recovery(spur_proto::proto::StepdRecoveryRequest) -> spur_proto::proto::StepdRecoveryResponse;
        create_token(spur_proto::proto::CreateTokenRequest) -> spur_proto::proto::CreateTokenResponse;
        list_tokens(spur_proto::proto::ListTokensRequest) -> spur_proto::proto::ListTokensResponse;
        revoke_token(spur_proto::proto::RevokeTokenRequest) -> spur_proto::proto::RevokeTokenResponse;
        create_partition(spur_proto::proto::CreatePartitionRequest) -> ();
        update_partition(spur_proto::proto::UpdatePartitionRequest) -> ();
        delete_partition(spur_proto::proto::DeletePartitionRequest) -> ();
        reconfigure(()) -> ();
        create_reservation(spur_proto::proto::CreateReservationRequest) -> ();
        update_reservation(spur_proto::proto::UpdateReservationRequest) -> ();
        delete_reservation(spur_proto::proto::DeleteReservationRequest) -> ();
        list_reservations(spur_proto::proto::ListReservationsRequest) -> spur_proto::proto::ListReservationsResponse;
        exec_in_job(spur_proto::proto::ExecInJobRequest) -> spur_proto::proto::ExecInJobResponse;
        run_step(spur_proto::proto::RunStepRequest) -> spur_proto::proto::RunStepResponse;
        cluster_up(spur_proto::proto::ClusterUpRequest) -> spur_proto::proto::ClusterUpResponse;
        cluster_down(spur_proto::proto::ClusterDownRequest) -> spur_proto::proto::ClusterDownResponse;
        cluster_status(spur_proto::proto::ClusterStatusRequest) -> spur_proto::proto::ClusterStatusResponse;
        cluster_kubeconfig(spur_proto::proto::ClusterKubeconfigRequest) -> spur_proto::proto::ClusterKubeconfigResponse;
        cluster_add_nodes(spur_proto::proto::ClusterAddNodesRequest) -> spur_proto::proto::ClusterAddNodesResponse;
        cluster_remove_nodes(spur_proto::proto::ClusterRemoveNodesRequest) -> spur_proto::proto::ClusterRemoveNodesResponse;
        raft_add_learner(spur_proto::proto::RaftAddLearnerRequest) -> ();
        raft_promote_voter(spur_proto::proto::RaftPromoteVoterRequest) -> ();
        raft_remove_voter(spur_proto::proto::RaftRemoveVoterRequest) -> ();
        raft_membership(spur_proto::proto::RaftMembershipRequest) -> spur_proto::proto::RaftMembershipResponse;
        get_fabric_topology(()) -> spur_proto::proto::GetFabricTopologyResponse;
        validate_block_file(spur_proto::proto::ValidateBlockFileRequest) -> spur_proto::proto::GetFabricTopologyResponse;

}

struct ControllerFixture {
    address: String,
    state: Arc<ControllerState>,
    server: tokio::task::JoinHandle<()>,
}

impl ControllerFixture {
    fn new(accept: bool) -> Self {
        let incoming = tonic::transport::server::TcpIncoming::bind(
            "127.0.0.1:0".parse().expect("loopback address"),
        )
        .expect("bind controller fixture");
        let address = format!(
            "http://{}",
            incoming.local_addr().expect("controller address")
        );
        let state = Arc::new(ControllerState::default());
        state.accept.store(accept, Ordering::SeqCst);
        let service = CleanupController(state.clone());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(spur_proto::controller_server(service))
                .serve_with_incoming(incoming)
                .await
                .expect("serve controller fixture");
        });
        Self {
            address,
            state,
            server,
        }
    }

    fn reports(&self) -> Vec<ReportJobStatusRequest> {
        self.state.reports.lock().expect("reports").clone()
    }
}

impl Drop for ControllerFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct SessionFixture {
    directory: tempfile::TempDir,
    store: StepdStore,
    descriptor: StepdDescriptor,
    cgroup: PathBuf,
}

impl SessionFixture {
    fn new(deadline_unix_ms: u64) -> Self {
        let directory = tempfile::tempdir().expect("fixture directory");
        let cgroup = directory.path().join("job_71_1");
        std::fs::create_dir(&cgroup).expect("create cgroup fixture");
        std::fs::write(cgroup.join("cgroup.kill"), "").expect("write kill fixture");
        std::fs::write(cgroup.join("cgroup.events"), "populated 1\nfrozen 0\n")
            .expect("write populated fixture");
        let store = StepdStore::new(directory.path());
        let step_id = spur_core::step::STEP_BATCH;
        let session = store
            .prepare_session_dir(71, 1, step_id)
            .expect("create session");
        let descriptor = StepdDescriptor::new(
            71,
            1,
            step_id,
            0,
            0,
            session.join("runtime.sock"),
            cgroup.clone(),
        );
        store.publish(&descriptor).expect("publish descriptor");
        store
            .obligations(71, 1, step_id)
            .append(&StepdObligation::ExitObserved {
                exit_code: 0,
                signal: nix::sys::signal::Signal::SIGKILL as i32,
            })
            .expect("record workload exit");
        store
            .cleanup_store()
            .track(&PendingCleanup {
                job_id: 71,
                run_attempt: 1,
                step_id,
                cgroup: cgroup.clone(),
                quarantine_acknowledged: false,
                deadline_unix_ms,
            })
            .expect("record cleanup");
        Self {
            directory,
            store,
            descriptor,
            cgroup,
        }
    }

    fn report(&self) -> CompletionReport<'_> {
        CompletionReport {
            job_id: self.descriptor.job_id,
            run_attempt: self.descriptor.run_attempt,
            step_id: Some(self.descriptor.step_id),
            exit_code: 0,
            signal: nix::sys::signal::Signal::SIGKILL as i32,
            reporting_node: "cleanup-node",
            drain: None,
            node_token: String::new(),
            pty_step_id: None,
        }
    }

    fn acknowledged(&self) -> bool {
        self.store
            .completion_reported(
                self.descriptor.job_id,
                self.descriptor.run_attempt,
                self.descriptor.step_id,
            )
            .expect("read acknowledgment")
    }

    fn agent(&self, controller: &ControllerFixture) -> AgentService {
        let reporter = Arc::new(NodeReporter::new(
            "cleanup-node".into(),
            controller.address.clone(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "cleanup-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            HashMap::new(),
            String::new(),
            String::new(),
            self.directory.path().join("wireguard"),
            new_running_jobs(),
            crate::reporter::ReporterTimeouts::default(),
        ));
        AgentService::new(
            reporter,
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(self.directory.path())
    }

    async fn track_allocation(&self, agent: &AgentService) {
        let mut tracked = TrackedJob::allocation_only(Some(self.cgroup.clone()));
        tracked.run_attempt = self.descriptor.run_attempt;
        agent
            .running
            .lock()
            .await
            .insert(self.descriptor.job_id, tracked);
        agent
            .stepds
            .lock()
            .await
            .insert(stepd_key(&self.descriptor), self.descriptor.clone());
        let mut allocation = agent.allocation.lock().await;
        allocation
            .allocate_for_job(
                self.descriptor.job_id,
                self.descriptor.run_attempt,
                1,
                128,
                &[],
            )
            .expect("allocate fixture resources");
        assert!(allocation.commit_job(self.descriptor.job_id, self.descriptor.run_attempt));
    }
}

fn command_request(step_id: u32) -> Request<RunCommandRequest> {
    Request::new(RunCommandRequest {
        job_id: 71,
        step_id,
        command: vec!["true".into()],
        uid: 1000,
        gid: 1000,
        ..Default::default()
    })
}

#[tokio::test]
async fn command_replay_returns_an_acknowledged_exit_during_cleanup() {
    let controller = ControllerFixture::new(true);
    for memoized in [false, true] {
        let fixture = SessionFixture::new(0);
        let agent = fixture.agent(&controller);
        fixture.track_allocation(&agent).await;
        let step_id = 3;
        let mut descriptor = fixture.descriptor.clone();
        descriptor.step_id = step_id;
        descriptor.socket_path = fixture
            .store
            .session_dir(71, 1, step_id)
            .join("runtime.sock");
        fixture.store.publish(&descriptor).expect("publish step");
        let obligations = fixture.store.obligations(71, 1, step_id);
        obligations
            .append(&StepdObligation::ExitObserved {
                exit_code: 9,
                signal: 0,
            })
            .expect("record exit");
        obligations
            .append(&StepdObligation::CompletionAcknowledged)
            .expect("record quarantine acknowledgment");
        if memoized {
            agent
                .step_completions
                .complete(
                    71,
                    1,
                    step_id,
                    crate::step_completion::StepOutcome {
                        exit_code: 9,
                        signal: 0,
                    },
                )
                .await;
        }

        let response = agent
            .run_command(command_request(step_id))
            .await
            .expect("retry returns the acknowledged result")
            .into_inner();

        assert_eq!(response.exit_code, 9);
        assert!(agent.active_steps.lock().await.is_empty());
        assert!(fixture.store.ensure_cleanup_ready().is_err());
    }
}

#[tokio::test]
async fn command_rejoins_a_running_step_during_unrelated_cleanup_but_refuses_a_new_step() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    let error = agent
        .run_command(command_request(4))
        .await
        .expect_err("fresh steps remain blocked");
    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    let mut descriptor = fixture.descriptor.clone();
    descriptor.step_id = 3;
    agent.stepds.lock().await.insert((71, 3), descriptor);
    let retry = agent.run_command(command_request(3));
    tokio::pin!(retry);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(retry.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(agent.step_completions.parked().await, 1);
    agent
        .step_completions
        .complete(
            71,
            1,
            3,
            crate::step_completion::StepOutcome {
                exit_code: 9,
                signal: 0,
            },
        )
        .await;
    assert_eq!(
        retry.await.expect("rejoined result").into_inner().exit_code,
        9
    );
    assert!(agent.active_steps.lock().await.is_empty());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn fresh_interactive_rpc_cannot_launch_during_cleanup() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    {
        let mut running = agent.running.lock().await;
        let job = running.get_mut(&71).expect("tracked allocation");
        job.uid = 1000;
        job.gid = 1000;
        job.user = "fixture-user".into();
    }
    let sessions = agent.stepds.clone();
    let active = agent.active_steps.clone();
    let incoming = tonic::transport::server::TcpIncoming::bind(
        "127.0.0.1:0".parse().expect("loopback address"),
    )
    .expect("bind agent fixture");
    let address = format!("http://{}", incoming.local_addr().expect("agent address"));
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(spur_proto::agent_server(agent))
            .serve_with_incoming(incoming)
            .await
            .expect("serve agent fixture");
    });
    let mut client = spur_proto::proto::slurm_agent_client::SlurmAgentClient::connect(address)
        .await
        .expect("connect agent fixture");
    let result = client
        .interactive_session(tokio_stream::iter([InteractiveInput {
            msg: Some(interactive_input::Msg::Init(InitSession {
                job_id: 71,
                step_id: 3,
                argv: vec!["true".into()],
                user: "fixture-user".into(),
                ..Default::default()
            })),
        }]))
        .await;
    server.abort();

    let error = result.expect_err("cleanup blocks a fresh terminal workload");
    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    assert!(!sessions
        .lock()
        .await
        .contains_key(&(71, spur_core::step::STEP_INTERACTIVE)));
    assert!(active.lock().await.is_empty());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn a_recorded_shell_exit_does_not_report_completion_while_cleanup_has_time() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(u64::MAX);

    assert!(
        !report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await
    );

    assert!(controller.reports().is_empty());
    assert!(!fixture.acknowledged());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
    assert_eq!(
        std::fs::read_to_string(fixture.cgroup.join("cgroup.kill")).expect("kill request"),
        "1"
    );
}

#[tokio::test]
async fn expired_cleanup_requires_controller_acknowledgment_and_remains_quarantined() {
    let controller = ControllerFixture::new(false);
    let fixture = SessionFixture::new(0);

    assert!(
        !report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await
    );
    assert!(!fixture.acknowledged());
    assert!(fixture.store.ensure_cleanup_ready().is_err());

    controller.state.accept.store(true, Ordering::SeqCst);
    assert!(report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await);

    let reports = controller.reports();
    assert_eq!(reports.len(), 2);
    for report in reports {
        assert!(report.drain_node);
        assert_eq!(report.drain_reason, DRAIN_REASON);
        assert_eq!(report.job_id, fixture.descriptor.job_id);
        assert_eq!(report.run_attempt, fixture.descriptor.run_attempt);
    }
    assert!(fixture.acknowledged());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
    assert!(fixture.cgroup.exists());
}

#[tokio::test]
async fn expired_cleanup_keeps_an_earlier_drain_reason() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let epilog = crate::agent_server::DrainRequest {
        reason: "epilog script failed".into(),
    };
    let report = CompletionReport {
        drain: Some(&epilog),
        ..fixture.report()
    };

    assert!(report_runtime_completion(&fixture.store, &controller.address, report).await);

    let reports = controller.reports();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].drain_node);
    assert_eq!(
        reports[0].drain_reason,
        format!("epilog script failed; {DRAIN_REASON}")
    );
}

#[tokio::test]
async fn unreadable_cgroup_requires_drain_instead_of_successful_cleanup() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    std::fs::remove_file(fixture.cgroup.join("cgroup.events")).expect("remove population fixture");

    assert!(report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await);

    let reports = controller.reports();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].drain_node);
    assert_eq!(reports[0].drain_reason, DRAIN_REASON);
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn confirmed_cleanup_reports_without_draining_and_unblocks_admission() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(u64::MAX);
    std::fs::remove_dir_all(&fixture.cgroup).expect("empty cgroup fixture");

    assert!(report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await);

    let reports = controller.reports();
    assert_eq!(reports.len(), 1);
    assert!(!reports[0].drain_node);
    assert!(reports[0].drain_reason.is_empty());
    assert!(fixture.acknowledged());
    fixture
        .store
        .ensure_cleanup_ready()
        .expect("cleanup finished");
}

#[tokio::test]
async fn restarted_agent_rejects_new_allocation_before_any_resource_is_reserved() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    let mut request = Request::new(RegisterJobAllocationRequest {
        job_id: 72,
        run_attempt: 1,
        user: "fixture-user".into(),
        uid: 1000,
        gid: 1000,
        cpus: 1,
        memory_mb: 128,
        ..Default::default()
    });
    request.extensions_mut().insert(spur_core::auth::Identity {
        user: spur_core::auth::CONTROLLER_SUBJECT.into(),
        uid: 0,
        gid: 0,
        is_admin: true,
        trusted_unix: false,
    });

    let error = agent
        .register_job_allocation(request)
        .await
        .expect_err("pending cleanup rejects admission");

    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    assert!(error.message().contains("unfinished job cgroup cleanup"));
    assert!(agent.running.lock().await.is_empty());
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 0);
    assert!(controller.reports().is_empty());
}

#[tokio::test]
async fn batch_resource_reservation_is_retryable_while_cleanup_is_pending_or_corrupt() {
    let controller = ControllerFixture::new(true);
    for corrupt in [false, true] {
        let fixture = SessionFixture::new(0);
        let agent = fixture.agent(&controller);
        if corrupt {
            std::fs::write(
                fixture
                    .store
                    .root()
                    .join("cleanup")
                    .join(format!("71.1.{}.json", spur_core::step::STEP_BATCH)),
                "invalid cleanup record",
            )
            .expect("corrupt cleanup record");
        }

        let error = agent
            .allocate_local_resources(72, 1, &JobSpec::default(), None, 1, 128)
            .await
            .expect_err("cleanup holds batch admission");

        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert!(error.message().contains("unfinished job cgroup cleanup"));
        assert!(agent.running.lock().await.is_empty());
        assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 0);
        assert!(fixture.store.ensure_cleanup_ready().is_err());
    }
    assert!(controller.reports().is_empty());
}

#[tokio::test]
async fn clean_released_session_replays_once_and_is_pruned() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let descriptor = &fixture.descriptor;
    let session = fixture.store.session_dir(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    );
    std::fs::remove_dir_all(&fixture.cgroup).expect("empty cgroup fixture");
    crate::stepd::record_resources_released(descriptor).expect("record local release");

    let replayed = replay_unacknowledged_stepd_completions(
        &fixture.store,
        &controller.address,
        "cleanup-node",
        "",
    )
    .await
    .expect("replay a previously released session");

    assert_eq!(replayed, vec![(71, 1, spur_core::step::STEP_BATCH)]);
    assert!(!session.exists());
    fixture
        .store
        .ensure_cleanup_ready()
        .expect("cleanup complete");
    let repeated = replay_unacknowledged_stepd_completions(
        &fixture.store,
        &controller.address,
        "cleanup-node",
        "",
    )
    .await
    .expect("repeat completed replay");
    assert!(repeated.is_empty());
    assert_eq!(controller.reports().len(), 1);
    assert!(!controller.reports()[0].drain_node);
    assert!(!session.exists());
}

#[tokio::test]
async fn pending_cleanup_prevents_a_prepared_supervisor_from_opening_its_start_gate() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    let mut request = Request::new(spur_proto::proto::AgentStartJobRequest {
        job_id: fixture.descriptor.job_id,
        run_attempt: fixture.descriptor.run_attempt,
    });
    request.extensions_mut().insert(spur_core::auth::Identity {
        user: spur_core::auth::CONTROLLER_SUBJECT.into(),
        uid: 0,
        gid: 0,
        is_admin: true,
        trusted_unix: false,
    });

    let error = agent
        .start_job(request)
        .await
        .expect_err("gate stays closed");

    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    assert!(error.message().contains("unfinished job cgroup cleanup"));
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 128);
    assert!(agent
        .stepds
        .lock()
        .await
        .contains_key(&stepd_key(&fixture.descriptor)));
    assert!(controller.reports().is_empty());
}

#[tokio::test]
async fn a_live_supervisors_duplicate_notification_waits_for_durable_acknowledgment() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let controller = ControllerFixture::new(false);
    let mut fixture = SessionFixture::new(0);
    fixture.descriptor.pid = std::process::id();
    fixture.descriptor.process_start_ticks =
        crate::stepd::process_start_ticks(fixture.descriptor.pid).expect("fixture start ticks");
    fixture
        .store
        .publish(&fixture.descriptor)
        .expect("publish live supervisor");
    assert!(matches!(
        crate::stepd::stepd_liveness(&fixture.descriptor).expect("supervisor liveness"),
        crate::stepd::StepdLiveness::Live
    ));
    let agent = fixture.agent(&controller);
    assert!(agent.stepds.lock().await.is_empty());
    let notification = crate::stepd::AgentNotification::StepdCompleted {
        job_id: fixture.descriptor.job_id,
        run_attempt: fixture.descriptor.run_attempt,
        step_id: fixture.descriptor.step_id,
        exit_code: 0,
        signal: nix::sys::signal::Signal::SIGKILL as i32,
        epilog_failed: false,
        capability: fixture.descriptor.capability.clone(),
    };
    let (mut sender, receiver) = tokio::net::UnixStream::pair().expect("notification sockets");
    let mut payload = serde_json::to_vec(&notification).expect("serialize notification");
    payload.push(b'\n');
    sender.write_all(&payload).await.expect("send notification");

    handle_completion_notification(receiver, &agent.completion_listener_context())
        .await
        .expect("handle duplicate notification");

    let mut response = String::new();
    tokio::io::BufReader::new(sender)
        .read_line(&mut response)
        .await
        .expect("read notification response");
    assert_eq!(
        serde_json::from_str::<crate::stepd::AgentNotificationResponse>(&response)
            .expect("parse notification response"),
        crate::stepd::AgentNotificationResponse::Deferred
    );
    assert!(!fixture.acknowledged());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
    assert!(controller.reports().is_empty());
}

#[tokio::test]
async fn supervisor_acknowledgment_after_agent_pruning_does_not_recreate_the_session() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let descriptor = &fixture.descriptor;
    let session = fixture.store.session_dir(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    );
    std::fs::remove_dir_all(&fixture.cgroup).expect("empty cgroup fixture");
    crate::stepd::record_resources_released(descriptor).expect("record local release");
    assert!(report_runtime_completion(&fixture.store, &controller.address, fixture.report()).await);
    assert!(!session.exists());

    fixture
        .store
        .acknowledge_completion(&crate::stepd::PendingStepdCompletion {
            job_id: descriptor.job_id,
            run_attempt: descriptor.run_attempt,
            step_id: descriptor.step_id,
            exit_code: 0,
            signal: nix::sys::signal::Signal::SIGKILL as i32,
            epilog_failed: false,
            pty_step_id: None,
        })
        .expect("supervisor observes the acknowledgment after agent pruning");
    crate::stepd::record_resources_released(descriptor)
        .expect("repeated release tolerates agent pruning");

    assert!(!session.exists());
    assert!(fixture
        .store
        .discover_unacknowledged_completions()
        .expect("discover pending reports")
        .is_empty());
    assert_eq!(controller.reports().len(), 1);
}

#[tokio::test]
async fn dead_supervisor_keeps_resources_until_its_cleanup_report_can_be_replayed() {
    let controller = ControllerFixture::new(false);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    assert!(matches!(
        crate::stepd::stepd_liveness(&fixture.descriptor).expect("supervisor liveness"),
        crate::stepd::StepdLiveness::Stale
    ));

    fence_dead_stepd(
        &agent.completion_listener_context(),
        fixture.descriptor.clone(),
    )
    .await;

    assert!(!fixture.acknowledged());
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 128);
    assert!(agent
        .running
        .lock()
        .await
        .contains_key(&fixture.descriptor.job_id));
    assert!(fixture.store.ensure_cleanup_ready().is_err());
    let refused = controller.reports();
    assert_eq!(refused.len(), 1);
    assert!(refused[0].drain_node);

    agent
        .drop_tracked_job(fixture.descriptor.job_id, fixture.descriptor.run_attempt)
        .await;
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 128);

    let restarted_store = StepdStore::new(fixture.directory.path());
    controller.state.accept.store(true, Ordering::SeqCst);
    let replayed = replay_unacknowledged_stepd_completions(
        &restarted_store,
        &controller.address,
        "cleanup-node",
        "",
    )
    .await
    .expect("replay cleanup completion");

    assert_eq!(replayed, vec![(71, 1, spur_core::step::STEP_BATCH)]);
    let reports = controller.reports();
    assert_eq!(reports.len(), 2);
    assert!(reports[1].drain_node);
    assert!(fixture.acknowledged());
    assert!(restarted_store.ensure_cleanup_ready().is_err());
    std::fs::remove_dir_all(&fixture.cgroup).expect("empty cgroup fixture");
    restarted_store
        .retry_cleanup()
        .expect("retry eventual cleanup");
    restarted_store
        .ensure_cleanup_ready()
        .expect("quarantine clears after actual cleanup");
}

#[test]
fn pending_cleanup_keeps_an_old_finalized_session_recoverable() {
    let fixture = SessionFixture::new(0);
    let descriptor = &fixture.descriptor;
    let obligations = fixture.store.obligations(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    );
    obligations
        .append(&StepdObligation::CompletionAcknowledged)
        .expect("record acknowledged completion");
    obligations
        .append(&StepdObligation::ResourcesReleased)
        .expect("record released allocation");
    let session = fixture.store.session_dir(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    );
    std::fs::File::open(session.join("obligations.jsonl"))
        .expect("open obligations")
        .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .expect("age obligations");

    assert_eq!(fixture.store.prune_finalized().expect("prune sessions"), 0);
    fixture
        .store
        .load_descriptor(&session)
        .expect("pending cleanup retains its descriptor");
    assert_eq!(obligations.read().expect("read obligations").len(), 3);

    std::fs::remove_dir_all(&fixture.cgroup).expect("empty cgroup fixture");
    fixture.store.retry_cleanup().expect("finish cleanup");
    assert_eq!(fixture.store.prune_finalized().expect("prune sessions"), 1);
    assert!(!session.exists());
}

#[tokio::test]
async fn unrelated_allocation_can_drop_while_dirty_allocation_stays_reserved() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    let mut unrelated = TrackedJob::allocation_only(None);
    unrelated.run_attempt = 1;
    agent.running.lock().await.insert(72, unrelated);
    {
        let mut allocation = agent.allocation.lock().await;
        allocation
            .allocate_for_job(72, 1, 1, 256, &[])
            .expect("allocate unrelated resources");
        assert!(allocation.commit_job(72, 1));
        assert_eq!(allocation.allocated_memory_mb, 384);
    }

    agent.drop_tracked_job(72, 1).await;
    agent.drop_tracked_job(71, 1).await;

    let running = agent.running.lock().await;
    assert!(!running.contains_key(&72));
    assert!(running.contains_key(&71));
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 128);
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn an_older_attempts_cleanup_does_not_block_dropping_a_newer_allocation() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    let mut newer = TrackedJob::allocation_only(None);
    newer.run_attempt = 2;
    agent.running.lock().await.insert(71, newer);
    {
        let mut allocation = agent.allocation.lock().await;
        allocation
            .allocate_for_job(71, 2, 1, 256, &[])
            .expect("allocate newer attempt resources");
        assert!(allocation.commit_job(71, 2));
    }

    agent.drop_tracked_job(71, 2).await;

    assert!(agent.running.lock().await.is_empty());
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 0);
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn unreadable_cleanup_identity_keeps_an_allocation_reserved() {
    let controller = ControllerFixture::new(true);
    let fixture = SessionFixture::new(0);
    let agent = fixture.agent(&controller);
    fixture.track_allocation(&agent).await;
    std::fs::write(
        fixture
            .store
            .root()
            .join("cleanup")
            .join(format!("71.1.{}.json", spur_core::step::STEP_BATCH)),
        "unreadable identity",
    )
    .expect("corrupt cleanup record");

    agent.drop_tracked_job(71, 1).await;

    assert!(agent.running.lock().await.contains_key(&71));
    assert_eq!(agent.allocation.lock().await.allocated_memory_mb, 128);
    assert!(fixture.store.ensure_cleanup_ready().is_err());
}

#[tokio::test]
async fn an_expired_live_supervisor_is_quarantined_once_without_completing_its_job() {
    let controller = ControllerFixture::new(false);
    let mut fixture = SessionFixture::new(0);
    fixture.descriptor.pid = std::process::id();
    fixture.descriptor.process_start_ticks =
        crate::stepd::process_start_ticks(fixture.descriptor.pid).expect("fixture start ticks");
    fixture
        .store
        .publish(&fixture.descriptor)
        .expect("live descriptor");
    assert!(fixture
        .store
        .discover_unacknowledged_completions()
        .unwrap()
        .is_empty());

    quarantine_expired_cleanup(&fixture.store, &controller.address, "cleanup-node", "")
        .await
        .unwrap();
    assert!(!fixture.store.cleanup_store().records().unwrap()[0].quarantine_acknowledged);
    controller.state.accept.store(true, Ordering::SeqCst);
    quarantine_expired_cleanup(&fixture.store, &controller.address, "cleanup-node", "")
        .await
        .unwrap();
    assert!(!fixture.acknowledged());
    assert!(fixture.store.ensure_cleanup_ready().is_err());
    for report in controller.reports() {
        assert_eq!(report.job_id, 0);
        assert!(report.drain_node);
        assert_eq!(report.step_id, None);
        assert_eq!(report.pty_step_id, None);
    }
    let restarted = StepdStore::new(fixture.directory.path());
    assert!(restarted.cleanup_store().records().unwrap()[0].quarantine_acknowledged);
    quarantine_expired_cleanup(&restarted, &controller.address, "cleanup-node", "")
        .await
        .unwrap();
    assert_eq!(controller.reports().len(), 2);

    assert!(report_runtime_completion(&restarted, &controller.address, fixture.report()).await);
    assert!(fixture.acknowledged());
    let reports = controller.reports();
    assert_eq!(reports.len(), 3);
    assert_eq!(reports[2].job_id, fixture.descriptor.job_id);
    assert!(!reports[2].drain_node);
}

#[tokio::test]
async fn expired_orphan_cleanup_completes_only_after_quarantine_and_does_not_repeat() {
    for missing_session in [false, true] {
        let controller = ControllerFixture::new(true);
        let fixture = SessionFixture::new(0);
        let session = fixture.descriptor.socket_path.parent().unwrap();
        if missing_session {
            std::fs::remove_dir_all(session).unwrap();
        } else {
            std::fs::write(session.join("descriptor.json"), "broken").unwrap();
        }
        quarantine_expired_cleanup(&fixture.store, &controller.address, "cleanup-node", "")
            .await
            .unwrap();
        let reports = controller.reports();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].job_id, 0);
        assert!(reports[0].drain_node);
        assert_eq!(reports[1].job_id, fixture.descriptor.job_id);
        assert!(!reports[1].drain_node);
        assert!(fixture.acknowledged());
        assert!(fixture.store.ensure_cleanup_ready().is_err());
        quarantine_expired_cleanup(&fixture.store, &controller.address, "cleanup-node", "")
            .await
            .unwrap();
        assert_eq!(controller.reports().len(), 2);
    }
}
