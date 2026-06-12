// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC server implementing the SlurmAgent service.
//! Receives job launch/cancel requests from spurctld.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use tokio_stream::wrappers::ReceiverStream;

use spur_proto::proto::slurm_agent_server::SlurmAgent;
use spur_proto::proto::*;

use spur_sched::cons_tres::{AllocationResult, NodeAllocation};

use spur_spank::{SpankHook, SpankHost};

use spur_core::config::HooksConfig;
use spur_devices::DeviceRegistry;

use crate::executor;
use crate::pmi::PmiServer;
use crate::reporter::NodeReporter;

struct TrackedJob {
    job: executor::RunningJob,
    rootfs_mode: crate::container::RootfsMode,
    allocation: Option<AllocationResult>,
    stdout_path: String,
    stderr_path: String,
    has_pid_namespace: bool,
    work_dir: String,
    uid: u32,
    gid: u32,
    partition: String,
    gpu_devices: Vec<u32>,
    cpus: u32,
    memory_mb: u64,
    nodelist: String,
}

struct CompletedJob {
    job_id: u32,
    exit_code: i32,
    signal: i32,
    rootfs_mode: crate::container::RootfsMode,
    allocation: Option<AllocationResult>,
    cgroup: Option<std::path::PathBuf>,
    work_dir: String,
    uid: u32,
    gid: u32,
    partition: String,
    gpu_devices: Vec<u32>,
    cpus: u32,
    memory_mb: u64,
    nodelist: String,
}

pub struct AgentService {
    pub reporter: Arc<NodeReporter>,
    running: Arc<Mutex<HashMap<u32, TrackedJob>>>,
    allocation: Arc<Mutex<NodeAllocation>>,
    spank: Arc<Option<SpankHost>>,
    pmi_servers: Arc<Mutex<HashMap<u32, Arc<PmiServer>>>>,
    hooks: Arc<HooksConfig>,
    #[allow(dead_code)]
    device_registry: Arc<Mutex<DeviceRegistry>>,
}

impl AgentService {
    pub fn new(
        reporter: Arc<NodeReporter>,
        hooks: HooksConfig,
        device_registry: Arc<Mutex<DeviceRegistry>>,
    ) -> Self {
        let allocation = NodeAllocation::new(
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".into()),
            &reporter.resources,
        );

        // Load SPANK plugins from plugstack.conf if available
        let plugstack_path = std::env::var("SPUR_PLUGSTACK")
            .unwrap_or_else(|_| "/etc/spur/plugstack.conf".to_string());
        let spank = if std::path::Path::new(&plugstack_path).exists() {
            match spur_spank::parse_plugstack(std::path::Path::new(&plugstack_path)) {
                Ok(entries) => {
                    let mut host = SpankHost::new();
                    for entry in &entries {
                        if let Err(e) = host.load_plugin(&entry.path) {
                            if entry.required {
                                warn!(
                                    plugin = %entry.path.display(),
                                    error = %e,
                                    "required SPANK plugin failed to load"
                                );
                            } else {
                                info!(
                                    plugin = %entry.path.display(),
                                    error = %e,
                                    "optional SPANK plugin failed to load, skipping"
                                );
                            }
                        }
                    }
                    if host.plugin_count() > 0 {
                        info!(count = host.plugin_count(), "SPANK plugins loaded");
                        Some(host)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    warn!(
                        path = %plugstack_path,
                        error = %e,
                        "failed to parse plugstack.conf"
                    );
                    None
                }
            }
        } else {
            None
        };

        Self {
            reporter,
            running: Arc::new(Mutex::new(HashMap::new())),
            allocation: Arc::new(Mutex::new(allocation)),
            spank: Arc::new(spank),
            pmi_servers: Arc::new(Mutex::new(HashMap::new())),
            hooks: Arc::new(hooks),
            device_registry,
        }
    }

    /// Spawn a background task to monitor running jobs and report completions.
    pub fn start_monitor(&self, controller_addr: String) {
        let running = self.running.clone();
        let allocation = self.allocation.clone();
        let spank = self.spank.clone();
        let pmi_servers = self.pmi_servers.clone();
        let hooks = self.hooks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let mut jobs = running.lock().await;
                let mut completed: Vec<CompletedJob> = Vec::new();

                for (job_id, tracked) in jobs.iter_mut() {
                    match tracked.job.try_wait() {
                        Ok(Some((exit_code, signal))) => {
                            info!(job_id, exit_code, signal, "job finished");
                            completed.push(CompletedJob {
                                job_id: *job_id,
                                exit_code,
                                signal,
                                rootfs_mode: tracked.rootfs_mode.clone(),
                                allocation: tracked.allocation.take(),
                                cgroup: tracked.job.take_cgroup(),
                                work_dir: tracked.work_dir.clone(),
                                uid: tracked.uid,
                                gid: tracked.gid,
                                partition: tracked.partition.clone(),
                                gpu_devices: tracked.gpu_devices.clone(),
                                cpus: tracked.cpus,
                                memory_mb: tracked.memory_mb,
                                nodelist: tracked.nodelist.clone(),
                            });
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(job_id, error = %e, "failed to check job status");
                        }
                    }
                }

                for c in &completed {
                    jobs.remove(&c.job_id);
                    crate::container::cleanup_rootfs(c.job_id, &c.rootfs_mode);
                    if let Some(ref cgroup) = c.cgroup {
                        crate::executor::cleanup_cgroup(cgroup);
                    }
                    if let Some(ref alloc) = c.allocation {
                        allocation.lock().await.release(alloc);
                    }
                    if let Some(pmi) = pmi_servers.lock().await.remove(&c.job_id) {
                        pmi.cleanup();
                    }
                }

                // Release lock BEFORE network I/O — holding the lock during
                // report_completion blocks new job launches and can lose
                // completions if the RPC times out.
                drop(jobs);

                let local_hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into());

                let mut drain_jobs: std::collections::HashSet<u32> =
                    std::collections::HashSet::new();

                // Run epilog hook for completed jobs
                if let Some(ref epilog_script) = hooks.epilog {
                    for c in &completed {
                        let ctx = spur_core::hooks::HookContext {
                            job_id: c.job_id,
                            work_dir: c.work_dir.clone(),
                            uid: c.uid,
                            gid: c.gid,
                            partition: c.partition.clone(),
                            nodelist: c.nodelist.clone(),
                            script_context: "epilog_slurmd".into(),
                            gpu_devices: c.gpu_devices.clone(),
                            cpus: c.cpus,
                            memory_mb: c.memory_mb,
                        };
                        if let Err(e) = spur_core::hooks::run_hook(epilog_script, &ctx).await {
                            error!(
                                job_id = c.job_id,
                                error = %e,
                                "epilog hook failed — requesting node drain"
                            );
                            drain_jobs.insert(c.job_id);
                        }
                    }
                }

                // Invoke SPANK TaskExit and JobEpilog hooks for completed jobs
                if let Some(ref spank_host) = *spank {
                    for c in &completed {
                        if let Err(e) = spank_host.invoke_hook(SpankHook::TaskExit) {
                            warn!(c.job_id, error = %e, "SPANK TaskExit hook failed");
                        }
                        if let Err(e) = spank_host.invoke_hook(SpankHook::JobEpilog) {
                            warn!(c.job_id, error = %e, "SPANK JobEpilog hook failed");
                        }
                    }
                }

                for c in &completed {
                    let drain = if drain_jobs.contains(&c.job_id) {
                        Some(DrainRequest {
                            reason: "epilog script failed".into(),
                        })
                    } else {
                        None
                    };
                    report_completion(
                        &controller_addr,
                        c.job_id,
                        c.exit_code,
                        c.signal,
                        &local_hostname,
                        drain.as_ref(),
                    )
                    .await;
                }
            }
        });
    }
}

