// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tracing::{debug, error, info, warn};

use spur_core::node::NodeSource;
use spur_core::partition::requested_partition_names;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    AgentCancelJobRequest, AgentSuspendJobRequest, JobSpec as ProtoJobSpec, LaunchJobRequest,
    RegisterJobAllocationRequest, SubmitJobRequest,
};
use spur_sched::backfill::{self, BackfillScheduler};
use spur_sched::traits::{ClusterState, Scheduler};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

/// Upper bound on a single CancelJob RPC (connect + call) when the caller
/// awaits delivery. Best-effort cleanup must not stall eviction on an
/// unreachable agent.
const CANCEL_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn the time-limit enforcement watchdog and power manager alongside the scheduler loop.
pub async fn run(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    let enforcer_cluster = cluster.clone();
    let enforcer_raft = raft.clone();
    tokio::spawn(async move {
        enforce_time_limits(enforcer_cluster, enforcer_raft).await;
    });
    let completing_cluster = cluster.clone();
    let completing_raft = raft.clone();
    tokio::spawn(async move {
        enforce_completing_timeout(completing_cluster, completing_raft).await;
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
        "scheduler loop started (event-driven wake enabled)"
    );

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
    let scheduler_notify = cluster.scheduler_notify.clone();

    loop {
        // Event-driven wake: sleep until EITHER a job is submitted OR the periodic tick fires.
        // This eliminates the up-to-`interval_secs` polling delay for new submissions while
        // preserving a periodic wake for resource-freed events and node state changes.
        tokio::select! {
            _ = scheduler_notify.notified() => {}
            _ = interval.tick() => {}
        }

        if !raft.is_leader() {
            continue;
        }

        // Finalize never-satisfiable deps before pending_jobs() so they drop
        // out of this cycle instead of sitting PENDING forever.
        cluster.cancel_unsatisfiable_dependency_jobs();

        // Drive before advance so capacity freed by completions is available to
        // newly-eligible jobs in the same cycle. Real agent-side data movement is
        // a follow-up; drive_bb_stage_in() is the controller-side seam only.
        cluster.drive_bb_stage_in();
        cluster.advance_bb_staging();
        cluster.purge_expired_reservations();
        cluster.enforce_reservation_end_times();

        // Tag jobs pending_jobs() will drop (QoS/license/reservation/BB) with
        // their real reason, since they never reach update_pending_reasons().
        // Runs after BB staging so BurstBufferStageIn reasons reflect the
        // up-to-date staging state set in this cycle. Before the empty-check so
        // reasons stay fresh even with nothing schedulable.
        cluster.tag_blocked_pending_reasons();

        let pending = cluster.pending_jobs();
        if pending.is_empty() {
            continue;
        }
        let hit_depth_limit = pending.len() > max_jobs;

        let nodes = cluster.get_nodes();
        let partitions = cluster.get_partitions();
        let reservations = cluster.get_reservations();

        if nodes.is_empty() {
            debug!("no nodes registered, skipping scheduling cycle");
            continue;
        }

        let cycle_start = Instant::now();

        let cluster_state = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &reservations,
            topology: topology.as_ref(),
        };

        // Catch panics in the scheduler so that a single bad job doesn't kill
        // the entire scheduling loop (issue #56).
        let sched_ref = &mut scheduler;
        let schedule_start = Instant::now();
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
                let cycle_time_us = cycle_start.elapsed().as_micros().min(u64::MAX as u128) as u64;
                let schedule_time_us =
                    schedule_start.elapsed().as_micros().min(u64::MAX as u128) as u64;
                cluster.record_sched_cycle(cycle_time_us, schedule_time_us, 0, hit_depth_limit);
                continue;
            }
        };
        let schedule_time_us = schedule_start.elapsed().as_micros().min(u64::MAX as u128) as u64;

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

                try_preempt(&cluster, &partitions, &unscheduled).await;

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

        let mut jobs_started_cycle = 0u64;
        for assignment in assignments {
            let job = match cluster.get_job(assignment.job_id) {
                Some(j) => j,
                None => continue,
            };

            let resources =
                compute_job_allocation(&job, &assignment.nodes, &assignment.per_node_alloc);

            let job_id = assignment.job_id;
            let spec = job.spec.clone();
            let all_nodes = assignment.nodes.clone();
            let per_node_allocs = assignment.per_node_alloc.clone();
            let dispatch_nodes = all_nodes.clone();
            let allocated_nodelist = all_nodes.join(",");

            let srun_step_dispatch = spec.srun_job
                && dispatch_nodes.iter().all(|name| {
                    cluster
                        .get_node(name)
                        .is_some_and(|n| n.source == NodeSource::NativeHost)
                });

            if spec.srun_job
                && !srun_step_dispatch
                && spec.script.as_deref().unwrap_or("").is_empty()
            {
                warn!(
                    job_id,
                    "srun batch fallback requires a script in the job spec"
                );
                if let Err(e) = cluster.requeue_job(job_id) {
                    error!(job_id, error = %e, "failed to requeue srun job without script");
                }
                continue;
            }

            if spec.srun_job && srun_step_dispatch {
                match register_allocation_on_nodes(
                    cluster.clone(),
                    job_id,
                    dispatch_nodes.clone(),
                    &spec,
                    per_node_allocs.clone(),
                    allocated_nodelist.clone(),
                )
                .await
                {
                    AllocationRegisterOutcome::AllFailed => {
                        if let Err(e) = cluster.requeue_job(job_id) {
                            error!(job_id, error = %e, "failed to requeue after registration failure");
                        }
                        continue;
                    }
                    AllocationRegisterOutcome::PartialFailed { succeeded_nodes } => {
                        cancel_job_on_nodes(&cluster, job_id, &succeeded_nodes, 9).await;
                        if let Err(e) = cluster.requeue_job(job_id) {
                            error!(job_id, error = %e, "failed to requeue after partial registration");
                        }
                        continue;
                    }
                    AllocationRegisterOutcome::AllSucceeded => {}
                }
            }

            // Transition job to Running
            let start_result = if srun_step_dispatch {
                cluster.start_job_impl(
                    job_id,
                    assignment.nodes.clone(),
                    resources,
                    assignment.per_node_alloc.clone(),
                    true,
                )
            } else {
                cluster.start_job(
                    job_id,
                    assignment.nodes.clone(),
                    resources,
                    assignment.per_node_alloc.clone(),
                )
            };
            let run_attempt = match start_result {
                Ok(attempt) => attempt,
                Err(e) => {
                    if spec.srun_job && srun_step_dispatch {
                        cancel_job_on_nodes(&cluster, job_id, &dispatch_nodes, 0).await;
                    }
                    debug!(
                        job_id = assignment.job_id,
                        error = %e,
                        "failed to start job"
                    );
                    continue;
                }
            };

            // Run PrologSlurmctld if configured
            if let Some(ref prolog_ctld) = cluster.config.hooks.prolog_slurmctld {
                let ctx = spur_core::hooks::HookContext {
                    job_id: assignment.job_id,
                    work_dir: job.spec.work_dir.clone(),
                    uid: job.spec.uid,
                    gid: job.spec.gid,
                    partition: job.spec.partition.clone().unwrap_or_default(),
                    nodelist: assignment.nodes.join(","),
                    script_context: "prolog_slurmctld".into(),
                    gpu_devices: Vec::new(),
                    cpus: job.spec.cpus_per_task,
                    memory_mb: job.spec.memory_per_node_mb.unwrap_or(0),
                };
                if let Err(e) = spur_core::hooks::run_hook(prolog_ctld, &ctx).await {
                    error!(
                        job_id = assignment.job_id,
                        error = %e,
                        "PrologSlurmctld failed"
                    );
                    if spec.srun_job && srun_step_dispatch && !job.spec.interactive {
                        cancel_job_on_nodes(&cluster, job_id, &dispatch_nodes, 0).await;
                    }
                    if job.spec.interactive {
                        if let Err(ce) = cluster.cancel_job(assignment.job_id, &job.spec.user) {
                            error!(job_id = assignment.job_id, error = %ce, "failed to cancel job after PrologSlurmctld failure");
                        }
                    } else {
                        if let Err(re) = cluster.requeue_job(assignment.job_id) {
                            error!(job_id = assignment.job_id, error = %re, "failed to requeue job after PrologSlurmctld failure");
                        }
                    }
                    continue;
                }
            }

            jobs_started_cycle += 1;

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

            let cluster_ref = cluster.clone();
            if spec.srun_job {
                if !srun_step_dispatch {
                    let mut batch_spec = spec;
                    batch_spec.srun_job = false;
                    tokio::spawn(dispatch_job_to_nodes(
                        cluster_ref,
                        job_id,
                        dispatch_nodes,
                        batch_spec,
                        peer_addrs,
                        per_node_allocs,
                        allocated_nodelist,
                        tasks_per_node,
                        run_attempt,
                    ));
                }
            } else {
                tokio::spawn(dispatch_job_to_nodes(
                    cluster_ref,
                    job_id,
                    dispatch_nodes,
                    spec,
                    peer_addrs,
                    per_node_allocs,
                    allocated_nodelist,
                    tasks_per_node,
                    run_attempt,
                ));
            }
        }

        let cycle_time_us = cycle_start.elapsed().as_micros().min(u64::MAX as u128) as u64;
        cluster.record_sched_cycle(
            cycle_time_us,
            schedule_time_us,
            jobs_started_cycle,
            hit_depth_limit,
        );
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
pub(crate) fn compute_job_allocation(
    job: &spur_core::job::Job,
    assignment_nodes: &[String],
    per_node_alloc: &std::collections::HashMap<String, spur_core::resource::ResourceAllocations>,
) -> spur_core::resource::ResourceAllocations {
    use spur_core::resource::{
        aggregate_allocations, build_exclusive_allocation, ResourceAllocations,
    };

    if job.spec.exclusive {
        let mut total = ResourceAllocations::default();
        let per_node_req = backfill::job_resource_request(job);
        for name in assignment_nodes {
            if let Some(alloc) = per_node_alloc.get(name) {
                total.add(alloc);
            }
        }
        if total.is_empty() {
            // Fallback if scheduler did not populate per-node slices.
            total = build_exclusive_allocation(
                &spur_core::resource::ResourceSet::default(),
                per_node_req.memory_mb,
            );
        }
        total
    } else {
        aggregate_allocations(
            assignment_nodes
                .iter()
                .filter_map(|name| per_node_alloc.get(name).cloned()),
        )
    }
}

