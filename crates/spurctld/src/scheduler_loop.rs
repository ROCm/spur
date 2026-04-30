use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use prost_types;
use tracing::{debug, error, info, warn};

use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    AgentCancelJobRequest, JobSpec as ProtoJobSpec, LaunchJobRequest,
    ResourceSet as ProtoResourceSet, SubmitJobRequest,
};
use spur_sched::backfill::{self, BackfillScheduler};
use spur_sched::traits::{ClusterState, Scheduler};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

/// Spawn the time-limit enforcement watchdog and power manager alongside the scheduler loop.
pub async fn run(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    let enforcer_cluster = cluster.clone();
    let enforcer_raft = raft.clone();
    tokio::spawn(async move {
        enforce_time_limits(enforcer_cluster, enforcer_raft).await;
    });
    let power_cluster = cluster.clone();
    let power_raft = raft.clone();
    tokio::spawn(async move {
        manage_power(power_cluster, power_raft).await;
    });
    let interval_secs = cluster.config.scheduler.interval_secs.max(1) as u64;
    let max_jobs = cluster.config.scheduler.max_jobs_per_cycle as usize;

    let mut scheduler = BackfillScheduler::new(max_jobs);

    // Build topology tree from config (if configured)
    let topology = cluster.config.topology.as_ref().and_then(|topo_config| {
        use spur_core::topology::TopologyTree;
        match topo_config.plugin.as_str() {
            "tree" => {
                let tree = TopologyTree::from_switches(&topo_config.switches);
                info!(
                    switches = tree.switches.len(),
                    nodes = tree.node_switch.len(),
                    "topology/tree loaded"
                );
                Some(tree)
            }
            "block" => {
                let block_size = topo_config.block_size.unwrap_or(18);
                let all_nodes = cluster.get_nodes();
                let node_names: Vec<String> = all_nodes.iter().map(|n| n.name.clone()).collect();
                let tree = TopologyTree::from_blocks(&node_names, block_size);
                info!(
                    blocks = tree.switches.len(),
                    block_size, "topology/block loaded"
                );
                Some(tree)
            }
            _ => None,
        }
    });

    info!(
        interval_secs,
        max_jobs,
        plugin = scheduler.name(),
        topology = topology.is_some(),
        "scheduler loop started"
    );

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;

        if !raft.is_leader() {
            continue;
        }

        let pending = cluster.pending_jobs();
        if pending.is_empty() {
            continue;
        }

        let nodes = cluster.get_nodes();
        let partitions = cluster.get_partitions();
        let reservations = cluster.get_reservations();

        if nodes.is_empty() {
            debug!("no nodes registered, skipping scheduling cycle");
            continue;
        }

        let cluster_state = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &reservations,
            topology: topology.as_ref(),
        };

        // Catch panics in the scheduler so that a single bad job doesn't kill
        // the entire scheduling loop (issue #56).
        let sched_ref = &mut scheduler;
        let assignments = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sched_ref.schedule(&pending, &cluster_state)
        })) {
            Ok(a) => a,
            Err(e) => {
                error!(
                    "scheduler panicked: {:?} — skipping cycle",
                    e.downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| e.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown")
                );
                continue;
            }
        };

        // Preemption: if high-priority jobs couldn't be scheduled,
        // cancel lower-priority running jobs to free resources.
        if assignments.len() < pending.len() {
            let unscheduled: Vec<_> = pending
                .iter()
                .filter(|p| !assignments.iter().any(|a| a.job_id == p.job_id))
                .collect();

            if !unscheduled.is_empty() {
                // Update pending_reason for unscheduled jobs to reflect actual cause.
                // This helps users distinguish "waiting for higher-priority jobs" vs
                // "no suitable nodes at all".
                cluster.update_pending_reasons(&unscheduled, &cluster_state);

                try_preempt(&cluster, &unscheduled);

                // Federation: forward still-unschedulable jobs to peer clusters.
                if !cluster.config.federation.clusters.is_empty() {
                    let jobs_to_fwd: Vec<spur_core::job::Job> =
                        unscheduled.iter().map(|j| (*j).clone()).collect();
                    let fed_cluster = cluster.clone();
                    tokio::spawn(async move {
                        forward_to_federation(&fed_cluster, &jobs_to_fwd).await;
                    });
                }
            }
        }

        for assignment in assignments {
            let job = match cluster.get_job(assignment.job_id) {
                Some(j) => j,
                None => continue,
            };

            let per_node = backfill::job_resource_request(&job);
            let resources = compute_job_allocation(&job, &assignment.nodes, |name| {
                cluster.get_node(name).map(|n| n.total_resources.clone())
            });

            // Transition job to Running
            if let Err(e) =
                cluster.start_job(assignment.job_id, assignment.nodes.clone(), resources)
            {
                debug!(
                    job_id = assignment.job_id,
                    error = %e,
                    "failed to start job"
                );
                continue;
            }

            // Dispatch job to ALL assigned nodes
            let job_id = assignment.job_id;
            let spec = job.spec.clone();
            let all_nodes = assignment.nodes.clone();
            let per_node_alloc = per_node.clone();

            // Build peer_nodes list with addresses for cross-node communication
            let peer_addrs: Vec<String> = all_nodes
                .iter()
                .filter_map(|name| {
                    cluster
                        .get_node(name)
                        .and_then(|n| n.address.as_ref().map(|a| format!("{}:{}", a, n.port)))
                })
                .collect();

            let tasks_per_node = if let Some(tpn) = spec.tasks_per_node {
                tpn
            } else {
                (spec.num_tasks / spec.num_nodes.max(1)).max(1)
            };

            // Collect dispatch tasks to track success/failure
            let cluster_ref = cluster.clone();
            let dispatch_nodes = all_nodes.clone();
            tokio::spawn(async move {
                let mut successes = 0u32;
                let mut failures = 0u32;
                let total = dispatch_nodes.len() as u32;

                let mut set = tokio::task::JoinSet::new();
                for (node_idx, node_name) in dispatch_nodes.iter().enumerate() {
                    let node_info = cluster_ref.get_node(node_name);
                    let (addr, port) = match node_info {
                        Some(ref n) if n.address.is_some() => (n.address.clone().unwrap(), n.port),
                        _ => {
                            warn!(
                                job_id,
                                node = %node_name,
                                "no agent address for node, skipping dispatch"
                            );
                            failures += 1;
                            continue;
                        }
                    };

                    let agent_addr = format!("http://{}:{}", addr, port);
                    let spec = spec.clone();
                    let peer_addrs = peer_addrs.clone();
                    let task_offset = node_idx as u32 * tasks_per_node;
                    let target_node = node_name.clone();
                    let allocated = per_node_alloc.clone();
                    set.spawn(async move {
                        dispatch_to_agent(
                            &agent_addr,
                            job_id,
                            &spec,
                            &peer_addrs,
                            task_offset,
                            &target_node,
                            &allocated,
                        )
                        .await
                    });
                }

                while let Some(result) = set.join_next().await {
                    match result {
                        Ok(Ok(())) => successes += 1,
                        Ok(Err(e)) => {
                            error!(job_id, error = %e, "dispatch to agent failed");
                            failures += 1;
                        }
                        Err(e) => {
                            error!(job_id, error = %e, "dispatch task panicked");
                            failures += 1;
                        }
                    }
                }

                // If ALL dispatches failed, requeue the job back to Pending
                // so the scheduler can retry (e.g., container image may be
                // imported later, or a transient agent error may resolve).
                // Issue #91: previously marked as Failed immediately, which
                // didn't give users a chance to fix the problem.
                if successes == 0 && total > 0 {
                    error!(
                        job_id,
                        failures, "all dispatches failed — requeueing job to Pending"
                    );
                    if let Err(e) = cluster_ref.requeue_job(job_id) {
                        error!(job_id, error = %e, "failed to requeue job after dispatch failure");
                    }
                } else if failures > 0 {
                    warn!(
                        job_id,
                        successes,
                        failures,
                        "partial dispatch failure — job continues on successful nodes"
                    );
                }
            });
        }
    }
}