struct DrainRequest {
    reason: String,
}

fn completion_report_retryable(status: &tonic::Status) -> bool {
    use tonic::Code;
    matches!(
        status.code(),
        Code::Unavailable | Code::Internal | Code::DeadlineExceeded | Code::Unknown
    )
}

#[cfg(test)]
mod completion_report_tests {
    use super::completion_report_retryable;
    use tonic::Status;

    #[test]
    fn permanent_errors_are_not_retryable() {
        assert!(!completion_report_retryable(&Status::invalid_argument("x")));
        assert!(!completion_report_retryable(&Status::not_found("x")));
    }

    #[test]
    fn transient_errors_are_retryable() {
        assert!(completion_report_retryable(&Status::unavailable("x")));
        assert!(completion_report_retryable(&Status::internal("x")));
    }
}

async fn report_completion(
    controller_addr: &str,
    job_id: u32,
    exit_code: i32,
    signal: i32,
    reporting_node: &str,
    drain: Option<&DrainRequest>,
) {
    use spur_proto::proto::slurm_controller_client::SlurmControllerClient;

    // Wire `state` is derived from `exit_code` alone (advisory): a signaled job
    // reports Completed/0 because the controller's validator requires
    // state<->exit_code agreement. The controller rederives the true Failed /
    // RaisedSignal outcome from the reported `signal`.
    let state = spur_core::job::JobState::completion_state_for_exit_code(exit_code).to_proto_i32();

    let url = if controller_addr.starts_with("http") {
        controller_addr.to_string()
    } else {
        format!("http://{}", controller_addr)
    };

    // Retry up to 3 times with 1-second backoff — a single transient failure
    // must not permanently lose a job completion.
    for attempt in 1..=3 {
        match SlurmControllerClient::connect(url.clone()).await {
            Ok(mut client) => {
                let req = ReportJobStatusRequest {
                    job_id,
                    state,
                    exit_code,
                    signal,
                    message: format!("exit_code={}", exit_code),
                    drain_node: drain.is_some(),
                    drain_reason: drain.as_ref().map(|d| d.reason.clone()).unwrap_or_default(),
                    reporting_node: reporting_node.to_string(),
                };
                match client.report_job_status(req).await {
                    Ok(_) => {
                        info!(job_id, exit_code, "reported completion to controller");
                        return;
                    }
                    Err(e) => {
                        if !completion_report_retryable(&e) {
                            error!(
                                job_id,
                                attempt,
                                code = ?e.code(),
                                error = %e,
                                "ReportJobStatus failed with non-retryable error"
                            );
                            return;
                        }
                        warn!(
                            job_id,
                            attempt,
                            error = %e,
                            "ReportJobStatus RPC failed"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    job_id,
                    attempt,
                    error = %e,
                    "failed to connect to controller for completion report"
                );
            }
        }
        if attempt < 3 {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
    error!(
        job_id,
        exit_code, "gave up reporting completion after 3 attempts"
    );
}

#[tonic::async_trait]
impl SlurmAgent for AgentService {
    type StreamJobOutputStream = ReceiverStream<Result<StreamJobOutputChunk, Status>>;
    type AttachJobStream = ReceiverStream<Result<AttachJobOutput, Status>>;

    async fn launch_job(
        &self,
        request: Request<LaunchJobRequest>,
    ) -> Result<Response<LaunchJobResponse>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id;
        let peer_nodes = req.peer_nodes;
        let task_offset = req.task_offset;
        // Per-task array identity is controller-assigned on the launch request,
        // not part of the (user-supplied) job spec.
        let array_job_id = req.array_job_id;
        let array_task_id = req.array_task_id;
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("missing job spec"))?;

        info!(
            job_id,
            name = %spec.name,
            task_offset,
            num_peers = peer_nodes.len(),
            "received job launch request"
        );

        let work_dir = if spec.work_dir.is_empty() {
            "/tmp".to_string()
        } else {
            spec.work_dir.clone()
        };

        let script = if spec.script.is_empty() {
            if spec.argv.is_empty() {
                return Err(Status::invalid_argument("no script or argv"));
            }
            // Build a script from argv
            let mut s = String::from("#!/bin/bash\n");
            s.push_str(&spec.argv.join(" "));
            s.push('\n');
            s
        } else {
            spec.script.clone()
        };

        // Inject peer node info as environment variables for MPI/distributed apps
        let mut env = spec.environment.clone();

        // Ensure the Spur CLI binaries (srun/sbatch/... symlinks to `spur`) are
        // on the job's PATH so `srun` works inside batch scripts. They live next
        // to this agent binary, so derive the dir from the agent's own path
        // (deployment-independent — no assumption about the install location).
        if let Some(bin_dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            let bin_dir = bin_dir.to_string_lossy().to_string();
            let base = env
                .get("PATH")
                .cloned()
                .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
            if !base.split(':').any(|p| p == bin_dir) {
                env.insert("PATH".into(), format!("{}:{}", bin_dir, base));
            }
        }

        env.insert("SPUR_JOB_ID".into(), job_id.to_string());
        env.insert("SPUR_TASK_OFFSET".into(), task_offset.to_string());
        env.insert("SPUR_NUM_NODES".into(), peer_nodes.len().to_string());
        // Signal to executor that GRES was explicitly requested (for GPU hiding)
        if !spec.gres.is_empty() {
            env.insert("SPUR_GRES_REQUESTED".into(), "1".into());
        }
        if !peer_nodes.is_empty() {
            env.insert("SPUR_PEER_NODES".into(), peer_nodes.join(","));
        }
        if !req.target_node.is_empty() {
            env.insert("SPUR_TARGET_NODE".into(), req.target_node.clone());
        }

        // Array task identity for scripts. Gate on array_job_id (0 = not an
        // array; job ids start at 1) since task index 0 is legitimate.
        if array_job_id != 0 {
            env.insert("SLURM_ARRAY_TASK_ID".into(), array_task_id.to_string());
            env.insert("SLURM_ARRAY_JOB_ID".into(), array_job_id.to_string());
            env.insert("SPUR_ARRAY_TASK_ID".into(), array_task_id.to_string());
            env.insert("SPUR_ARRAY_JOB_ID".into(), array_job_id.to_string());
        }

        // Burst buffer: pass via env var so executor can wrap the script
        if !spec.burst_buffer.is_empty() {
            env.insert("SPUR_BURST_BUFFER".into(), spec.burst_buffer.clone());
        }

        // Compute tasks_per_node for both single- and multi-node jobs
        let tasks_per_node = if spec.tasks_per_node > 0 {
            spec.tasks_per_node
        } else {
            (spec.num_tasks / spec.num_nodes.max(1)).max(1)
        };
        let node_rank = task_offset / tasks_per_node.max(1);

        // LOCAL_RANK / LOCAL_WORLD_SIZE — always set, even for single-node jobs
        env.insert("LOCAL_RANK".into(), "0".to_string()); // Single process per node (multi-task wrapper overrides per-process)
        env.insert("LOCAL_WORLD_SIZE".into(), tasks_per_node.to_string());
        env.insert("NPROC_PER_NODE".into(), tasks_per_node.to_string());
        env.insert("NODE_RANK".into(), node_rank.to_string());

        // PMI env vars for MPI runtimes
        env.insert("PMI_SIZE".into(), spec.num_tasks.to_string());
        env.insert("PMI_UNIVERSE_SIZE".into(), spec.num_tasks.to_string());
        env.insert("PMI_APPNUM".into(), "0".to_string());
        // PMI_RANK is set per-task in the multi-task wrapper; default to task_offset for single-task
        env.insert("PMI_RANK".into(), task_offset.to_string());

        // PMIx environment (for OpenMPI and other PMIx-aware runtimes)
        if spec.mpi == "pmix" {
            env.insert("PMIX_SIZE".into(), spec.num_tasks.to_string());
            env.insert("PMIX_NAMESPACE".into(), format!("spur.{}", job_id));
            // PMIX_RANK is set per-task in the multi-task wrapper; default to task_offset
            env.insert("PMIX_RANK".into(), task_offset.to_string());
            // OpenMPI direct-launch bootstrap vars
            env.insert("OMPI_COMM_WORLD_SIZE".into(), spec.num_tasks.to_string());
            env.insert("OMPI_COMM_WORLD_RANK".into(), task_offset.to_string());
            env.insert("OMPI_COMM_WORLD_LOCAL_RANK".into(), "0".to_string());
            env.insert(
                "OMPI_COMM_WORLD_LOCAL_SIZE".into(),
                tasks_per_node.to_string(),
            );
            env.insert("OMPI_COMM_WORLD_NODE_RANK".into(), node_rank.to_string());
        }

        // PyTorch/NCCL/RCCL distributed training env vars
        if peer_nodes.len() > 1 {
            // MASTER_ADDR: first peer node's address (strip port)
            if let Some(first_peer) = peer_nodes.first() {
                let master_addr = first_peer
                    .rsplit(':')
                    .nth(1)
                    .or_else(|| first_peer.split(':').next())
                    .unwrap_or(first_peer);
                env.insert("MASTER_ADDR".into(), master_addr.to_string());
            }
            env.insert("MASTER_PORT".into(), "29500".to_string());
            env.insert("WORLD_SIZE".into(), peer_nodes.len().to_string());

            // RANK = node index within peer list
            env.insert("RANK".into(), node_rank.to_string());
            env.insert("SPUR_NODE_RANK".into(), node_rank.to_string());
        }

        // If container image is specified, prepare rootfs and config for
        // the Rust container runtime (fork + container_init + pivot_root).
        let mut container_config: Option<crate::container::ContainerConfig> = None;
        let mut rootfs_path: Option<std::path::PathBuf> = None;

        let (launch_script, rootfs_mode) = if !spec.container_image.is_empty() {
            info!(job_id, image = %spec.container_image, "launching containerized job");

            let mounts: Vec<crate::container::BindMount> = spec
                .container_mounts
                .iter()
                .filter_map(|m| crate::container::parse_mount(m).ok())
                .collect();

            let username = spec.user.clone();
            let uid = spec.uid;
            let gid = spec.gid;
            let home_dir = std::env::var("HOME").unwrap_or_else(|_| format!("/home/{}", username));

            let cfg = crate::container::ContainerConfig {
                image: spec.container_image.clone(),
                mounts,
                workdir: if spec.container_workdir.is_empty() {
                    None
                } else {
                    Some(spec.container_workdir.clone())
                },
                name: if spec.container_name.is_empty() {
                    None
                } else {
                    Some(spec.container_name.clone())
                },
                readonly: spec.container_readonly,
                mount_home: spec.container_mount_home,
                remap_root: spec.container_remap_root,
                gpu_devices: vec![], // overwritten below after GRES allocation
                environment: env.clone(),
                container_env: spec.container_env.clone(),
                entrypoint: if spec.container_entrypoint.is_empty() {
                    None
                } else {
                    Some(spec.container_entrypoint.clone())
                },
                uid,
                gid,
                username: if username.is_empty() {
                    "spur".to_string()
                } else {
                    username
                },
                home_dir,
                device_plan: None, // set after GRES allocation
            };

            let image_path = crate::container::resolve_image(
                &spec.container_image,
                Some(&spec.user),
                Some(spec.uid),
            )
            .map_err(|e| Status::failed_precondition(e.to_string()))?;

            let (rootfs, rootfs_mode) =
                crate::container::setup_rootfs(&image_path, job_id, cfg.name.as_deref())
                    .map_err(|e| Status::internal(format!("container setup failed: {}", e)))?;

            // Copy user script into rootfs/tmp/ so it's accessible after pivot_root
            let container_script = format!("{}/tmp/spur_job_{}.sh", rootfs.display(), job_id);
            std::fs::write(&container_script, &script).map_err(|e| {
                Status::internal(format!("failed to write container script: {}", e))
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &container_script,
                    std::fs::Permissions::from_mode(0o755),
                );
            }

            rootfs_path = Some(rootfs);
            container_config = Some(cfg);

            // The launch_script passed to executor is the user's script
            // (used as fallback for non-container path; for container path,
            // the executor reads from rootfs/tmp/ directly).
            (script, rootfs_mode)
        } else {
            (script, crate::container::RootfsMode::Extracted)
        };

        // PMI-1 server: if MPI mode is "pmi1" and multiple tasks, start a
        // Unix socket KVS server so MPI ranks can bootstrap via PMI.
        if spec.mpi == "pmi1" && tasks_per_node > 1 {
            let socket_path = format!("/tmp/spur-pmi-{}.sock", job_id);
            let pmi = Arc::new(PmiServer::new(&socket_path, spec.num_tasks));
            let pmi_run = pmi.clone();
            tokio::spawn(async move {
                pmi_run.run().await;
            });
            env.insert("PMI_PORT".into(), socket_path.clone());
            self.pmi_servers.lock().await.insert(job_id, pmi);
        }

        // Multi-task per-node: wrap the user script so it forks N processes,
        // each with a distinct LOCAL_RANK. The wrapper backgrounds N copies and
        // waits for all to finish, so TrackedJob only tracks a single PID (the
        // wrapper shell). GPU devices are partitioned across tasks via
        // ROCR_VISIBLE_DEVICES / CUDA_VISIBLE_DEVICES overrides in each fork.
        let launch_script = if tasks_per_node > 1 {
            // Write the user script to disk first so the wrapper can reference it
            let user_script_path = format!("{}/.spur_user_{}.sh", work_dir, job_id);
            std::fs::write(&user_script_path, &launch_script)
                .map_err(|e| Status::internal(format!("failed to write user script: {}", e)))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &user_script_path,
                    std::fs::Permissions::from_mode(0o755),
                );
            }

            // Build the wrapper that launches N tasks with GPU partitioning
            let mut wrapper = String::from("#!/bin/bash\n");
            wrapper.push_str(&format!(
                "SPUR_NTASKS={}\nSPUR_TASK_OFFSET=${{SPUR_TASK_OFFSET:-0}}\n",
                tasks_per_node
            ));
            wrapper.push_str("for LOCAL_RANK in $(seq 0 $((SPUR_NTASKS - 1))); do\n");
            wrapper.push_str("  export LOCAL_RANK\n");
            wrapper.push_str("  export SPUR_LOCALID=$LOCAL_RANK\n");
            wrapper.push_str("  export SPUR_PROCID=$((SPUR_TASK_OFFSET + LOCAL_RANK))\n");
            wrapper.push_str("  export PMI_RANK=$SPUR_PROCID\n");
            // PMIx per-task overrides
            wrapper.push_str("  export PMIX_RANK=$SPUR_PROCID\n");
            wrapper.push_str("  export OMPI_COMM_WORLD_RANK=$SPUR_PROCID\n");
            wrapper.push_str("  export OMPI_COMM_WORLD_LOCAL_RANK=$LOCAL_RANK\n");

            // Partition GPUs across tasks if GPUs are allocated
            wrapper.push_str("  if [ -n \"$SPUR_JOB_GPUS\" ]; then\n");
            wrapper.push_str("    IFS=',' read -ra _ALL_GPUS <<< \"$SPUR_JOB_GPUS\"\n");
            wrapper.push_str("    _GPUS_PER_TASK=$(( ${#_ALL_GPUS[@]} / SPUR_NTASKS ))\n");
            wrapper.push_str("    if [ $_GPUS_PER_TASK -gt 0 ]; then\n");
            wrapper.push_str("      _START=$((LOCAL_RANK * _GPUS_PER_TASK))\n");
            wrapper.push_str(
                "      _TASK_GPUS=$(echo \"${_ALL_GPUS[@]:$_START:$_GPUS_PER_TASK}\" | tr ' ' ',')\n",
            );
            wrapper.push_str("      export ROCR_VISIBLE_DEVICES=$_TASK_GPUS\n");
            wrapper.push_str("      export CUDA_VISIBLE_DEVICES=$_TASK_GPUS\n");
            wrapper.push_str("      export GPU_DEVICE_ORDINAL=$_TASK_GPUS\n");
            wrapper.push_str("    fi\n");
            wrapper.push_str("  fi\n");

            wrapper.push_str("  if [ \"$SPUR_LABEL\" = \"1\" ]; then\n");
            wrapper.push_str(&format!(
                "    bash \"{}\" 2>&1 | sed \"s/^/[$SPUR_PROCID] /\" &\n",
                user_script_path.replace('"', "\\\"")
            ));
            wrapper.push_str("  else\n");
            wrapper.push_str(&format!(
                "    bash \"{}\" &\n",
                user_script_path.replace('"', "\\\"")
            ));
            wrapper.push_str("  fi\n");
            wrapper.push_str("done\nwait\n");
            wrapper
        } else {
            launch_script
        };

        let (alloc_result, allocated_device_ids) = self
            .allocate_local_resources(&spec, req.allocated.as_ref())
            .await?;

        let (host_device_plan, container_device_plan) = {
            let reg = self.device_registry.lock().await;
            reg.build_job_injection_plans("gpu", &allocated_device_ids, spec.uid, spec.gid)
                .map_err(|e| {
                    error!(job_id, error = %e, "device registry resolution failed");
                    Status::failed_precondition(format!("device resolution failed: {}", e))
                })?
        };

        // Wire allocated device IDs and injection plan into container config.
        if let Some(ref mut cfg) = container_config {
            cfg.gpu_devices = allocated_device_ids.clone();
            cfg.device_plan = Some(container_device_plan);
        }

        let cpu_ids: Vec<u32> = alloc_result
            .as_ref()
            .map(|a| a.cpu_ids.clone())
            .unwrap_or_default();

        // Resolve stdout/stderr paths
        let stdout_path = if spec.stdout_path.is_empty() {
            format!("{}/spur-{}.out", work_dir, job_id)
        } else {
            spec.stdout_path.clone()
        };
        let stderr_path = if spec.stderr_path.is_empty() {
            format!("{}/spur-{}.out", work_dir, job_id)
        } else {
            spec.stderr_path.clone()
        };

        // Build container launch config if this is a containerized job
        let container_launch = if !spec.container_image.is_empty() {
            Some(executor::ContainerLaunchConfig {
                config: container_config.take().unwrap(),
                rootfs: rootfs_path.take().unwrap(),
            })
        } else {
            None
        };

        let launch_cfg = executor::JobLaunchConfig {
            job_id,
            script: launch_script,
            work_dir: work_dir.clone(),
            environment: env,
            stdout_path,
            stderr_path,
            cpus: spec.cpus_per_task.max(1),
            memory_mb: spec.memory_per_node_mb,
            gpu_devices: allocated_device_ids,
            cpu_ids,
            open_mode: if spec.open_mode.is_empty() {
                None
            } else {
                Some(spec.open_mode.clone())
            },
            uid: spec.uid,
            gid: spec.gid,
            container: container_launch,
            prolog_script: self.hooks.prolog.clone(),
            partition: spec.partition.clone(),
            nodelist: spec.nodelist.clone(),
            host_device_plan: Some(host_device_plan),
        };

        match executor::launch_job(&launch_cfg, (*self.spank).as_ref()).await {
            Ok(running_job) => {
                let mut jobs = self.running.lock().await;
                info!(job_id, gpus = ?launch_cfg.gpu_devices, "job launched successfully");
                jobs.insert(
                    job_id,
                    TrackedJob {
                        job: running_job,
                        rootfs_mode: rootfs_mode.clone(),
                        allocation: alloc_result,
                        stdout_path: launch_cfg.stdout_path,
                        stderr_path: launch_cfg.stderr_path,
                        has_pid_namespace: nix::unistd::geteuid().is_root(),
                        work_dir: launch_cfg.work_dir,
                        uid: launch_cfg.uid,
                        gid: launch_cfg.gid,
                        partition: launch_cfg.partition,
                        gpu_devices: launch_cfg.gpu_devices,
                        cpus: launch_cfg.cpus,
                        memory_mb: launch_cfg.memory_mb,
                        nodelist: launch_cfg.nodelist,
                    },
                );
                Ok(Response::new(LaunchJobResponse {
                    success: true,
                    error: String::new(),
                }))
            }
            Err(e) => {
                // Release allocation on launch failure
                if let Some(ref alloc) = alloc_result {
                    self.allocation.lock().await.release(alloc);
                }

                let is_prolog_failure = matches!(e, executor::LaunchError::PrologFailed(_));
                let err_msg = e.to_string();
                error!(job_id, error = %err_msg, "failed to launch job");

                if is_prolog_failure {
                    let controller = self.reporter.controller_addr.clone();
                    let node_name = self.reporter.hostname.clone();
                    let drain_reason = format!("prolog failed: {}", err_msg);
                    tokio::spawn(async move {
                        let drain = DrainRequest {
                            reason: drain_reason.clone(),
                        };
                        report_completion(&controller, job_id, -1, 0, &node_name, Some(&drain))
                            .await;
                    });
                }

                Ok(Response::new(LaunchJobResponse {
                    success: false,
                    error: err_msg,
                }))
            }
        }
    }

    async fn cancel_job(
        &self,
        request: Request<AgentCancelJobRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id;

        if req.signal > 0 {
            self.send_explicit_signal(job_id, req.signal).await;
        } else {
            self.graceful_cancel(job_id).await;
        }

        Ok(Response::new(()))
    }

    async fn suspend_job(
        &self,
        request: Request<AgentSuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.suspend_signal(req.job_id, req.resume).await;
        Ok(Response::new(()))
    }

    async fn get_node_resources(
        &self,
        _request: Request<()>,
    ) -> Result<Response<NodeResourcesResponse>, Status> {
        let resources = &self.reporter.resources;
        Ok(Response::new(NodeResourcesResponse {
            total: Some(crate::reporter::resource_to_proto(resources)),
            used: Some(crate::reporter::allocations_to_proto(
                &spur_core::resource::ResourceAllocations::default(),
            )),
        }))
    }

    async fn exec_in_job(
        &self,
        request: Request<ExecInJobRequest>,
    ) -> Result<Response<ExecInJobResponse>, Status> {
        let req = request.into_inner();

        let (pid, has_pid_ns) = {
            let jobs = self.running.lock().await;
            let tracked = jobs.get(&req.job_id).ok_or_else(|| {
                Status::not_found(format!("job {} not running on this node", req.job_id))
            })?;
            let pid = tracked.job.pid().ok_or_else(|| {
                Status::failed_precondition(format!("job {} has no tracked PID", req.job_id))
            })?;
            (pid, tracked.has_pid_namespace)
        };

        if req.command.is_empty() {
            return Err(Status::invalid_argument("no command specified"));
        }

        info!(
            job_id = req.job_id,
            pid,
            command = ?req.command,
            "exec into running job"
        );

        // Use nsenter to enter the job's namespace(s) and run the command
        let mut cmd = tokio::process::Command::new("nsenter");
        cmd.arg("--target").arg(pid.to_string()).arg("--mount");
        if has_pid_ns {
            cmd.arg("--pid");
        }
        cmd.arg("--");
        cmd.arg(&req.command[0]);
        for arg in &req.command[1..] {
            cmd.arg(arg);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| Status::internal(format!("nsenter failed: {}", e)))?;

        Ok(Response::new(ExecInJobResponse {
            success: output.status.success(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }

    /// #146: run a one-shot command on this node, used by `srun` inside an
    /// `salloc` interactive shell. Unlike ExecInJob, this does not require
    /// a tracked job process — salloc allocations don't run anything until
    /// `srun` dispatches a step.
    async fn run_command(
        &self,
        request: Request<RunCommandRequest>,
    ) -> Result<Response<RunCommandResponse>, Status> {
        let req = request.into_inner();
        if req.command.is_empty() {
            return Err(Status::invalid_argument("no command specified"));
        }

        let work_dir = if req.work_dir.is_empty() {
            "/tmp".to_string()
        } else {
            req.work_dir
        };

        let job_id = req.job_id;
        if job_id == 0 {
            return Err(Status::invalid_argument("job_id is required"));
        }

        let (gpu_devices, partition, cpus, memory_mb, nodelist) = {
            let jobs = self.running.lock().await;
            let tracked = jobs.get(&job_id).ok_or_else(|| {
                Status::not_found(format!("job {} not running on this node", job_id))
            })?;
            let nodelist = if tracked.nodelist.is_empty() {
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into())
            } else {
                tracked.nodelist.clone()
            };
            (
                tracked.gpu_devices.clone(),
                tracked.partition.clone(),
                tracked.cpus,
                tracked.memory_mb,
                nodelist,
            )
        };

        let gpu_env = if gpu_devices.is_empty() {
            HashMap::new()
        } else {
            self.device_registry
                .lock()
                .await
                .build_job_injection_plans("gpu", &gpu_devices, req.uid, req.gid)
                .map_err(|e| {
                    Status::failed_precondition(format!("GPU injection plan failed: {}", e))
                })?
                .0
                .env
        };

        let mut cmd = tokio::process::Command::new(&req.command[0]);
        cmd.args(&req.command[1..]).current_dir(&work_dir);
        for (k, v) in &req.environment {
            cmd.env(k, v);
        }
        for (k, v) in &gpu_env {
            cmd.env(k, v);
        }

        // Drop privilege if requested (and we're root). Mirrors the privilege
        // drop in launch_job's non-namespace path.
        if req.uid > 0 && nix::unistd::geteuid().is_root() {
            let target_uid = req.uid;
            let target_gid = req.gid;
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::setgid(nix::unistd::Gid::from_raw(target_gid))
                        .map_err(std::io::Error::other)?;
                    nix::unistd::setuid(nix::unistd::Uid::from_raw(target_uid))
                        .map_err(std::io::Error::other)?;
                    Ok(())
                });
            }
        }

        info!(
            command = ?req.command,
            uid = req.uid,
            work_dir = %work_dir,
            "RunCommand: executing one-shot step"
        );

        // TaskProlog: run before the step command
        if let Some(ref task_prolog) = self.hooks.task_prolog {
            let ctx = spur_core::hooks::HookContext {
                job_id,
                work_dir: work_dir.clone(),
                uid: req.uid,
                gid: req.gid,
                partition: partition.clone(),
                nodelist: nodelist.clone(),
                script_context: "prolog_task".into(),
                gpu_devices: gpu_devices.clone(),
                cpus,
                memory_mb,
            };
            if let Err(e) = spur_core::hooks::run_hook(task_prolog, &ctx).await {
                return Err(Status::aborted(format!("TaskProlog failed: {}", e)));
            }
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| Status::internal(format!("command failed: {}", e)))?;

        // TaskEpilog: run after the step command (failure logged, not fatal)
        if let Some(ref task_epilog) = self.hooks.task_epilog {
            let ctx = spur_core::hooks::HookContext {
                job_id,
                work_dir: work_dir.clone(),
                uid: req.uid,
                gid: req.gid,
                partition,
                nodelist,
                script_context: "epilog_task".into(),
                gpu_devices,
                cpus,
                memory_mb,
            };
            if let Err(e) = spur_core::hooks::run_hook(task_epilog, &ctx).await {
                warn!(error = %e, "TaskEpilog failed");
            }
        }

        Ok(Response::new(RunCommandResponse {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }

    async fn stream_job_output(
        &self,
        request: Request<StreamJobOutputRequest>,
    ) -> Result<Response<Self::StreamJobOutputStream>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id;

        // Look up the output file path from the tracked job
        let file_path = {
            let jobs = self.running.lock().await;
            match jobs.get(&job_id) {
                Some(tracked) => {
                    if req.stream == "stderr" {
                        tracked.stderr_path.clone()
                    } else {
                        tracked.stdout_path.clone()
                    }
                }
                None => {
                    return Err(Status::not_found(format!(
                        "job {} not running on this node",
                        job_id
                    )));
                }
            }
        };

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let running = self.running.clone();

        tokio::spawn(async move {
            // Wait for the output file to appear
            let mut waited = 0;
            while !std::path::Path::new(&file_path).exists() && waited < 30 {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                waited += 1;
            }

            let mut offset = 0u64;
            loop {
                // Read new data from the file
                if let Ok(data) = tokio::fs::read(&file_path).await {
                    if data.len() as u64 > offset {
                        let new_data = data[offset as usize..].to_vec();
                        offset = data.len() as u64;
                        if tx
                            .send(Ok(StreamJobOutputChunk {
                                data: new_data,
                                eof: false,
                            }))
                            .await
                            .is_err()
                        {
                            break; // Client disconnected
                        }
                    }
                }

                // Check if job is still running
                let still_running = running.lock().await.contains_key(&job_id);
                if !still_running {
                    // Final read to get any remaining output
                    if let Ok(data) = tokio::fs::read(&file_path).await {
                        if data.len() as u64 > offset {
                            let _ = tx
                                .send(Ok(StreamJobOutputChunk {
                                    data: data[offset as usize..].to_vec(),
                                    eof: false,
                                }))
                                .await;
                        }
                    }
                    let _ = tx
                        .send(Ok(StreamJobOutputChunk {
                            data: Vec::new(),
                            eof: true,
                        }))
                        .await;
                    break;
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn attach_job(
        &self,
        request: Request<tonic::Streaming<AttachJobInput>>,
    ) -> Result<Response<Self::AttachJobStream>, Status> {
        let mut in_stream = request.into_inner();

        // Read the first message to get the job_id
        let first_msg = in_stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("failed to read first message: {}", e)))?
            .ok_or_else(|| {
                Status::invalid_argument("empty stream — expected job_id in first message")
            })?;

        let job_id = first_msg.job_id;

        // Check the job is running and get its PID for namespace entry
        let (pid, env_vars) = {
            let jobs = self.running.lock().await;
            match jobs.get(&job_id) {
                Some(tracked) => {
                    let pid = tracked.job.pid().ok_or_else(|| {
                        Status::failed_precondition(format!("job {} has no PID", job_id))
                    })?;
                    // Read a few env vars from /proc to replicate the job's environment
                    let env = Self::read_proc_env(pid);
                    (pid, env)
                }
                None => {
                    return Err(Status::not_found(format!(
                        "job {} not running on this node",
                        job_id
                    )));
                }
            }
        };

        // Issue #54: Use a larger buffer to prevent deadlock when stdout+stderr
        // produce high-volume output concurrently.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AttachJobOutput, Status>>(256);

        tokio::spawn(async move {
            // Spawn an interactive shell inside the job's cgroup/namespace
            use std::process::Stdio;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::process::Command;

            // Use nsenter to enter the job process's namespaces if possible,
            // otherwise just spawn a shell with the same environment.
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-i")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            // Set the job's environment variables
            for (k, v) in &env_vars {
                cmd.env(k, v);
            }
            cmd.env("SPUR_JOB_ID", job_id.to_string());

            // Try nsenter for namespace isolation (if running as root)
            let mut child = if nix::unistd::geteuid().is_root() {
                let mut ns_cmd = Command::new("nsenter");
                ns_cmd
                    .args(["-t", &pid.to_string(), "--mount", "--pid", "--"])
                    .args(["/bin/sh", "-i"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                for (k, v) in &env_vars {
                    ns_cmd.env(k, v);
                }
                ns_cmd.env("SPUR_JOB_ID", job_id.to_string());
                match ns_cmd.spawn() {
                    Ok(c) => c,
                    Err(_) => match cmd.spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx
                                .send(Err(Status::internal(format!(
                                    "failed to spawn shell: {}",
                                    e
                                ))))
                                .await;
                            return;
                        }
                    },
                }
            } else {
                match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "failed to spawn shell: {}",
                                e
                            ))))
                            .await;
                        return;
                    }
                }
            };

            let mut child_stdin = child.stdin.take().unwrap();
            let mut child_stdout = child.stdout.take().unwrap();
            let mut child_stderr = child.stderr.take().unwrap();

            // Forward initial data from first message (if any)
            if !first_msg.data.is_empty() {
                let _ = child_stdin.write_all(&first_msg.data).await;
            }

            let tx_clone = tx.clone();

            // Task: read from client stream → child stdin
            let stdin_task = tokio::spawn(async move {
                while let Ok(Some(msg)) = in_stream.message().await {
                    if !msg.data.is_empty() && child_stdin.write_all(&msg.data).await.is_err() {
                        break;
                    }
                }
                drop(child_stdin); // EOF to child
            });

            // Task: read child stderr → merge into output
            let tx_stderr = tx.clone();
            let stderr_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match child_stderr.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx_stderr
                                .send(Ok(AttachJobOutput {
                                    data: buf[..n].to_vec(),
                                    eof: false,
                                }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            // Main: read child stdout → output stream
            let mut buf = vec![0u8; 4096];
            loop {
                match child_stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx_clone
                            .send(Ok(AttachJobOutput {
                                data: buf[..n].to_vec(),
                                eof: false,
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            // Wait for child to exit, then let I/O tasks drain gracefully
            // before sending EOF. Aborting immediately loses buffered data
            // (issue #54).
            let _ = child.wait().await;
            // Give tasks a moment to flush remaining data
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let _ = stderr_task.await;
            })
            .await;
            stdin_task.abort();

            // Send EOF
            let _ = tx_clone
                .send(Ok(AttachJobOutput {
                    data: Vec::new(),
                    eof: true,
                }))
                .await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

impl AgentService {
    /// Record controller-allocated GPUs and allocate local CPU/memory resources.
    async fn allocate_local_resources(
        &self,
        spec: &JobSpec,
        allocated: Option<&ResourceAllocations>,
    ) -> Result<(Option<AllocationResult>, Vec<u32>), Status> {
        let controller_gpu_ids: Vec<u32> = allocated
            .and_then(|a| a.devices.get("gpu"))
            .map(|d| d.devices.iter().map(|dev| dev.device_id).collect())
            .unwrap_or_default();

        let (gres_gpu_count, gres_gpu_type) = Self::parse_gpu_gres(&spec.gres);

        if controller_gpu_ids.is_empty() && gres_gpu_count > 0 {
            return Err(Status::internal(format!(
                "job requests {} GPUs (type: {}) but controller sent no device IDs",
                gres_gpu_count,
                gres_gpu_type.as_deref().unwrap_or("any"),
            )));
        }

        let mut alloc = self.allocation.lock().await;

        if !controller_gpu_ids.is_empty() && !alloc.record_gpus(&controller_gpu_ids) {
            return Err(Status::resource_exhausted(
                "controller-allocated GPUs unavailable on this node",
            ));
        }

        let cpu_alloc = if spec.cpus_per_task > 0 {
            alloc.try_allocate(spec.cpus_per_task.max(1), spec.memory_per_node_mb, 0, None)
        } else {
            None
        };

        let gpu_ids = controller_gpu_ids.clone();
        let result = AllocationResult {
            cpu_ids: cpu_alloc.map(|a| a.cpu_ids).unwrap_or_default(),
            gpu_ids: controller_gpu_ids,
            memory_mb: spec.memory_per_node_mb,
        };

        Ok((Some(result), gpu_ids))
    }

    fn parse_gpu_gres(gres: &[String]) -> (u32, Option<String>) {
        let mut count = 0;
        let mut gpu_type = None;
        for g in gres {
            if let Some((name, gtype, n)) = spur_core::resource::parse_gres(g) {
                if name == "gpu" {
                    count += n;
                    if gtype.is_some() {
                        gpu_type = gtype;
                    }
                }
            }
        }
        (count, gpu_type)
    }

    /// Send a user-specified signal to a running job.
    async fn send_explicit_signal(&self, job_id: u32, signal: i32) {
        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
        let sig =
            nix::sys::signal::Signal::try_from(signal).unwrap_or(nix::sys::signal::Signal::SIGTERM);
        info!(job_id, signal, "sending explicit signal to job");
        let _ = tracked.job.kill_signal(sig);
    }

    /// Freeze (SIGSTOP) or thaw (SIGCONT) a running job's process(es).
    async fn suspend_signal(&self, job_id: u32, resume: bool) {
        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
        let sig = if resume {
            nix::sys::signal::Signal::SIGCONT
        } else {
            nix::sys::signal::Signal::SIGSTOP
        };
        info!(job_id, resume, "sending suspend/resume signal to job");
        let _ = tracked.job.kill_signal(sig);
    }

    /// SIGTERM now, escalate to SIGKILL after a 5-second grace period.
    async fn graceful_cancel(&self, job_id: u32) {
        {
            let jobs = self.running.lock().await;
            let Some(tracked) = jobs.get(&job_id) else {
                return;
            };
            info!(job_id, "graceful cancel: SIGTERM → 5s grace → SIGKILL");
            let _ = tracked.job.kill_signal(nix::sys::signal::Signal::SIGTERM);
        }

        let running = self.running.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let jobs = running.lock().await;
            if let Some(tracked) = jobs.get(&job_id) {
                info!(job_id, "grace period expired, sending SIGKILL");
                let _ = tracked.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                // Job stays in `running` and monitor loop reaps it and does full cleanup.
            }
        });
    }

    /// Read environment variables from a running process via /proc.
    fn read_proc_env(pid: u32) -> Vec<(String, String)> {
        let path = format!("/proc/{}/environ", pid);
        match std::fs::read(&path) {
            Ok(data) => data
                .split(|&b| b == 0)
                .filter_map(|entry| {
                    let s = std::str::from_utf8(entry).ok()?;
                    let (k, v) = s.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
impl TrackedJob {
    fn dummy(_pid: u32) -> Self {
        let child = tokio::process::Command::new("sleep")
            .arg("3600")
            .spawn()
            .expect("failed to spawn dummy process");
        Self {
            job: executor::RunningJob::Managed {
                child,
                cgroup_path: None,
            },
            rootfs_mode: crate::container::RootfsMode::Extracted,
            allocation: None,
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            has_pid_namespace: false,
            work_dir: "/tmp".into(),
            uid: 0,
            gid: 0,
            partition: String::new(),
            gpu_devices: Vec::new(),
            cpus: 1,
            memory_mb: 0,
            nodelist: String::new(),
        }
    }
}

#[cfg(test)]
impl AgentService {
    async fn insert_test_job(&self, job_id: u32, job: TrackedJob) {
        self.running.lock().await.insert(job_id, job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::resource::ResourceSet;
    use tonic::Request;

    async fn run_command_test_setup() -> (AgentService, u32) {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        let job_id = 100;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;
        (svc, job_id)
    }

    fn test_gpu_registry() -> DeviceRegistry {
        use spur_devices::cdi::cache::CdiCache;
        use spur_devices::{GresCache, GresEntry};

        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            file: Some("/dev/dri/renderD[128-129]".into()),
            count: Some(2),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }];
        let gres_cache = GresCache::from_entries(&gres);
        let mut reg = DeviceRegistry::new();
        reg.populate(&CdiCache::new(), &gres_cache);
        reg
    }

    fn test_reporter() -> Arc<NodeReporter> {
        Arc::new(NodeReporter::new(
            "test-node".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "test-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
        ))
    }

    #[tokio::test]
    async fn exec_in_job_returns_without_deadlock() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        let pid = std::process::id();
        svc.insert_test_job(42, TrackedJob::dummy(pid)).await;

        let req = Request::new(ExecInJobRequest {
            job_id: 42,
            command: vec!["echo".into(), "hello".into()],
        });

        let result = svc.exec_in_job(req).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn exec_in_job_not_found() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );

        let req = Request::new(ExecInJobRequest {
            job_id: 999,
            command: vec!["echo".into()],
        });

        let err = svc.exec_in_job(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // --- #146: srun-in-salloc step dispatch via RunCommand ---
    //
    // Regression: srun's run_as_step previously called
    //   tokio::process::Command::new(args.command[0]).status()
    // which executed the command on whichever host the user had typed
    // srun on (the controller / submit host), not on the allocated
    // compute node. After the fix, srun calls the controller's RunStep
    // RPC, which forwards to the allocated agent's RunCommand.
    //
    // These tests cover the agent-side RunCommand handler. The controller
    // routing is glue (~50 lines) that mirrors exec_in_job's pattern.

    #[tokio::test]
    async fn run_command_executes_simple_command() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hello-from-agent".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "hello-from-agent");
        assert!(resp.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_command_propagates_nonzero_exit_code() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["false".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 1, "false exits 1");
    }

    #[tokio::test]
    async fn run_command_passes_environment() {
        let (svc, job_id) = run_command_test_setup().await;
        let mut env = HashMap::new();
        env.insert("SPUR_TEST_VAR".into(), "step-dispatched".into());
        let req = Request::new(RunCommandRequest {
            command: vec!["/bin/sh".into(), "-c".into(), "echo $SPUR_TEST_VAR".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: env,
            job_id,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "step-dispatched");
    }

    #[tokio::test]
    async fn run_command_empty_command_is_rejected() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        let req = Request::new(RunCommandRequest {
            command: vec![],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 0,
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn run_command_requires_job_id() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hi".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 0,
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn run_command_not_found_without_tracked_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hi".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 999,
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn run_command_uses_provided_work_dir() {
        // The bug repro: the user's workflow is `salloc; srun hostname`.
        // hostname runs in whatever cwd the agent picks; we can't easily
        // assert it's a specific directory without mounting a tempdir as
        // the agent's cwd. Instead use `pwd` and assert it matches the
        // dir we passed.
        let (svc, job_id) = run_command_test_setup().await;
        let tmp = std::env::temp_dir();
        // Resolve symlinks (e.g., macOS /tmp -> /private/tmp).
        let tmp_canonical = std::fs::canonicalize(&tmp).unwrap_or(tmp.clone());
        let req = Request::new(RunCommandRequest {
            command: vec!["pwd".into()],
            uid: 0,
            gid: 0,
            work_dir: tmp_canonical.to_string_lossy().into_owned(),
            environment: HashMap::new(),
            job_id,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        let observed_canonical = std::fs::canonicalize(resp.stdout.trim()).unwrap();
        assert_eq!(observed_canonical, tmp_canonical);
    }

    #[tokio::test]
    async fn run_command_injects_gpu_env_from_tracked_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(test_gpu_registry())),
        );

        let job_id = 700;
        let mut tracked = TrackedJob::dummy(0);
        tracked.gpu_devices = vec![0, 1];
        tracked.partition = "gpu".into();
        tracked.cpus = 8;
        tracked.memory_mb = 16384;
        svc.insert_test_job(job_id, tracked).await;

        let req = Request::new(RunCommandRequest {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo ROCR=$ROCR_VISIBLE_DEVICES CUDA=$CUDA_VISIBLE_DEVICES".into(),
            ],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert!(
            resp.stdout.contains("ROCR=0,1"),
            "expected ROCR_VISIBLE_DEVICES=0,1 in stdout, got: {}",
            resp.stdout
        );
        assert!(
            !resp.stdout.contains("CUDA=0,1"),
            "AMD registry should not set CUDA_VISIBLE_DEVICES, got: {}",
            resp.stdout
        );
    }

    /// Helper: poll until the job is removed from `running` (by the monitor).
    async fn wait_job_reaped(svc: &AgentService, job_id: u32, timeout_ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if svc.running.lock().await.get(&job_id).is_none() {
                return true;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
        false
    }

    #[tokio::test]
    async fn graceful_cancel_sigterm_responsive() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 900;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;

        svc.graceful_cancel(job_id).await;

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGTERM-killed job within 5s"
        );
    }

    #[tokio::test]
    async fn graceful_cancel_escalates_to_sigkill() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 901;
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while true; do sleep 1; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn SIGTERM-trapping process");
        let tracked = TrackedJob {
            job: executor::RunningJob::Managed {
                child,
                cgroup_path: None,
            },
            rootfs_mode: crate::container::RootfsMode::Extracted,
            allocation: None,
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            has_pid_namespace: false,
            work_dir: "/tmp".into(),
            uid: 0,
            gid: 0,
            partition: String::new(),
            gpu_devices: Vec::new(),
            cpus: 1,
            memory_mb: 0,
            nodelist: String::new(),
        };
        svc.insert_test_job(job_id, tracked).await;

        svc.graceful_cancel(job_id).await;

        // 5s grace + up to 2s monitor tick + buffer
        assert!(
            wait_job_reaped(&svc, job_id, 10_000).await,
            "monitor should reap job after SIGKILL escalation"
        );
    }

    fn proc_state(pid: i32) -> char {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after = stat.rsplit(')').next().unwrap();
        after
            .split_whitespace()
            .next()
            .unwrap()
            .chars()
            .next()
            .unwrap()
    }

    /// Poll the process state until it matches `want` (or any char in it), up to ~2s.
    async fn await_proc_state(pid: i32, want: &[char]) -> char {
        for _ in 0..200 {
            let s = proc_state(pid);
            if want.contains(&s) {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        proc_state(pid)
    }

    #[tokio::test]
    async fn suspend_then_resume_toggles_process_state() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 903;
        let tracked = TrackedJob::dummy(0);
        let pid = tracked.job.pid().expect("dummy child should have a pid") as i32;
        svc.insert_test_job(job_id, tracked).await;

        svc.suspend_signal(job_id, false).await; // SIGSTOP
        assert_eq!(
            await_proc_state(pid, &['T']).await,
            'T',
            "process should be stopped after SIGSTOP"
        );

        svc.suspend_signal(job_id, true).await; // SIGCONT
        let state = await_proc_state(pid, &['R', 'S']).await;
        assert!(
            matches!(state, 'R' | 'S'),
            "process should run after SIGCONT, got {state}"
        );

        svc.send_explicit_signal(job_id, 9).await; // cleanup
    }

    #[tokio::test]
    async fn send_explicit_signal_kills_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 902;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;

        svc.send_explicit_signal(job_id, 9).await; // SIGKILL

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGKILL'd job within 5s"
        );
    }
}