/// Resolve the effective PreemptMode for a job: QoS override wins if set
/// (see `qos_preempt_override`), else the most aggressive matched partition.
fn job_preempt_mode(
    job: &spur_core::job::Job,
    partitions: &[spur_core::partition::Partition],
    qos: &spur_core::accounting::Qos,
) -> spur_core::partition::PreemptMode {
    use spur_core::partition::PreemptMode;

    if let Some(mode) = spur_core::qos::qos_preempt_override(qos) {
        return mode;
    }

    spur_core::partition::matched_partitions(job.spec.partition.as_deref(), partitions)
        .into_iter()
        .map(|p| p.preempt_mode)
        .max_by_key(|m| m.aggressiveness())
        .unwrap_or(PreemptMode::Off)
}

/// Preempt lower-priority running jobs per their partition PreemptMode
/// (Off jobs are never preempted).
pub(crate) async fn try_preempt(
    cluster: &Arc<ClusterManager>,
    partitions: &[spur_core::partition::Partition],
    unscheduled: &[&spur_core::job::Job],
) {
    use crate::cluster::PreemptOutcome;
    use spur_core::job::JobState;
    use spur_core::partition::{Partition, PreemptMode};
    use spur_core::reservation::job_runs_in_active_reservation;

    let now = chrono::Utc::now();
    let reservations = cluster.get_reservations();
    let cluster_nodes = cluster.get_nodes();

    let partition_for = |job: &spur_core::job::Job| -> Option<&Partition> {
        spur_core::partition::matched_partitions(job.spec.partition.as_deref(), partitions)
            .into_iter()
            .max_by_key(|p| p.preempt_mode.aggressiveness())
    };

    let mut running: Vec<spur_core::job::Job> = cluster
        .get_jobs(&[JobState::Running], None, None, None, None, &[])
        .into_iter()
        .collect();
    // Resolve once, reuse for both the priority recompute and the
    // preempt-mode decision below.
    let running_qos: std::collections::HashMap<spur_core::job::JobId, spur_core::accounting::Qos> =
        running
            .iter()
            .map(|j| (j.job_id, cluster.resolve_qos(j)))
            .collect();
    // Running jobs' stored `priority` is the raw base value, unlike
    // `pending`'s fully adjusted one; recompute a comparable value.
    let running_priority: std::collections::HashMap<spur_core::job::JobId, u32> = running
        .iter()
        .map(|j| {
            (
                j.job_id,
                cluster.current_effective_priority_with_qos(j, &running_qos[&j.job_id], partitions),
            )
        })
        .collect();
    running.sort_by_key(|j| running_priority[&j.job_id]);

    for pending in unscheduled {
        let Some(pending_part) = partition_for(pending) else {
            continue;
        };
        if pending_part.preempt_mode == PreemptMode::Off {
            continue;
        }
        let pending_tier = pending_part.priority_tier;

        for candidate in &running {
            let candidate_priority = running_priority[&candidate.job_id];
            if candidate_priority >= pending.priority / 2 {
                continue;
            }

            if !preempt_overlaps_pending_nodes(pending, candidate, &cluster_nodes) {
                continue;
            }

            if job_runs_in_active_reservation(candidate, &reservations, now) {
                let candidate_tier = partition_for(candidate)
                    .map(|p| p.priority_tier)
                    .unwrap_or(1);
                if pending_tier <= candidate_tier {
                    continue;
                }
            }

            let mode = job_preempt_mode(candidate, partitions, &running_qos[&candidate.job_id]);
            if mode == PreemptMode::Off {
                continue;
            }
            info!(
                preempted_job = candidate.job_id,
                preempted_priority = candidate_priority,
                pending_job = pending.job_id,
                pending_priority = pending.priority,
                mode = ?mode,
                "preempting lower-priority job"
            );
            match cluster.preempt_job(candidate.job_id, mode) {
                Ok(PreemptOutcome::Killed) => {
                    // Signal 0 = graceful cancel (SIGTERM then SIGKILL).
                    send_cancel_to_agents(cluster, candidate, 0).await;
                }
                Ok(PreemptOutcome::Suspended) => {
                    send_suspend_to_agents(cluster, candidate, false).await;
                }
                Err(e) => {
                    warn!(
                        job_id = candidate.job_id,
                        error = %e,
                        "failed to preempt job"
                    );
                    continue;
                }
            }
            break; // One preemption per cycle, re-evaluate next cycle
        }
    }
}

/// True when `candidate` occupies a node the pending job could target.
fn preempt_overlaps_pending_nodes(
    pending: &spur_core::job::Job,
    candidate: &spur_core::job::Job,
    nodes: &[spur_core::node::Node],
) -> bool {
    if candidate.allocated_nodes.is_empty() {
        return false;
    }
    let occupied: HashSet<&str> = candidate
        .allocated_nodes
        .iter()
        .map(String::as_str)
        .collect();

    if let Some(ref nodelist) = pending.spec.nodelist {
        return nodelist
            .split(',')
            .map(str::trim)
            .any(|n| occupied.contains(n));
    }

    let partitions: Vec<&str> =
        requested_partition_names(pending.spec.partition.as_deref()).collect();

    nodes.iter().any(|node| {
        if !occupied.contains(node.name.as_str()) {
            return false;
        }
        if partitions.is_empty() {
            return true;
        }
        partitions
            .iter()
            .any(|p| node.partitions.iter().any(|np| np == p))
    })
}

/// Forward unschedulable jobs to federation peer clusters.
///
/// Tries each peer in order; stops forwarding a job as soon as one peer accepts it.
/// Failed peer connections are logged as warnings and skipped.
async fn forward_to_federation(cluster: &ClusterManager, jobs: &[spur_core::job::Job]) {
    let peers = &cluster.config.federation.clusters;
    for job in jobs {
        for peer in peers {
            match SlurmControllerClient::connect(peer.address.clone())
                .await
                .map(|c| {
                    c.max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
                        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE)
                }) {
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
        gpus: s.gpus.as_ref().map(Into::into),
        gpus_per_node: s.gpus_per_node.as_ref().map(Into::into),
        gpus_per_task: s.gpus_per_task.as_ref().map(Into::into),
        licenses,
        script: s.script.clone().unwrap_or_default(),
        argv: s.argv.clone(),
        script_args: s.script_args.clone(),
        work_dir: s.work_dir.clone(),
        stdout_path: s.stdout_path.clone().unwrap_or_default(),
        stderr_path: s.stderr_path.clone().unwrap_or_default(),
        stdin_path: s.stdin_path.clone().unwrap_or_default(),
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
        srun_job: s.srun_job,
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
        pty: s.pty,
        initial_winsize: None,
    }
}

/// Parameters for dispatching a job to a single node agent.
struct AgentDispatchParams<'a> {
    job_id: u32,
    spec: &'a spur_core::job::JobSpec,
    peer_nodes: &'a [String],
    task_offset: u32,
    target_node: &'a str,
    allocated: &'a spur_core::resource::ResourceAllocations,
    allocated_nodelist: &'a str,
    run_attempt: u32,
}