/// Compute the resource set to record against the cluster for an assignment.
///
/// Non-exclusive: per-node request × node count (cpus, memory, generic),
/// plus the per-job GPU list verbatim.
///
/// Exclusive (#147): cpus / gpus / generic gres are bumped to the **sum of
/// each assigned node's total resources**, so the node shows as fully
/// allocated and the backfill scheduler's CPU-saturation check fires for
/// subsequent jobs. Memory stays at requested (matches Slurm semantics).
///
/// `node_totals` returns the total resources for a node by name. Returns
/// `None` if the node has been deregistered between assignment and start;
/// in that case its contribution is silently zero.
pub(crate) fn compute_job_allocation<F>(
    job: &spur_core::job::Job,
    assignment_nodes: &[String],
    node_totals: F,
) -> spur_core::resource::ResourceSet
where
    F: Fn(&str) -> Option<spur_core::resource::ResourceSet>,
{
    use spur_core::resource::{GpuResource, ResourceSet};
    use std::collections::HashMap;

    let per_node = backfill::job_resource_request(job);
    let node_count = assignment_nodes.len() as u32;

    if job.spec.exclusive {
        let mut cpus: u32 = 0;
        let mut gpus: Vec<GpuResource> = Vec::new();
        let mut generic: HashMap<String, u64> = HashMap::new();
        for name in assignment_nodes {
            if let Some(total) = node_totals(name) {
                cpus = cpus.saturating_add(total.cpus);
                gpus.extend(total.gpus.iter().cloned());
                for (k, v) in &total.generic {
                    *generic.entry(k.clone()).or_insert(0) += v;
                }
            }
        }
        ResourceSet {
            cpus,
            memory_mb: per_node.memory_mb * node_count as u64,
            gpus,
            generic,
        }
    } else {
        ResourceSet {
            cpus: per_node.cpus * node_count,
            memory_mb: per_node.memory_mb * node_count as u64,
            gpus: per_node.gpus.clone(),
            generic: per_node
                .generic
                .iter()
                .map(|(k, v)| (k.clone(), v * node_count as u64))
                .collect(),
        }
    }
}

/// Try to preempt lower-priority running jobs to make room for higher-priority pending jobs.
fn try_preempt(cluster: &Arc<ClusterManager>, unscheduled: &[&spur_core::job::Job]) {
    use spur_core::job::JobState;

    // Get running jobs sorted by priority (lowest first = best preemption candidates)
    let mut running: Vec<spur_core::job::Job> = cluster
        .get_jobs(&[JobState::Running], None, None, None, &[])
        .into_iter()
        .collect();
    running.sort_by_key(|j| j.priority);

    for pending in unscheduled {
        // Only preempt if pending job has significantly higher priority
        for candidate in &running {
            if candidate.priority < pending.priority / 2 {
                // Preempt: cancel the lower-priority job
                info!(
                    preempted_job = candidate.job_id,
                    preempted_priority = candidate.priority,
                    pending_job = pending.job_id,
                    pending_priority = pending.priority,
                    "preempting lower-priority job"
                );
                if let Err(e) = cluster.complete_job(candidate.job_id, -1, JobState::Preempted) {
                    warn!(
                        job_id = candidate.job_id,
                        error = %e,
                        "failed to preempt job"
                    );
                }
                break; // One preemption per cycle, re-evaluate next cycle
            }
        }
    }
}

/// Forward unschedulable jobs to federation peer clusters.
///
/// Tries each peer in order; stops forwarding a job as soon as one peer accepts it.
/// Failed peer connections are logged as warnings and skipped.
async fn forward_to_federation(cluster: &ClusterManager, jobs: &[spur_core::job::Job]) {
    let peers = &cluster.config.federation.clusters;
    for job in jobs {
        for peer in peers {
            match SlurmControllerClient::connect(peer.address.clone()).await {
                Ok(mut client) => {
                    let req = SubmitJobRequest {
                        spec: Some(core_spec_to_proto(&job.spec)),
                    };
                    match client.submit_job(req).await {
                        Ok(resp) => {
                            let remote_id = resp.into_inner().job_id;
                            info!(
                                job_id = job.job_id,
                                peer = %peer.name,
                                remote_id,
                                "forwarded unschedulable job to federation peer"
                            );
                            break; // accepted by this peer — don't try others
                        }
                        Err(e) => {
                            warn!(
                                job_id = job.job_id,
                                peer = %peer.name,
                                error = %e,
                                "federation peer rejected job"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        peer = %peer.name,
                        error = %e,
                        "could not connect to federation peer"
                    );
                }
            }
        }
    }
}

/// Convert a core JobSpec to its proto representation for cross-cluster forwarding.
fn core_spec_to_proto(s: &spur_core::job::JobSpec) -> ProtoJobSpec {
    // Split licenses back out of GRES (stored as "license:<entry>")
    let mut gres = Vec::new();
    let mut licenses = Vec::new();
    for g in &s.gres {
        if let Some(lic) = g.strip_prefix("license:") {
            licenses.push(lic.to_string());
        } else {
            gres.push(g.clone());
        }
    }

    ProtoJobSpec {
        name: s.name.clone(),
        partition: s.partition.clone().unwrap_or_default(),
        account: s.account.clone().unwrap_or_default(),
        user: s.user.clone(),
        uid: s.uid,
        gid: s.gid,
        num_nodes: s.num_nodes,
        num_tasks: s.num_tasks,
        tasks_per_node: s.tasks_per_node.unwrap_or(0),
        cpus_per_task: s.cpus_per_task,
        memory_per_node_mb: s.memory_per_node_mb.unwrap_or(0),
        memory_per_cpu_mb: s.memory_per_cpu_mb.unwrap_or(0),
        gres,
        licenses,
        script: s.script.clone().unwrap_or_default(),
        argv: s.argv.clone(),
        work_dir: s.work_dir.clone(),
        stdout_path: s.stdout_path.clone().unwrap_or_default(),
        stderr_path: s.stderr_path.clone().unwrap_or_default(),
        environment: s.environment.clone(),
        time_limit: s.time_limit.map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
        time_min: s.time_min.map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
        qos: s.qos.clone().unwrap_or_default(),
        priority: s.priority.unwrap_or(0),
        reservation: s.reservation.clone().unwrap_or_default(),
        dependency: s.dependency.clone(),
        nodelist: s.nodelist.clone().unwrap_or_default(),
        exclude: s.exclude.clone().unwrap_or_default(),
        constraint: s.constraint.clone().unwrap_or_default(),
        mpi: s.mpi.clone().unwrap_or_default(),
        distribution: s.distribution.clone().unwrap_or_default(),
        het_group: s.het_group.unwrap_or(0),
        array_spec: s.array_spec.clone().unwrap_or_default(),
        requeue: s.requeue,
        exclusive: s.exclusive,
        hold: s.hold,
        interactive: s.interactive,
        mail_type: s.mail_type.clone(),
        mail_user: s.mail_user.clone().unwrap_or_default(),
        comment: s.comment.clone().unwrap_or_default(),
        wckey: s.wckey.clone().unwrap_or_default(),
        container_image: s.container_image.clone().unwrap_or_default(),
        container_mounts: s.container_mounts.clone(),
        container_workdir: s.container_workdir.clone().unwrap_or_default(),
        container_name: s.container_name.clone().unwrap_or_default(),
        container_readonly: s.container_readonly,
        container_mount_home: s.container_mount_home,
        container_env: s.container_env.clone(),
        container_entrypoint: s.container_entrypoint.clone().unwrap_or_default(),
        container_remap_root: s.container_remap_root,
        burst_buffer: s.burst_buffer.clone().unwrap_or_default(),
        begin_time: s.begin_time.map(|dt| prost_types::Timestamp {
            seconds: dt.timestamp(),
            nanos: 0,
        }),
        deadline: s.deadline.map(|dt| prost_types::Timestamp {
            seconds: dt.timestamp(),
            nanos: 0,
        }),
        spread_job: s.spread_job,
        topology: s.topology.clone().unwrap_or_default(),
        host_network: s.host_network,
        privileged: s.privileged,
        host_ipc: s.host_ipc,
        shm_size: s.shm_size.clone().unwrap_or_default(),
        extra_resources: s.extra_resources.clone(),
        open_mode: s.open_mode.clone().unwrap_or_default(),
    }
}

/// Send a LaunchJob RPC to a node agent.
async fn dispatch_to_agent(
    agent_addr: &str,
    job_id: u32,
    spec: &spur_core::job::JobSpec,
    peer_nodes: &[String],
    task_offset: u32,
    target_node: &str,
    allocated: &spur_core::resource::ResourceSet,
) -> anyhow::Result<()> {
    let mut client = SlurmAgentClient::connect(agent_addr.to_string()).await?;

    let proto_spec = ProtoJobSpec {
        name: spec.name.clone(),
        partition: spec.partition.clone().unwrap_or_default(),
        account: spec.account.clone().unwrap_or_default(),
        user: spec.user.clone(),
        uid: spec.uid,
        gid: spec.gid,
        num_nodes: spec.num_nodes,
        num_tasks: spec.num_tasks,
        tasks_per_node: spec.tasks_per_node.unwrap_or(0),
        cpus_per_task: spec.cpus_per_task,
        memory_per_node_mb: spec.memory_per_node_mb.unwrap_or(0),
        memory_per_cpu_mb: spec.memory_per_cpu_mb.unwrap_or(0),
        gres: spec.gres.clone(),
        script: spec.script.clone().unwrap_or_default(),
        argv: spec.argv.clone(),
        work_dir: spec.work_dir.clone(),
        stdout_path: spec.stdout_path.clone().unwrap_or_default(),
        stderr_path: spec.stderr_path.clone().unwrap_or_default(),
        environment: spec.environment.clone(),
        time_limit: spec.time_limit.map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
        time_min: None,
        qos: spec.qos.clone().unwrap_or_default(),
        priority: spec.priority.unwrap_or(0),
        reservation: spec.reservation.clone().unwrap_or_default(),
        dependency: spec.dependency.clone(),
        nodelist: spec.nodelist.clone().unwrap_or_default(),
        exclude: spec.exclude.clone().unwrap_or_default(),
        constraint: spec.constraint.clone().unwrap_or_default(),
        mpi: spec.mpi.clone().unwrap_or_default(),
        distribution: spec.distribution.clone().unwrap_or_default(),
        het_group: spec.het_group.unwrap_or(0),
        array_spec: spec.array_spec.clone().unwrap_or_default(),
        requeue: spec.requeue,
        exclusive: spec.exclusive,
        hold: spec.hold,
        interactive: spec.interactive,
        comment: spec.comment.clone().unwrap_or_default(),
        wckey: spec.wckey.clone().unwrap_or_default(),
        container_image: spec.container_image.clone().unwrap_or_default(),
        container_mounts: spec.container_mounts.clone(),
        container_workdir: spec.container_workdir.clone().unwrap_or_default(),
        container_name: spec.container_name.clone().unwrap_or_default(),
        container_readonly: spec.container_readonly,
        container_mount_home: spec.container_mount_home,
        container_env: spec.container_env.clone(),
        container_entrypoint: spec.container_entrypoint.clone().unwrap_or_default(),
        container_remap_root: spec.container_remap_root,
        burst_buffer: spec.burst_buffer.clone().unwrap_or_default(),
        licenses: Vec::new(),
        mail_type: spec.mail_type.clone(),
        mail_user: spec.mail_user.clone().unwrap_or_default(),
        begin_time: spec.begin_time.map(|dt| prost_types::Timestamp {
            seconds: dt.timestamp(),
            nanos: dt.timestamp_subsec_nanos() as i32,
        }),
        deadline: spec.deadline.map(|dt| prost_types::Timestamp {
            seconds: dt.timestamp(),
            nanos: dt.timestamp_subsec_nanos() as i32,
        }),
        spread_job: spec.spread_job,
        topology: spec.topology.clone().unwrap_or_default(),
        host_network: spec.host_network,
        privileged: spec.privileged,
        host_ipc: spec.host_ipc,
        shm_size: spec.shm_size.clone().unwrap_or_default(),
        extra_resources: spec.extra_resources.clone(),
        open_mode: spec.open_mode.clone().unwrap_or_default(),
    };

    let response = client
        .launch_job(LaunchJobRequest {
            job_id,
            spec: Some(proto_spec),
            allocated: Some(core_resource_to_proto(allocated)),
            peer_nodes: peer_nodes.to_vec(),
            task_offset,
            target_node: target_node.to_string(),
        })
        .await?;

    let inner = response.into_inner();
    if inner.success {
        info!(job_id, "job dispatched to agent successfully");
    } else {
        anyhow::bail!("agent rejected job: {}", inner.error);
    }

    Ok(())
}

/// Convert a core ResourceSet to proto ResourceSet.
fn core_resource_to_proto(r: &spur_core::resource::ResourceSet) -> ProtoResourceSet {
    use spur_core::resource::GpuLinkType;
    ProtoResourceSet {
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
                    GpuLinkType::XGMI => spur_proto::proto::GpuLinkType::GpuLinkXgmi as i32,
                    GpuLinkType::NVLink => spur_proto::proto::GpuLinkType::GpuLinkNvlink as i32,
                    GpuLinkType::PCIe => spur_proto::proto::GpuLinkType::GpuLinkPcie as i32,
                },
            })
            .collect(),
        generic: r.generic.clone(),
    }
}