/// Resolved output paths reported by an agent after a successful launch.
struct LaunchOutcome {
    stdout_path: String,
    stderr_path: String,
}

/// Send a LaunchJob RPC to a node agent.
async fn dispatch_to_agent(
    agent_addr: &str,
    params: &AgentDispatchParams<'_>,
) -> anyhow::Result<LaunchOutcome> {
    let mut client = SlurmAgentClient::connect(agent_addr.to_string())
        .await?
        .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

    let spec = params.spec;

    // The scheduler distributes GPUs per node (a --gpus total may be uneven
    // across nodes). Rewrite this node's `gres` to its concrete GPU count so the
    // agent's fallback and any display see the real per-node figure; the agent
    // itself binds the exact device IDs from `allocated`.
    let node_gpu_count = params.allocated.total_device_count("gpu") as u32;
    let gpu_type = spur_core::gpu_request::resolve_gpu_demand(spec)
        .ok()
        .and_then(|d| d.gpu_type().map(str::to_string));
    let mut per_node_gres: Vec<String> = spec
        .gres
        .iter()
        .filter(|g| !(g.starts_with("gpu:") || g.as_str() == "gpu"))
        .cloned()
        .collect();
    if node_gpu_count > 0 {
        per_node_gres.push(match &gpu_type {
            Some(t) => format!("gpu:{}:{}", t, node_gpu_count),
            None => format!("gpu:{}", node_gpu_count),
        });
    }

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
        gres: per_node_gres,
        // Per-node count is carried in `gres` above; the explicit GPU request
        // fields are controller-side scheduling inputs the agent does not use.
        gpus: None,
        gpus_per_node: None,
        gpus_per_task: None,
        script: spec.script.clone().unwrap_or_default(),
        argv: spec.argv.clone(),
        script_args: spec.script_args.clone(),
        work_dir: spec.work_dir.clone(),
        stdout_path: spec.stdout_path.clone().unwrap_or_default(),
        stderr_path: spec.stderr_path.clone().unwrap_or_default(),
        stdin_path: spec.stdin_path.clone().unwrap_or_default(),
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
        nodelist: params.allocated_nodelist.to_string(),
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
        srun_job: spec.srun_job,
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
        pty: spec.pty,
        initial_winsize: None,
    };

    let response = client
        .launch_job(LaunchJobRequest {
            job_id: params.job_id,
            spec: Some(proto_spec),
            allocated: Some(crate::server::allocations_to_proto(params.allocated)),
            peer_nodes: params.peer_nodes.to_vec(),
            task_offset: params.task_offset,
            target_node: params.target_node.to_string(),
            // Controller-assigned at array expansion; consumed agent-side.
            array_job_id: spec.array_job_id.unwrap_or(0),
            array_task_id: spec.array_task_id.unwrap_or(0),
            run_attempt: params.run_attempt,
        })
        .await?;

    let inner = response.into_inner();
    if inner.success {
        info!(
            job_id = params.job_id,
            "job dispatched to agent successfully"
        );
    } else {
        anyhow::bail!("agent rejected job: {}", inner.error);
    }

    Ok(LaunchOutcome {
        stdout_path: inner.stdout_path,
        stderr_path: inner.stderr_path,
    })
}

/// Outcome of parallel RegisterJobAllocation RPCs for a standalone srun job.
pub(crate) enum AllocationRegisterOutcome {
    AllSucceeded,
    AllFailed,
    PartialFailed { succeeded_nodes: Vec<String> },
}

/// Parameters for registering a srun-only allocation on a single node agent.
struct AllocationRegisterParams {
    job_id: u32,
    partition: String,
    uid: u32,
    gid: u32,
    mpi: String,
    allocated_nodelist: String,
    allocated: spur_core::resource::ResourceAllocations,
    work_dir: String,
}

/// Register a srun-only allocation on a node agent without launching a batch process.
async fn register_allocation_to_agent(
    agent_addr: &str,
    params: &AllocationRegisterParams,
) -> anyhow::Result<()> {
    let mut client = SlurmAgentClient::connect(agent_addr.to_string())
        .await?
        .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);

    client
        .register_job_allocation(RegisterJobAllocationRequest {
            job_id: params.job_id,
            partition: params.partition.clone(),
            nodelist: params.allocated_nodelist.clone(),
            uid: params.uid,
            gid: params.gid,
            cpus: params.allocated.cpus,
            memory_mb: params.allocated.memory_mb,
            gpu_devices: params
                .allocated
                .device_ids("gpu")
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
            allocated: Some(crate::server::allocations_to_proto(&params.allocated)),
            mpi: params.mpi.clone(),
            work_dir: params.work_dir.clone(),
        })
        .await?;

    info!(
        job_id = params.job_id,
        "srun allocation registered on agent successfully"
    );

    Ok(())
}

/// Register a srun-only allocation on every assigned node.
#[allow(clippy::too_many_arguments)]
async fn register_allocation_on_nodes(
    cluster: Arc<ClusterManager>,
    job_id: spur_core::job::JobId,
    dispatch_nodes: Vec<String>,
    spec: &spur_core::job::JobSpec,
    per_node_allocs: std::collections::HashMap<String, spur_core::resource::ResourceAllocations>,
    allocated_nodelist: String,
) -> AllocationRegisterOutcome {
    let mut successes = 0u32;
    let mut failures = 0u32;
    let mut succeeded_nodes: Vec<String> = Vec::new();
    let total = dispatch_nodes.len() as u32;

    let mut set = tokio::task::JoinSet::new();
    for node_name in &dispatch_nodes {
        let node_info = cluster.get_node(node_name);
        let (addr, port) = match node_info {
            Some(ref n) if n.address.is_some() => (n.address.clone().unwrap(), n.port),
            _ => {
                warn!(
                    job_id,
                    node = %node_name,
                    "no agent address for node, skipping allocation registration"
                );
                failures += 1;
                continue;
            }
        };

        let agent_addr = format!("http://{}:{}", addr, port);
        let result_node = node_name.clone();
        let allocated = per_node_allocs.get(node_name).cloned().unwrap_or_default();
        let params = AllocationRegisterParams {
            job_id,
            partition: spec.partition.clone().unwrap_or_default(),
            uid: spec.uid,
            gid: spec.gid,
            mpi: spec.mpi.clone().unwrap_or_default(),
            allocated_nodelist: allocated_nodelist.clone(),
            allocated,
            work_dir: spec.work_dir.clone(),
        };
        set.spawn(async move {
            let result = register_allocation_to_agent(&agent_addr, &params).await;
            (result_node, result)
        });
    }

    while let Some(result) = set.join_next().await {
        match result {
            Ok((node_name, Ok(()))) => {
                successes += 1;
                succeeded_nodes.push(node_name);
            }
            Ok((node_name, Err(e))) => {
                error!(
                    job_id,
                    node = %node_name,
                    error = %e,
                    "allocation registration on agent failed"
                );
                failures += 1;
            }
            Err(e) => {
                error!(job_id, error = %e, "allocation registration task panicked");
                failures += 1;
            }
        }
    }

    if successes == 0 && total > 0 {
        error!(job_id, failures, "all allocation registrations failed");
        AllocationRegisterOutcome::AllFailed
    } else if failures > 0 {
        warn!(
            job_id,
            successes, failures, "partial allocation registration failure"
        );
        AllocationRegisterOutcome::PartialFailed { succeeded_nodes }
    } else {
        AllocationRegisterOutcome::AllSucceeded
    }
}

/// Release standalone srun allocations on agents after CompleteJob.
pub async fn release_srun_allocation_on_agents(
    cluster: &Arc<ClusterManager>,
    job: &spur_core::job::Job,
) {
    send_cancel_to_agents(cluster, job, 0).await;
}