/// Watchdog: gracefully terminate running jobs that exceed their time limit.
///
/// Two-phase shutdown:
///   1. **Warning phase**: When `start_time + time_limit < now`, send SIGTERM
///      (signal 15) to all agents and record the job as "warned".
///   2. **Kill phase**: 30 seconds after warning, if the job is still running,
///      mark it as Timeout and send SIGKILL (signal 9).
///
/// Runs every 10 seconds.
async fn enforce_time_limits(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    const GRACE_PERIOD_SECS: i64 = 30;

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
    let mut warned_jobs: HashSet<spur_core::job::JobId> = HashSet::new();
    let mut warn_times: std::collections::HashMap<spur_core::job::JobId, chrono::DateTime<Utc>> =
        std::collections::HashMap::new();

    loop {
        interval.tick().await;

        if !raft.is_leader() {
            continue;
        }

        let now = Utc::now();

        // Deadline enforcement: cancel pending jobs whose deadline has passed
        {
            let pending =
                cluster.get_jobs(&[spur_core::job::JobState::Pending], None, None, None, &[]);
            for job in &pending {
                if let Some(deadline) = job.spec.deadline {
                    if now > deadline {
                        info!(
                            job_id = job.job_id,
                            "job deadline passed while still pending — cancelling"
                        );
                        if let Err(e) = cluster.cancel_job(job.job_id, "system") {
                            warn!(job_id = job.job_id, error = %e, "failed to cancel deadline-expired job");
                        }
                    }
                }
            }
        }

        let running = cluster.get_jobs(&[spur_core::job::JobState::Running], None, None, None, &[]);

        for job in &running {
            let (Some(time_limit), Some(start_time)) = (job.spec.time_limit, job.start_time) else {
                continue;
            };
            let deadline = start_time + time_limit;
            if now < deadline {
                continue;
            }

            if warned_jobs.contains(&job.job_id) {
                // Already warned — check if grace period has elapsed
                let warn_time = warn_times
                    .get(&job.job_id)
                    .copied()
                    .unwrap_or(now - chrono::Duration::seconds(GRACE_PERIOD_SECS + 1));
                if (now - warn_time).num_seconds() < GRACE_PERIOD_SECS {
                    continue; // Still in grace period
                }

                // Grace period expired — force kill
                info!(
                    job_id = job.job_id,
                    "grace period expired — force-killing job"
                );

                if let Err(e) =
                    cluster.complete_job(job.job_id, -1, spur_core::job::JobState::Timeout)
                {
                    warn!(job_id = job.job_id, error = %e, "failed to mark job as timed out");
                    continue;
                }

                send_cancel_to_agents(&cluster, job, 9).await; // SIGKILL
                warned_jobs.remove(&job.job_id);
                warn_times.remove(&job.job_id);
            } else {
                // First time past deadline — send SIGTERM (graceful warning)
                info!(
                    job_id = job.job_id,
                    elapsed_secs = (now - start_time).num_seconds(),
                    limit_secs = time_limit.num_seconds(),
                    grace_secs = GRACE_PERIOD_SECS,
                    "time limit exceeded — sending SIGTERM, grace period starts"
                );

                send_cancel_to_agents(&cluster, job, 15).await; // SIGTERM
                warned_jobs.insert(job.job_id);
                warn_times.insert(job.job_id, now);
            }
        }

        // Clean up warned_jobs for jobs that are no longer running
        // (e.g., they exited cleanly during grace period)
        let running_ids: HashSet<_> = running.iter().map(|j| j.job_id).collect();
        warned_jobs.retain(|id| running_ids.contains(id));
        warn_times.retain(|id, _| running_ids.contains(id));
    }
}

/// Power management: suspend idle nodes and resume them when jobs are pending.
///
/// Disabled when `power.suspend_timeout_secs` is not set in the config.
async fn manage_power(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    let suspend_timeout = match cluster.config.power.suspend_timeout_secs {
        Some(t) => t,
        None => return,
    };

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    info!(suspend_timeout, "power management enabled");

    loop {
        interval.tick().await;

        if !raft.is_leader() {
            continue;
        }

        let now = Utc::now();
        let nodes = cluster.get_nodes();

        // Suspend idle nodes that have been idle longer than the timeout
        for node in &nodes {
            if node.state == spur_core::node::NodeState::Idle {
                if let Some(last_busy) = node.last_busy {
                    if (now - last_busy).num_seconds() as u64 > suspend_timeout {
                        info!(node = %node.name, "suspending idle node (power saving)");
                        let _ = cluster.update_node_state(
                            &node.name,
                            spur_core::node::NodeState::Suspended,
                            Some("Power saving".into()),
                        );
                        if let Some(ref cmd) = cluster.config.power.suspend_command {
                            let cmd = cmd.replace("{node}", &node.name);
                            let node_name = node.name.clone();
                            tokio::spawn(async move {
                                if let Err(e) = tokio::process::Command::new("sh")
                                    .args(["-c", &cmd])
                                    .status()
                                    .await
                                {
                                    warn!(
                                        node = %node_name,
                                        error = %e,
                                        "suspend command failed"
                                    );
                                }
                            });
                        }
                    }
                }
            }
        }

        // Resume suspended nodes if there are pending jobs
        let pending = cluster.pending_jobs();
        if !pending.is_empty() {
            for node in &nodes {
                if node.state == spur_core::node::NodeState::Suspended {
                    info!(node = %node.name, "resuming suspended node for pending jobs");
                    let _ = cluster.update_node_state(
                        &node.name,
                        spur_core::node::NodeState::Idle,
                        None,
                    );
                    if let Some(ref cmd) = cluster.config.power.resume_command {
                        let cmd = cmd.replace("{node}", &node.name);
                        let node_name = node.name.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tokio::process::Command::new("sh")
                                .args(["-c", &cmd])
                                .status()
                                .await
                            {
                                warn!(
                                    node = %node_name,
                                    error = %e,
                                    "resume command failed"
                                );
                            }
                        });
                    }
                }
            }
        }
    }
}