/// Dispatch a job to every assigned node and act on the aggregate result:
/// requeue to Pending if every node rejected the launch, or evict the job to
/// NodeFail if only some nodes accepted it (a node that never launched the
/// job will never report completion, so the job would otherwise hang).
#[allow(clippy::too_many_arguments)]
async fn dispatch_job_to_nodes(
    cluster: Arc<ClusterManager>,
    job_id: spur_core::job::JobId,
    dispatch_nodes: Vec<String>,
    spec: spur_core::job::JobSpec,
    peer_addrs: Vec<String>,
    per_node_allocs: std::collections::HashMap<String, spur_core::resource::ResourceAllocations>,
    allocated_nodelist: String,
    tasks_per_node: u32,
    run_attempt: u32,
) {
    let mut successes = 0u32;
    let mut failures = 0u32;
    let mut succeeded_nodes: Vec<String> = Vec::new();
    let total = dispatch_nodes.len() as u32;

    // Batch stdout/stderr live on the primary node (task_offset == 0). Capture
    // only its resolved paths; the JoinSet completes out of order, so select by
    // this flag rather than arrival order.
    let mut primary_outcome: Option<LaunchOutcome> = None;

    let mut set = tokio::task::JoinSet::new();
    for (node_idx, node_name) in dispatch_nodes.iter().enumerate() {
        let node_info = cluster.get_node(node_name);
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
        let is_primary = task_offset == 0;
        let target_node = node_name.clone();
        let result_node = node_name.clone();
        let allocated = per_node_allocs.get(node_name).cloned().unwrap_or_default();
        let allocated_nodelist = allocated_nodelist.clone();
        set.spawn(async move {
            let result = dispatch_to_agent(
                &agent_addr,
                &AgentDispatchParams {
                    job_id,
                    spec: &spec,
                    peer_nodes: &peer_addrs,
                    task_offset,
                    target_node: &target_node,
                    allocated: &allocated,
                    allocated_nodelist: &allocated_nodelist,
                    run_attempt,
                },
            )
            .await;
            (result_node, is_primary, result)
        });
    }

    while let Some(result) = set.join_next().await {
        match result {
            Ok((node_name, is_primary, Ok(outcome))) => {
                successes += 1;
                succeeded_nodes.push(node_name);
                if is_primary {
                    primary_outcome = Some(outcome);
                }
            }
            Ok((node_name, _, Err(e))) => {
                error!(job_id, node = %node_name, error = %e, "dispatch to agent failed");
                failures += 1;
            }
            Err(e) => {
                error!(job_id, error = %e, "dispatch task panicked");
                failures += 1;
            }
        }
    }

    // Only surface the primary's paths on a clean launch; a partial/total
    // failure requeues or evicts the job, leaving it to fall back to the
    // computed path on the next attempt.
    if failures == 0 {
        if let Some(outcome) = primary_outcome {
            cluster.set_job_output_paths(job_id, outcome.stdout_path, outcome.stderr_path);
        }
    }

    // If ALL dispatches failed, requeue the job back to Pending
    // so the scheduler can retry (e.g., container image may be
    // imported later, or a transient agent error may resolve) rather
    // than failing it outright and denying the user a chance to fix it.
    if successes == 0 && total > 0 {
        error!(
            job_id,
            failures, "all dispatches failed — requeueing job to Pending"
        );
        if let Err(e) = cluster.requeue_job(job_id) {
            error!(job_id, error = %e, "failed to requeue job after dispatch failure");
        }
    } else if failures > 0 {
        // A node that never got the dispatch will never report completion, so evict
        // the whole job to NodeFail (same as a node dying mid-run) instead of hanging.
        warn!(
            job_id,
            successes, failures, "partial dispatch failure — evicting job to NodeFail"
        );
        // Cancel the job on nodes that DID launch it *before* evicting, and wait
        // for those cancels to be delivered. Eviction can synchronously requeue
        // the job to Pending (when `spec.requeue` is set), making it eligible for
        // immediate re-dispatch on the next scheduler cycle; because CancelJob is
        // keyed only by job_id, a cancel still in flight would otherwise be able
        // to terminate a freshly re-dispatched attempt on the same node. Ordering
        // the awaited cancel ahead of the requeue closes that race — the launched
        // processes are stopped before the job can be relaunched anywhere.
        cancel_job_on_nodes(&cluster, job_id, &succeeded_nodes, 9).await;
        if let Err(e) = cluster.evict_job(job_id) {
            error!(job_id, error = %e, "failed to evict job after partial dispatch failure");
        }
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

        // Deadline enforcement: mark pending jobs whose deadline has passed
        {
            let pending = cluster.get_jobs(
                &[spur_core::job::JobState::Pending],
                None,
                None,
                None,
                None,
                &[],
            );
            for job in &pending {
                if let Some(deadline) = job.spec.deadline {
                    if now > deadline {
                        if let Err(e) = cluster.deadline_job(job.job_id) {
                            warn!(job_id = job.job_id, error = %e, "failed to mark job DEADLINE");
                        }
                    }
                }
            }
        }

        let running = cluster.get_jobs(
            &[
                spur_core::job::JobState::Running,
                spur_core::job::JobState::Completing,
            ],
            None,
            None,
            None,
            None,
            &[],
        );

        for job in &running {
            if job.state == spur_core::job::JobState::Completing {
                continue;
            }

            let (Some(time_limit), Some(start_time)) = (job.spec.time_limit, job.start_time) else {
                continue;
            };
            let deadline = job.effective_deadline(start_time, time_limit);
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

/// Force-finish jobs stuck in COMPLETING past `complete_wait_secs`.
async fn enforce_completing_timeout(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

    loop {
        interval.tick().await;

        if !raft.is_leader() {
            continue;
        }

        let now = Utc::now();
        let wait = chrono::Duration::seconds(cluster.config.scheduler.complete_wait_secs as i64);

        let completing = cluster.get_jobs(
            &[spur_core::job::JobState::Completing],
            None,
            None,
            None,
            None,
            &[],
        );

        for job in completing {
            let Some(completing_since) = job.end_time else {
                continue;
            };
            if now - completing_since < wait {
                continue;
            }

            force_finish_completing_job(&cluster, &job).await;
        }
    }
}

/// Force-finish a job stuck in Completing past `complete_wait_secs`, cancelling
/// it on the unreported nodes first so their agents release the allocation
/// before the controller frees those nodes. Best-effort; the agent reclaim backs it.
async fn force_finish_completing_job(cluster: &Arc<ClusterManager>, job: &spur_core::job::Job) {
    let missing: Vec<_> = job
        .allocated_nodes
        .iter()
        .filter(|n| !job.node_completions.contains_key(*n))
        .cloned()
        .collect();

    // Empty when no nodes allocated; derived_completion falls back to worst completion.
    let primary = job.allocated_nodes.first().cloned().unwrap_or_default();
    let (mut state, mut exit_code, _signal) =
        spur_core::job::Job::derived_completion(&job.node_completions, &primary);
    if job.node_completions.is_empty() {
        state = spur_core::job::JobState::Failed;
        exit_code = -1;
    } else if !missing.is_empty() {
        warn!(
            job_id = job.job_id,
            missing = ?missing,
            reported = job.node_completions.len(),
            expected = job.allocated_nodes.len(),
            "completing timeout — not all nodes reported"
        );
        state = spur_core::job::JobState::Failed;
        if exit_code == 0 {
            exit_code = 1;
        }
    }

    if !missing.is_empty() {
        cancel_job_on_nodes(cluster, job.job_id, &missing, 9).await;
    }

    info!(
        job_id = job.job_id,
        state = ?state,
        exit_code,
        "completing timeout expired — force-finishing job"
    );

    if let Err(e) = cluster.complete_job(job.job_id, exit_code, state) {
        warn!(
            job_id = job.job_id,
            error = %e,
            "failed to force-finish job after completing timeout"
        );
    }
}

fn spawn_power_command(cmd: &str, node_name: &str, action: &str) {
    let cmd = cmd.to_owned();
    let node_name = node_name.to_owned();
    let action = action.to_owned();
    tokio::spawn(async move {
        if let Err(e) = tokio::process::Command::new("sh")
            .args(["-c", &cmd])
            .status()
            .await
        {
            warn!(node = %node_name, error = %e, action = %action, "power command failed");
        }
    });
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
            if node.state != spur_core::node::NodeState::Idle {
                continue;
            }
            let Some(last_busy) = node.last_busy else {
                continue;
            };
            if (now - last_busy).num_seconds() as u64 <= suspend_timeout {
                continue;
            }
            info!(node = %node.name, "suspending idle node (power saving)");
            let _ = cluster.update_node_state(
                &node.name,
                spur_core::node::NodeState::Suspended,
                Some("Power saving".into()),
            );
            if let Some(ref cmd) = cluster.config.power.suspend_command {
                spawn_power_command(&cmd.replace("{node}", &node.name), &node.name, "suspend");
            }
        }

        // Resume suspended nodes if there are pending jobs
        let pending = cluster.pending_jobs();
        if !pending.is_empty() {
            for node in &nodes {
                if node.state != spur_core::node::NodeState::Suspended {
                    continue;
                }
                info!(node = %node.name, "resuming suspended node for pending jobs");
                let _ =
                    cluster.update_node_state(&node.name, spur_core::node::NodeState::Idle, None);
                if let Some(ref cmd) = cluster.config.power.resume_command {
                    spawn_power_command(&cmd.replace("{node}", &node.name), &node.name, "resume");
                }
            }
        }
    }
}

/// Send CancelJob RPC to all agents for a job with a specific signal.
pub async fn send_cancel_to_agents(
    cluster: &Arc<ClusterManager>,
    job: &spur_core::job::Job,
    signal: i32,
) {
    send_cancel_to_nodes(cluster, job.job_id, &job.allocated_nodes, signal).await;
}

/// Send CancelJob RPC to an explicit set of nodes for a job with a specific
/// signal. Callers that already know which nodes ran the job (e.g. a dispatch
/// loop) should use this instead of `send_cancel_to_agents` so the cancel
/// isn't at the mercy of `job.allocated_nodes` having been mutated in the
/// meantime (e.g. cleared by a requeue-on-eviction side effect).
///
/// Fire-and-forget: each node's cancel runs on its own task and this returns
/// immediately. Use `cancel_job_on_nodes` when the cancel must be delivered
/// before subsequent work (e.g. a requeue that could re-dispatch the job).
pub async fn send_cancel_to_nodes(
    cluster: &Arc<ClusterManager>,
    job_id: spur_core::job::JobId,
    node_names: &[String],
    signal: i32,
) {
    for agent_addr in cancel_agent_addrs(cluster, job_id, node_names) {
        tokio::spawn(cancel_one_agent(agent_addr, job_id, signal));
    }
}

/// Like `send_cancel_to_nodes`, but awaits delivery of every cancel before
/// returning so the caller can establish a happens-before ordering against
/// later actions. Each RPC is bounded by `CANCEL_RPC_TIMEOUT` so an
/// unreachable agent can't stall the caller indefinitely.
pub async fn cancel_job_on_nodes(
    cluster: &Arc<ClusterManager>,
    job_id: spur_core::job::JobId,
    node_names: &[String],
    signal: i32,
) {
    let mut set = tokio::task::JoinSet::new();
    for agent_addr in cancel_agent_addrs(cluster, job_id, node_names) {
        set.spawn(cancel_one_agent(agent_addr, job_id, signal));
    }
    while set.join_next().await.is_some() {}
}

/// Resolve `node_names` to agent URLs, logging and skipping any node whose
/// address is unknown.
fn cancel_agent_addrs(
    cluster: &Arc<ClusterManager>,
    job_id: spur_core::job::JobId,
    node_names: &[String],
) -> Vec<String> {
    let mut addrs = Vec::with_capacity(node_names.len());
    for node_name in node_names {
        match cluster.get_node(node_name) {
            Some(ref n) if n.address.is_some() => {
                addrs.push(format!("http://{}:{}", n.address.clone().unwrap(), n.port));
            }
            _ => {
                warn!(
                    job_id,
                    node = %node_name,
                    "no agent address — cannot cancel job on node"
                );
            }
        }
    }
    addrs
}

/// Deliver one CancelJob RPC, bounded by `CANCEL_RPC_TIMEOUT`. Errors and
/// timeouts are logged, never propagated: a cancel is best-effort cleanup and
/// must not block the caller past the timeout.
async fn cancel_one_agent(agent_addr: String, job_id: spur_core::job::JobId, signal: i32) {
    let attempt = async {
        match SlurmAgentClient::connect(agent_addr.clone())
            .await
            .map(|c| {
                c.max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
                    .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE)
            }) {
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
    };
    if tokio::time::timeout(CANCEL_RPC_TIMEOUT, attempt)
        .await
        .is_err()
    {
        warn!(
            job_id,
            agent = %agent_addr,
            "CancelJob RPC timed out"
        );
    }
}

/// Dispatch suspend (SIGSTOP) or resume (SIGCONT) to every allocated node.
pub async fn send_suspend_to_agents(
    cluster: &Arc<ClusterManager>,
    job: &spur_core::job::Job,
    resume: bool,
) {
    for node_name in &job.allocated_nodes {
        let node_info = cluster.get_node(node_name);
        let (addr, port) = match node_info {
            Some(ref n) if n.address.is_some() => (n.address.clone().unwrap(), n.port),
            _ => {
                warn!(job_id = job.job_id, node = %node_name,
                    "no agent address — cannot suspend/resume job on node");
                continue;
            }
        };
        let agent_addr = format!("http://{}:{}", addr, port);
        let job_id = job.job_id;
        tokio::spawn(async move {
            match SlurmAgentClient::connect(agent_addr.clone())
                .await
                .map(|c| {
                    c.max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
                        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE)
                }) {
                Ok(mut client) => {
                    if let Err(e) = client
                        .suspend_job(AgentSuspendJobRequest { job_id, resume })
                        .await
                    {
                        warn!(job_id, resume, agent = %agent_addr, error = %e, "SuspendJob RPC failed");
                    } else {
                        info!(job_id, resume, agent = %agent_addr, "sent SuspendJob");
                    }
                }
                Err(e) => {
                    warn!(job_id, agent = %agent_addr, error = %e,
                        "failed to connect to agent for suspend/resume");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::{Job, JobSpec};
    use spur_core::resource::{
        build_exclusive_allocation, build_node_allocation, GpuLinkType, GpuResource,
        ResourceAllocations, ResourceSet,
    };
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

    fn exclusive_per_node(
        nodes: &[String],
        totals: &HashMap<String, ResourceSet>,
        memory_mb: u64,
    ) -> HashMap<String, ResourceAllocations> {
        nodes
            .iter()
            .filter_map(|name| {
                totals
                    .get(name)
                    .map(|inv| (name.clone(), build_exclusive_allocation(inv, memory_mb)))
            })
            .collect()
    }

    fn request_per_node(
        nodes: &[String],
        totals: &HashMap<String, ResourceSet>,
        request: &ResourceSet,
    ) -> HashMap<String, ResourceAllocations> {
        nodes
            .iter()
            .filter_map(|name| {
                totals.get(name).map(|inv| {
                    (
                        name.clone(),
                        build_node_allocation(inv, &ResourceAllocations::default(), request),
                    )
                })
            })
            .collect()
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
        let spec = JobSpec {
            cpus_per_task: 2,
            num_tasks: 1,
            num_nodes: 1,
            exclusive: true,
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let totals = HashMap::from([("n1".to_string(), node_total(64, 256_000, vec![]))]);
        let per_node = exclusive_per_node(&nodes, &totals, 0);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(
            alloc.cpus, 64,
            "exclusive job must record full node CPU count, not requested"
        );
    }

    #[test]
    fn exclusive_job_bumps_cpus_across_multiple_nodes() {
        let spec = JobSpec {
            cpus_per_task: 1,
            num_nodes: 2,
            exclusive: true,
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string(), "n2".to_string()];
        let totals = HashMap::from([
            ("n1".to_string(), node_total(64, 256_000, vec![])),
            ("n2".to_string(), node_total(48, 128_000, vec![])),
        ]);
        let per_node = exclusive_per_node(&nodes, &totals, 0);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(alloc.cpus, 112, "exclusive job must sum CPUs across nodes");
    }

    #[test]
    fn exclusive_job_takes_all_gpus_from_each_node() {
        let spec = JobSpec {
            exclusive: true,
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let totals = HashMap::from([(
            "n1".to_string(),
            node_total(64, 256_000, vec![gpu(0, "mi300x"), gpu(1, "mi300x")]),
        )]);
        let per_node = exclusive_per_node(&nodes, &totals, 0);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(
            alloc.total_device_count("gpu"),
            2,
            "exclusive job must take every GPU"
        );
        assert_eq!(alloc.device_ids("gpu"), vec![0, 1]);
    }

    #[test]
    fn exclusive_job_keeps_memory_at_request_not_node_total() {
        let spec = JobSpec {
            cpus_per_task: 1,
            exclusive: true,
            memory_per_node_mb: Some(4096),
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let totals = HashMap::from([("n1".to_string(), node_total(64, 256_000, vec![]))]);
        let per_node = exclusive_per_node(&nodes, &totals, 4096);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(
            alloc.memory_mb, 4096,
            "exclusive memory must stay at request, not node total"
        );
    }

    #[test]
    fn exclusive_job_sums_generic_gres_from_each_node() {
        let spec = JobSpec {
            exclusive: true,
            ..Default::default()
        };
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
        let totals = HashMap::from([("n1".to_string(), total_a), ("n2".to_string(), total_b)]);
        let per_node = exclusive_per_node(&nodes, &totals, 0);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(
            alloc
                .devices
                .get("license:fluent")
                .map(|d| d.iter().map(|x| x.count).sum::<u64>()),
            Some(8)
        );
    }

    #[test]
    fn non_exclusive_job_records_request_not_node_total() {
        let spec = JobSpec {
            cpus_per_task: 2,
            num_tasks: 1,
            num_nodes: 1,
            exclusive: false,
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string()];
        let totals = HashMap::from([("n1".to_string(), node_total(64, 256_000, vec![]))]);
        let request = backfill::job_resource_request(&job);
        let per_node = request_per_node(&nodes, &totals, &request);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(
            alloc.cpus, 2,
            "non-exclusive job must record exactly what was requested"
        );
    }

    #[test]
    fn exclusive_job_handles_missing_node_metadata() {
        let spec = JobSpec {
            exclusive: true,
            ..Default::default()
        };
        let job = job_with_spec(spec);

        let nodes = vec!["n1".to_string(), "ghost".to_string()];
        let totals = HashMap::from([("n1".to_string(), node_total(64, 256_000, vec![]))]);
        let per_node = exclusive_per_node(&nodes, &totals, 0);
        let alloc = compute_job_allocation(&job, &nodes, &per_node);

        assert_eq!(alloc.cpus, 64);
    }

    fn partition_with_mode(
        name: &str,
        mode: spur_core::partition::PreemptMode,
    ) -> spur_core::partition::Partition {
        spur_core::partition::Partition {
            name: name.into(),
            preempt_mode: mode,
            ..Default::default()
        }
    }

    fn job_in_partitions(partition: &str) -> Job {
        job_with_spec(JobSpec {
            partition: Some(partition.into()),
            ..Default::default()
        })
    }

    fn qos_with_mode(mode: spur_core::accounting::QosPreemptMode) -> spur_core::accounting::Qos {
        spur_core::accounting::Qos {
            preempt_mode: mode,
            ..Default::default()
        }
    }

    fn no_qos_override() -> spur_core::accounting::Qos {
        qos_with_mode(spur_core::accounting::QosPreemptMode::Off)
    }

    #[test]
    fn job_preempt_mode_single_partition() {
        use spur_core::partition::PreemptMode;
        let parts = vec![partition_with_mode("gpu", PreemptMode::Requeue)];
        assert_eq!(
            job_preempt_mode(&job_in_partitions("gpu"), &parts, &no_qos_override()),
            PreemptMode::Requeue
        );
    }

    #[test]
    fn job_preempt_mode_unset_or_unknown_is_off() {
        use spur_core::partition::PreemptMode;
        let parts = vec![partition_with_mode("gpu", PreemptMode::Cancel)];
        assert_eq!(
            job_preempt_mode(
                &job_with_spec(JobSpec::default()),
                &parts,
                &no_qos_override()
            ),
            PreemptMode::Off
        );
        assert_eq!(
            job_preempt_mode(&job_in_partitions("nope"), &parts, &no_qos_override()),
            PreemptMode::Off
        );
    }

    #[test]
    fn job_preempt_mode_multi_partition_picks_most_aggressive() {
        use spur_core::partition::PreemptMode;
        // A job spanning gpu,cpu must resolve a mode (was Off before the fix,
        // making multi-partition jobs unpreemptable). Cancel > Requeue.
        let parts = vec![
            partition_with_mode("gpu", PreemptMode::Requeue),
            partition_with_mode("cpu", PreemptMode::Cancel),
        ];
        assert_eq!(
            job_preempt_mode(&job_in_partitions("gpu, cpu"), &parts, &no_qos_override()),
            PreemptMode::Cancel
        );
    }

    #[test]
    fn job_preempt_mode_multi_partition_off_when_none_configured() {
        use spur_core::partition::PreemptMode;
        let parts = vec![
            partition_with_mode("gpu", PreemptMode::Off),
            partition_with_mode("cpu", PreemptMode::Off),
        ];
        assert_eq!(
            job_preempt_mode(&job_in_partitions("gpu,cpu"), &parts, &no_qos_override()),
            PreemptMode::Off
        );
    }

    // ── dispatch_job_to_nodes: real partial-dispatch-failure trigger path ──
    //
    // Exercises the actual JoinSet success/failure counting in
    // dispatch_job_to_nodes (not a reimplementation of it) against a real
    // agent over the network, so the eviction + cancel-RPC behavior it
    // drives is verified end-to-end rather than by calling evict_job
    // directly on an already-Running job.
    mod dispatch_trigger_tests {
        use super::*;
        use spur_core::config::SlurmConfig;
        use spur_core::node::NodeSource;
        use std::sync::atomic::{AtomicU32, Ordering};
        use tempfile::TempDir;
        use tonic::transport::server::TcpIncoming;
        use tonic::transport::Server;

        /// Minimal SlurmAgent: always accepts launch_job and counts cancel_job
        /// calls, so tests can assert the controller actually tried to stop
        /// the job on nodes that did launch it.
        struct MockAgent {
            cancel_calls: Arc<AtomicU32>,
        }

        #[tonic::async_trait]
        impl spur_proto::proto::slurm_agent_server::SlurmAgent for MockAgent {
            type StreamJobOutputStream =
                tonic::codegen::BoxStream<spur_proto::proto::StreamJobOutputChunk>;
            type InteractiveSessionStream =
                tonic::codegen::BoxStream<spur_proto::proto::InteractiveOutput>;

            async fn launch_job(
                &self,
                request: tonic::Request<spur_proto::proto::LaunchJobRequest>,
            ) -> Result<tonic::Response<spur_proto::proto::LaunchJobResponse>, tonic::Status>
            {
                // Echo a path keyed by task_offset so tests can assert the
                // controller keeps the primary node's (task_offset == 0) path.
                let req = request.into_inner();
                let path = format!("/spool/off{}/spur.out", req.task_offset);
                Ok(tonic::Response::new(spur_proto::proto::LaunchJobResponse {
                    success: true,
                    error: String::new(),
                    stdout_path: path.clone(),
                    stderr_path: path,
                }))
            }

            async fn cancel_job(
                &self,
                _request: tonic::Request<spur_proto::proto::AgentCancelJobRequest>,
            ) -> Result<tonic::Response<()>, tonic::Status> {
                self.cancel_calls.fetch_add(1, Ordering::SeqCst);
                Ok(tonic::Response::new(()))
            }

            async fn suspend_job(
                &self,
                _request: tonic::Request<spur_proto::proto::AgentSuspendJobRequest>,
            ) -> Result<tonic::Response<()>, tonic::Status> {
                Ok(tonic::Response::new(()))
            }

            async fn get_node_resources(
                &self,
                _request: tonic::Request<()>,
            ) -> Result<tonic::Response<spur_proto::proto::NodeResourcesResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn exec_in_job(
                &self,
                _request: tonic::Request<spur_proto::proto::ExecInJobRequest>,
            ) -> Result<tonic::Response<spur_proto::proto::ExecInJobResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn run_command(
                &self,
                _request: tonic::Request<spur_proto::proto::RunCommandRequest>,
            ) -> Result<tonic::Response<spur_proto::proto::RunCommandResponse>, tonic::Status>
            {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn register_job_allocation(
                &self,
                _request: tonic::Request<spur_proto::proto::RegisterJobAllocationRequest>,
            ) -> Result<
                tonic::Response<spur_proto::proto::RegisterJobAllocationResponse>,
                tonic::Status,
            > {
                Ok(tonic::Response::new(Default::default()))
            }

            async fn stream_job_output(
                &self,
                _request: tonic::Request<spur_proto::proto::StreamJobOutputRequest>,
            ) -> Result<tonic::Response<Self::StreamJobOutputStream>, tonic::Status> {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn interactive_session(
                &self,
                _request: tonic::Request<tonic::Streaming<spur_proto::proto::InteractiveInput>>,
            ) -> Result<tonic::Response<Self::InteractiveSessionStream>, tonic::Status>
            {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            // k0s cluster-component + mesh RPCs: not exercised by these scheduler tests.
            async fn start_cluster_component(
                &self,
                _request: tonic::Request<spur_proto::proto::StartClusterComponentRequest>,
            ) -> Result<
                tonic::Response<spur_proto::proto::StartClusterComponentResponse>,
                tonic::Status,
            > {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn stop_cluster_component(
                &self,
                _request: tonic::Request<spur_proto::proto::StopClusterComponentRequest>,
            ) -> Result<
                tonic::Response<spur_proto::proto::StopClusterComponentResponse>,
                tonic::Status,
            > {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn get_cluster_component_status(
                &self,
                _request: tonic::Request<spur_proto::proto::GetClusterComponentStatusRequest>,
            ) -> Result<
                tonic::Response<spur_proto::proto::GetClusterComponentStatusResponse>,
                tonic::Status,
            > {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn create_k0s_join_token(
                &self,
                _request: tonic::Request<spur_proto::proto::CreateK0sJoinTokenRequest>,
            ) -> Result<tonic::Response<spur_proto::proto::CreateK0sJoinTokenResponse>, tonic::Status>
            {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn get_admin_kubeconfig(
                &self,
                _request: tonic::Request<spur_proto::proto::GetAdminKubeconfigRequest>,
            ) -> Result<tonic::Response<spur_proto::proto::GetAdminKubeconfigResponse>, tonic::Status>
            {
                Err(tonic::Status::unimplemented("not used in tests"))
            }

            async fn apply_mesh(
                &self,
                _request: tonic::Request<spur_proto::proto::MeshMembership>,
            ) -> Result<tonic::Response<spur_proto::proto::ApplyMeshResponse>, tonic::Status>
            {
                Err(tonic::Status::unimplemented("not used in tests"))
            }
        }

        /// Spawn a real MockAgent gRPC server on an OS-assigned localhost
        /// port. Returns the bound address and the shared cancel-call counter.
        async fn spawn_mock_agent() -> (std::net::SocketAddr, Arc<AtomicU32>) {
            let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = incoming.local_addr().unwrap();
            let cancel_calls = Arc::new(AtomicU32::new(0));
            let agent = MockAgent {
                cancel_calls: cancel_calls.clone(),
            };
            tokio::spawn(async move {
                let _ = Server::builder()
                    .add_service(
                        spur_proto::proto::slurm_agent_server::SlurmAgentServer::new(agent),
                    )
                    .serve_with_incoming(incoming)
                    .await;
            });
            (addr, cancel_calls)
        }

        /// Reserve a localhost port with nothing listening on it, so a
        /// connection attempt to it deterministically fails fast (connection
        /// refused) instead of hanging.
        async fn unreachable_addr() -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            addr
        }

        fn test_config() -> SlurmConfig {
            SlurmConfig {
                cluster_name: "test".into(),
                controller: spur_core::config::ControllerConfig {
                    first_job_id: 1,
                    ..Default::default()
                },
                accounting: Default::default(),
                scheduler: Default::default(),
                auth: Default::default(),
                partitions: vec![spur_core::config::PartitionConfig {
                    name: "default".into(),
                    default: true,
                    state: "UP".into(),
                    nodes: "ALL".into(),
                    selector: Default::default(),
                    max_time: None,
                    default_time: None,
                    max_nodes: None,
                    min_nodes: 1,
                    allow_accounts: Vec::new(),
                    allow_groups: Vec::new(),
                    deny_accounts: Vec::new(),
                    priority_tier: 1,
                    preempt_mode: String::new(),
                }],
                nodes: Vec::new(),
                network: Default::default(),
                logging: Default::default(),
                kubernetes: Default::default(),
                cluster: Default::default(),
                notifications: Default::default(),
                power: Default::default(),
                federation: Default::default(),
                topology: None,
                isolation: Default::default(),
                licenses: HashMap::new(),
                burst_buffer: Default::default(),
                update: Default::default(),
                metrics: Default::default(),
                rest_api: Default::default(),
                hooks: Default::default(),
                devices: Default::default(),
                admission: Default::default(),
                rlimits: Default::default(),
            }
        }

        async fn test_cluster(dir: &TempDir) -> Arc<ClusterManager> {
            let cm = Arc::new(ClusterManager::new(test_config(), dir.path()).unwrap());
            let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cm.clone())
                .await
                .unwrap();
            handle
                .raft
                .wait(Some(std::time::Duration::from_secs(5)))
                .metrics(|m| m.current_leader == Some(1), "leader elected")
                .await
                .expect("single-node raft did not self-elect within 5s");
            cm.set_raft(handle.raft);
            cm
        }

        /// Spin until an async mutation (Raft-committed state, or a
        /// fire-and-forget cancel RPC) is visible. Bounded retry, not an
        /// open-ended wait: fails loudly if the condition never holds.
        fn wait_for<F: Fn() -> bool>(label: &str, f: F) {
            for _ in 0..200 {
                if f() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("timed out waiting for: {label}");
        }

        fn register_node_at(cm: &ClusterManager, name: &str, addr: std::net::SocketAddr) {
            cm.register_node(
                name.into(),
                ResourceSet {
                    cpus: 4,
                    memory_mb: 8000,
                    ..Default::default()
                },
                addr.ip().to_string(),
                addr.port(),
                String::new(),
                String::new(),
                NodeSource::NativeHost,
                HashMap::new(),
            )
            .unwrap();
            let n = name.to_string();
            wait_for(&format!("node '{n}' registered"), || {
                cm.get_node(&n).is_some()
            });
        }

        fn submit_and_wait(cm: &ClusterManager, spec: JobSpec) -> spur_core::job::JobId {
            let id = cm.submit_job(spec).unwrap();
            wait_for(&format!("job {id} applied"), || cm.get_job(id).is_some());
            id
        }

        fn settle(
            cm: &ClusterManager,
            job_id: spur_core::job::JobId,
            expected: spur_core::job::JobState,
        ) {
            wait_for(&format!("job {job_id} -> {expected:?}"), || {
                cm.get_job(job_id).is_some_and(|j| j.state == expected)
            });
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn partial_dispatch_evicts_job_and_cancels_the_node_that_launched() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            let (good_addr, cancel_calls) = spawn_mock_agent().await;
            let bad_addr = unreachable_addr().await;
            register_node_at(&cm, "n1", good_addr);
            register_node_at(&cm, "n2", bad_addr);

            let mut spec = JobSpec {
                name: "partial-dispatch".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec.clone());
            spec = cm.get_job(job_id).unwrap().spec;

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            let run_attempt = cm
                .start_job(
                    job_id,
                    nodes.clone(),
                    ResourceAllocations::with_scalar(2, 0),
                    per_node_allocs.clone(),
                )
                .unwrap();
            settle(&cm, job_id, JobState::Running);

            // This calls the exact same function `run()` spawns per assignment:
            // real network dispatch to both nodes, real JoinSet success/failure
            // counting, and the real branch that decides to evict on a partial
            // failure — n1 (real agent) accepts, n2 (nothing listening) fails.
            dispatch_job_to_nodes(
                cm.clone(),
                job_id,
                nodes,
                spec,
                Vec::new(),
                per_node_allocs,
                "n1,n2".into(),
                1,
                run_attempt,
            )
            .await;

            settle(&cm, job_id, JobState::NodeFail);
            assert_eq!(
                cm.get_job(job_id).unwrap().pending_reason,
                spur_core::job::PendingReason::JobLaunchFailure
            );

            // n1 actually launched the job, so the controller must tell its
            // agent to stop it instead of leaving an orphaned process behind.
            // The cancel is awaited inside dispatch_job_to_nodes, so it has
            // already been delivered by the time that call returned.
            assert_eq!(
                cancel_calls.load(Ordering::SeqCst),
                1,
                "n1 must have been cancelled before dispatch_job_to_nodes returned"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn partial_dispatch_with_requeue_still_cancels_the_node_that_launched() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            let (good_addr, cancel_calls) = spawn_mock_agent().await;
            let bad_addr = unreachable_addr().await;
            register_node_at(&cm, "n1", good_addr);
            register_node_at(&cm, "n2", bad_addr);

            let mut spec = JobSpec {
                name: "partial-dispatch-requeue".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                requeue: true,
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec.clone());
            spec = cm.get_job(job_id).unwrap().spec;

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            cm.start_job(
                job_id,
                nodes.clone(),
                ResourceAllocations::with_scalar(2, 0),
                per_node_allocs.clone(),
            )
            .unwrap();
            settle(&cm, job_id, JobState::Running);

            // Same as the non-requeue case, but `spec.requeue` is set: eviction's
            // requeue side effect resets the job to Pending and clears
            // `allocated_nodes`. The cancel RPC must be delivered to n1 *before*
            // that requeue makes the job eligible for re-dispatch, otherwise a
            // job_id-keyed cancel still in flight could kill a fresh attempt.
            // dispatch_job_to_nodes awaits the cancel before evicting, so by the
            // time it returns both the cancel is delivered and the job is Pending.
            dispatch_job_to_nodes(
                cm.clone(),
                job_id,
                nodes,
                spec,
                Vec::new(),
                per_node_allocs,
                "n1,n2".into(),
                1,
                1,
            )
            .await;

            // Cancel-before-requeue: the cancel is already delivered, and the
            // requeue side effect has put the job back to Pending with its
            // allocation cleared — no polling needed, both are guaranteed by the
            // await ordering inside dispatch_job_to_nodes.
            assert_eq!(
                cancel_calls.load(Ordering::SeqCst),
                1,
                "n1 must have been cancelled before the requeue re-enabled dispatch"
            );
            let job = cm.get_job(job_id).unwrap();
            assert_eq!(job.state, JobState::Pending);
            assert!(job.allocated_nodes.is_empty());
        }

        // Force-finish must cancel the job on the unreported node before freeing
        // it, or the agent keeps the stale allocation and rejects the next dispatch.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn completing_timeout_cancels_only_the_unreported_node() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            let (addr1, cancel1) = spawn_mock_agent().await;
            let (addr2, cancel2) = spawn_mock_agent().await;
            register_node_at(&cm, "n1", addr1);
            register_node_at(&cm, "n2", addr2);

            let spec = JobSpec {
                name: "completing-timeout".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec);

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            let run_attempt = cm
                .start_job(
                    job_id,
                    nodes,
                    ResourceAllocations::with_scalar(2, 0),
                    per_node_allocs,
                )
                .unwrap();
            settle(&cm, job_id, JobState::Running);

            // n1 reports completion; n2 never does, so the job stays Completing.
            cm.node_complete(job_id, "n1", 0, 0, run_attempt).unwrap();
            settle(&cm, job_id, JobState::Completing);

            let job = cm.get_job(job_id).unwrap();
            force_finish_completing_job(&cm, &job).await;

            assert_eq!(
                cancel2.load(Ordering::SeqCst),
                1,
                "the unreported node n2 must be cancelled before its resources are freed"
            );
            assert_eq!(
                cancel1.load(Ordering::SeqCst),
                0,
                "the node that already reported must not be cancelled"
            );
        }

        // When no node reported (e.g. a suspended job's tasks died out-of-band),
        // every allocated node is unreported and must be cancelled on force-finish.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn completing_timeout_cancels_all_nodes_when_none_reported() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            let (addr1, cancel1) = spawn_mock_agent().await;
            let (addr2, cancel2) = spawn_mock_agent().await;
            register_node_at(&cm, "n1", addr1);
            register_node_at(&cm, "n2", addr2);

            let spec = JobSpec {
                name: "completing-none".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec);

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            cm.start_job(
                job_id,
                nodes,
                ResourceAllocations::with_scalar(2, 0),
                per_node_allocs,
            )
            .unwrap();
            settle(&cm, job_id, JobState::Running);

            // Suspend routes through Completing; no node reports completion.
            cm.suspend_job(job_id, "").unwrap();
            settle(&cm, job_id, JobState::Suspended);
            let mut job = cm.get_job(job_id).unwrap();
            job.transition(JobState::Completing).unwrap();

            force_finish_completing_job(&cm, &job).await;

            assert_eq!(cancel1.load(Ordering::SeqCst), 1, "n1 must be cancelled");
            assert_eq!(cancel2.load(Ordering::SeqCst), 1, "n2 must be cancelled");
        }

        // Mock agents echo an offset-keyed path; the stored path must be the
        // primary's (task_offset == 0) regardless of which response arrives first.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn clean_dispatch_stores_primary_node_output_path() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            let (addr1, _) = spawn_mock_agent().await;
            let (addr2, _) = spawn_mock_agent().await;
            register_node_at(&cm, "n1", addr1);
            register_node_at(&cm, "n2", addr2);

            let spec = JobSpec {
                name: "multi-out".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec.clone());
            let spec = cm.get_job(job_id).unwrap().spec;

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            let run_attempt = cm
                .start_job(
                    job_id,
                    nodes.clone(),
                    ResourceAllocations::with_scalar(2, 0),
                    per_node_allocs.clone(),
                )
                .unwrap();
            settle(&cm, job_id, JobState::Running);

            dispatch_job_to_nodes(
                cm.clone(),
                job_id,
                nodes,
                spec,
                Vec::new(),
                per_node_allocs,
                "n1,n2".into(),
                1,
                run_attempt,
            )
            .await;

            let job = cm.get_job(job_id).unwrap();
            assert_eq!(
                job.actual_stdout_path.as_deref(),
                Some("/spool/off0/spur.out"),
                "must store the primary (task_offset==0) node's path"
            );
            assert_eq!(
                job.actual_stderr_path.as_deref(),
                Some("/spool/off0/spur.out")
            );
        }

        // If the primary node fails to launch, the dispatch requeues/evicts the
        // job and no output path is recorded, so queries fall back to the
        // computed path.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn failed_primary_leaves_output_path_unset() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            // Primary (n1) is unreachable; secondary (n2) accepts.
            let bad_addr = unreachable_addr().await;
            let (good_addr, _) = spawn_mock_agent().await;
            register_node_at(&cm, "n1", bad_addr);
            register_node_at(&cm, "n2", good_addr);

            let spec = JobSpec {
                name: "primary-fail".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec.clone());
            let spec = cm.get_job(job_id).unwrap().spec;

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            let run_attempt = cm
                .start_job(
                    job_id,
                    nodes.clone(),
                    ResourceAllocations::with_scalar(2, 0),
                    per_node_allocs.clone(),
                )
                .unwrap();
            settle(&cm, job_id, JobState::Running);

            dispatch_job_to_nodes(
                cm.clone(),
                job_id,
                nodes,
                spec,
                Vec::new(),
                per_node_allocs,
                "n1,n2".into(),
                1,
                run_attempt,
            )
            .await;

            let job = cm.get_job(job_id).unwrap();
            assert!(
                job.actual_stdout_path.is_none(),
                "a failed dispatch must not record an output path"
            );
            assert!(job.actual_stderr_path.is_none());
        }

        // A secondary-node failure (primary succeeds) still evicts/requeues the
        // job, so the `failures == 0` gate must skip storing the primary's path.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn secondary_failure_leaves_output_path_unset() {
            use spur_core::job::JobState;

            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;

            // Primary (n1) accepts; secondary (n2) is unreachable.
            let (good_addr, _) = spawn_mock_agent().await;
            let bad_addr = unreachable_addr().await;
            register_node_at(&cm, "n1", good_addr);
            register_node_at(&cm, "n2", bad_addr);

            let spec = JobSpec {
                name: "secondary-fail".into(),
                user: "testuser".into(),
                num_nodes: 2,
                num_tasks: 2,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            };
            let job_id = submit_and_wait(&cm, spec.clone());
            let spec = cm.get_job(job_id).unwrap().spec;

            let nodes = vec!["n1".to_string(), "n2".to_string()];
            let per_node_allocs: HashMap<String, ResourceAllocations> = nodes
                .iter()
                .map(|n| (n.clone(), ResourceAllocations::with_scalar(1, 0)))
                .collect();
            let run_attempt = cm
                .start_job(
                    job_id,
                    nodes.clone(),
                    ResourceAllocations::with_scalar(2, 0),
                    per_node_allocs.clone(),
                )
                .unwrap();
            settle(&cm, job_id, JobState::Running);

            dispatch_job_to_nodes(
                cm.clone(),
                job_id,
                nodes,
                spec,
                Vec::new(),
                per_node_allocs,
                "n1,n2".into(),
                1,
                run_attempt,
            )
            .await;

            let job = cm.get_job(job_id).unwrap();
            assert!(
                job.actual_stdout_path.is_none(),
                "a partial dispatch failure must not record an output path"
            );
            assert!(job.actual_stderr_path.is_none());
        }
    }

    #[test]
    fn job_preempt_mode_qos_override_wins_over_partition() {
        use spur_core::accounting::QosPreemptMode;
        use spur_core::partition::PreemptMode;
        let parts = vec![partition_with_mode("gpu", PreemptMode::Cancel)];
        let qos = qos_with_mode(QosPreemptMode::Requeue);
        assert_eq!(
            job_preempt_mode(&job_in_partitions("gpu"), &parts, &qos),
            PreemptMode::Requeue
        );
    }

    #[test]
    fn job_preempt_mode_qos_off_falls_back_to_partition() {
        use spur_core::partition::PreemptMode;
        let parts = vec![partition_with_mode("gpu", PreemptMode::Cancel)];
        assert_eq!(
            job_preempt_mode(&job_in_partitions("gpu"), &parts, &no_qos_override()),
            PreemptMode::Cancel
        );
    }
}