/// Send CancelJob RPC to all agents for a job with a specific signal.
async fn send_cancel_to_agents(
    cluster: &Arc<ClusterManager>,
    job: &spur_core::job::Job,
    signal: i32,
) {
    for node_name in &job.allocated_nodes {
        let node_info = cluster.get_node(node_name);
        let (addr, port) = match node_info {
            Some(ref n) if n.address.is_some() => (n.address.clone().unwrap(), n.port),
            _ => {
                warn!(
                    job_id = job.job_id,
                    node = %node_name,
                    "no agent address — cannot cancel job on node"
                );
                continue;
            }
        };

        let agent_addr = format!("http://{}:{}", addr, port);
        let job_id = job.job_id;
        tokio::spawn(async move {
            match SlurmAgentClient::connect(agent_addr.clone()).await {
                Ok(mut client) => {
                    if let Err(e) = client
                        .cancel_job(AgentCancelJobRequest { job_id, signal })
                        .await
                    {
                        warn!(
                            job_id,
                            signal,
                            agent = %agent_addr,
                            error = %e,
                            "CancelJob RPC failed"
                        );
                    } else {
                        info!(job_id, signal, agent = %agent_addr, "sent CancelJob");
                    }
                }
                Err(e) => {
                    warn!(
                        job_id,
                        agent = %agent_addr,
                        error = %e,
                        "failed to connect to agent for cancel"
                    );
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::{Job, JobSpec, JobState};
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
    use std::collections::HashMap;

    fn job_with_spec(mut spec: JobSpec) -> Job {
        spec.cpus_per_task = spec.cpus_per_task.max(1);
        spec.num_tasks = spec.num_tasks.max(1);
        spec.num_nodes = spec.num_nodes.max(1);
        Job::new(1, spec)
    }

    fn node_total(cpus: u32, memory_mb: u64, gpus: Vec<GpuResource>) -> ResourceSet {
        ResourceSet {
            cpus,
            memory_mb,
            gpus,
            generic: HashMap::new(),
        }
    }

    fn gpu(device_id: u32, gpu_type: &str) -> GpuResource {
        GpuResource {
            device_id,
            gpu_type: gpu_type.into(),
            memory_mb: 192_000,
            peer_gpus: vec![],
            link_type: GpuLinkType::PCIe,
        }
    }

    // ── #147: --exclusive enforcement ─────────────────────────────
    //
    // Repro of the reported bug: an exclusive job that requests 1 CPU
    // would only record 1 CPU as allocated against the node. Backfill's
    // `alloc.cpus >= total.cpus` saturation check would never fire,
    // letting other jobs schedule onto the node. compute_job_allocation
    // must bump cpus / gpus / generic to the sum of node totals.

    #[test]
    fn exclusive_job_bumps_cpus_to_node_total() {
        let mut spec = JobSpec::default();
        spec.cpus_per_task = 2;
        spec.num_tasks = 1;
        spec.num_nodes = 1;
        spec.exclusive = true;
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |name| match name {
            "n1" => Some(node_total(64, 256_000, vec![])),
            _ => None,
        });

        assert_eq!(
            alloc.cpus, 64,
            "exclusive job must record full node CPU count, not requested"
        );
    }

    #[test]
    fn exclusive_job_bumps_cpus_across_multiple_nodes() {
        let mut spec = JobSpec::default();
        spec.cpus_per_task = 1;
        spec.num_nodes = 2;
        spec.exclusive = true;
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string(), "n2".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |name| match name {
            "n1" => Some(node_total(64, 256_000, vec![])),
            "n2" => Some(node_total(48, 128_000, vec![])),
            _ => None,
        });

        assert_eq!(alloc.cpus, 112, "exclusive job must sum CPUs across nodes");
    }

    #[test]
    fn exclusive_job_takes_all_gpus_from_each_node() {
        let mut spec = JobSpec::default();
        spec.exclusive = true;
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |name| match name {
            "n1" => Some(node_total(
                64,
                256_000,
                vec![gpu(0, "mi300x"), gpu(1, "mi300x")],
            )),
            _ => None,
        });

        assert_eq!(alloc.gpus.len(), 2, "exclusive job must take every GPU");
        assert_eq!(alloc.gpus[0].device_id, 0);
        assert_eq!(alloc.gpus[1].device_id, 1);
    }

    #[test]
    fn exclusive_job_keeps_memory_at_request_not_node_total() {
        // Slurm semantics: exclusive nodes give all CPUs/GRES but only
        // the requested memory.
        let mut spec = JobSpec::default();
        spec.cpus_per_task = 1;
        spec.exclusive = true;
        spec.memory_per_node_mb = Some(4096);
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |_| Some(node_total(64, 256_000, vec![])));

        assert_eq!(
            alloc.memory_mb, 4096,
            "exclusive memory must stay at request, not node total"
        );
    }

    #[test]
    fn exclusive_job_sums_generic_gres_from_each_node() {
        let mut spec = JobSpec::default();
        spec.exclusive = true;
        let job = job_with_spec(spec);

        let mut gen_a = HashMap::new();
        gen_a.insert("license:fluent".to_string(), 5u64);
        let total_a = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![],
            generic: gen_a,
        };

        let mut gen_b = HashMap::new();
        gen_b.insert("license:fluent".to_string(), 3u64);
        let total_b = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![],
            generic: gen_b,
        };

        let nodes = vec!["n1".to_string(), "n2".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |name| match name {
            "n1" => Some(total_a.clone()),
            "n2" => Some(total_b.clone()),
            _ => None,
        });

        assert_eq!(alloc.generic.get("license:fluent").copied(), Some(8));
    }

    #[test]
    fn non_exclusive_job_records_request_not_node_total() {
        // Regression guard: don't accidentally bump non-exclusive jobs.
        let mut spec = JobSpec::default();
        spec.cpus_per_task = 2;
        spec.num_tasks = 1;
        spec.num_nodes = 1;
        spec.exclusive = false;
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |_| Some(node_total(64, 256_000, vec![])));

        assert_eq!(
            alloc.cpus, 2,
            "non-exclusive job must record exactly what was requested"
        );
    }

    #[test]
    fn exclusive_job_handles_missing_node_metadata() {
        // If a node was deregistered between schedule and start, that
        // node's contribution is zero — don't panic.
        let mut spec = JobSpec::default();
        spec.exclusive = true;
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string(), "ghost".to_string()];
        let alloc = compute_job_allocation(&job, &nodes, |name| match name {
            "n1" => Some(node_total(64, 256_000, vec![])),
            _ => None,
        });

        assert_eq!(alloc.cpus, 64);
    }
}
