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
}

pub struct AgentService {
    pub reporter: Arc<NodeReporter>,
    running: Arc<Mutex<HashMap<u32, TrackedJob>>>,
    allocation: Arc<Mutex<NodeAllocation>>,
    spank: Arc<Option<SpankHost>>,
    pmi_servers: Arc<Mutex<HashMap<u32, Arc<PmiServer>>>>,
}

impl AgentService {
    pub fn new(reporter: Arc<NodeReporter>) -> Self {
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
        }
    }

    /// Spawn a background task to monitor running jobs and report completions.
    pub fn start_monitor(&self, controller_addr: String) {
        let running = self.running.clone();
        let allocation = self.allocation.clone();
        let spank = self.spank.clone();
        let pmi_servers = self.pmi_servers.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let mut jobs = running.lock().await;
                let mut completed: Vec<(
                    u32,
                    i32,
                    crate::container::RootfsMode,
                    Option<AllocationResult>,
                    Option<std::path::PathBuf>,
                )> = Vec::new();

                for (job_id, tracked) in jobs.iter_mut() {
                    match tracked.job.try_wait() {
                        Ok(Some(exit_code)) => {
                            info!(job_id, exit_code, "job finished");
                            completed.push((
                                *job_id,
                                exit_code,
                                tracked.rootfs_mode.clone(),
                                tracked.allocation.take(),
                                tracked.job.take_cgroup(),
                            ));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(job_id, error = %e, "failed to check job status");
                        }
                    }
                }

                for (job_id, _exit_code, mode, alloc, cgroup) in &completed {
                    jobs.remove(job_id);
                    crate::container::cleanup_rootfs(*job_id, mode);
                    if let Some(cgroup) = cgroup {
                        crate::executor::cleanup_cgroup(cgroup);
                    }
                    // Release GPU/CPU allocation
                    if let Some(alloc) = alloc {
                        allocation.lock().await.release(alloc);
                    }
                    // Cleanup PMI server if one was started for this job
                    if let Some(pmi) = pmi_servers.lock().await.remove(job_id) {
                        pmi.cleanup();
                    }
                }

                // Release lock BEFORE network I/O — holding the lock during
                // report_completion blocks new job launches and can lose
                // completions if the RPC times out.
                drop(jobs);

                // Invoke SPANK TaskExit and JobEpilog hooks for completed jobs
                if let Some(ref spank_host) = *spank {
                    for (job_id, _exit_code, _mode, _alloc, _cgroup) in &completed {
                        if let Err(e) = spank_host.invoke_hook(SpankHook::TaskExit) {
                            warn!(job_id, error = %e, "SPANK TaskExit hook failed");
                        }
                        if let Err(e) = spank_host.invoke_hook(SpankHook::JobEpilog) {
                            warn!(job_id, error = %e, "SPANK JobEpilog hook failed");
                        }
                    }
                }

                for (job_id, exit_code, _mode, _alloc, _cgroup) in &completed {
                    report_completion(&controller_addr, *job_id, *exit_code).await;
                }
            }
        });
    }
}

async fn report_completion(controller_addr: &str, job_id: u32, exit_code: i32) {
    use spur_proto::proto::slurm_controller_client::SlurmControllerClient;

    let state = if exit_code == 0 {
        JobState::JobCompleted as i32
    } else {
        JobState::JobFailed as i32
    };

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
                    message: format!("exit_code={}", exit_code),
                };
                match client.report_job_status(req).await {
                    Ok(_) => {
                        info!(job_id, exit_code, "reported completion to controller");
                        return;
                    }
                    Err(e) => {
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

        // Allocate GPU devices from the node's pool
        let mut gpu_count = 0u32;
        let mut gpu_type: Option<String> = None;
        for gres in &spec.gres {
            if let Some((name, gtype, count)) = spur_core::resource::parse_gres(gres) {
                if name == "gpu" {
                    gpu_count += count;
                    if let Some(t) = gtype {
                        gpu_type = Some(t);
                    }
                }
            }
        }

        let alloc_result = if gpu_count > 0 || spec.cpus_per_task > 0 {
            let mut alloc = self.allocation.lock().await;
            alloc.try_allocate(
                spec.cpus_per_task.max(1),
                spec.memory_per_node_mb,
                gpu_count,
                gpu_type.as_deref(),
            )
        } else {
            None
        };

        let gpu_devices: Vec<u32> = alloc_result
            .as_ref()
            .map(|a| a.gpu_ids.clone())
            .unwrap_or_default();

        // Wire allocated GPU IDs into container config so mount_hw_devices
        // can selectively expose only the allocated GPUs.
        if let Some(ref mut cfg) = container_config {
            cfg.gpu_devices = gpu_devices.clone();
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

        // Launch the job
        let open_mode = if spec.open_mode.is_empty() {
            None
        } else {
            Some(spec.open_mode.as_str())
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

        match executor::launch_job(
            job_id,
            &launch_script,
            &work_dir,
            &env,
            &stdout_path,
            &stderr_path,
            spec.cpus_per_task.max(1),
            spec.memory_per_node_mb,
            &gpu_devices,
            &cpu_ids,
            (*self.spank).as_ref(),
            open_mode,
            spec.uid,
            spec.gid,
            container_launch,
        )
        .await
        {
            Ok(running_job) => {
                let mut jobs = self.running.lock().await;
                jobs.insert(
                    job_id,
                    TrackedJob {
                        job: running_job,
                        rootfs_mode: rootfs_mode.clone(),
                        allocation: alloc_result,
                        stdout_path,
                        stderr_path,
                        has_pid_namespace: nix::unistd::geteuid().is_root(),
                    },
                );
                info!(job_id, gpus = ?gpu_devices, "job launched successfully");
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
                error!(job_id, error = %e, "failed to launch job");
                Ok(Response::new(LaunchJobResponse {
                    success: false,
                    error: e.to_string(),
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
        let signal = req.signal;

        {
            let jobs = self.running.lock().await;
            let Some(tracked) = jobs.get(&job_id) else {
                return Ok(Response::new(()));
            };

            let sig = if signal > 0 {
                info!(job_id, signal, "sending signal to job");
                nix::sys::signal::Signal::try_from(signal)
                    .unwrap_or(nix::sys::signal::Signal::SIGTERM)
            } else {
                info!(job_id, "graceful cancel: SIGTERM → 5s grace → SIGKILL");
                nix::sys::signal::Signal::SIGTERM
            };

            let _ = tracked.job.kill_signal(sig);
        }

        if signal == 0 {
            let running = self.running.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let mut jobs = running.lock().await;
                if let Some(tracked) = jobs.get(&job_id) {
                    info!(job_id, "grace period expired, sending SIGKILL");
                    let _ = tracked.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                    jobs.remove(&job_id);
                }
            });
        }

        Ok(Response::new(()))
    }

    async fn get_node_resources(
        &self,
        _request: Request<()>,
    ) -> Result<Response<NodeResourcesResponse>, Status> {
        let resources = &self.reporter.resources;
        Ok(Response::new(NodeResourcesResponse {
            total: Some(crate::reporter::resource_to_proto(resources)),
            used: Some(ResourceSet::default()),
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

        let mut cmd = tokio::process::Command::new(&req.command[0]);
        cmd.args(&req.command[1..]).current_dir(&work_dir);
        for (k, v) in &req.environment {
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
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                    nix::unistd::setuid(nix::unistd::Uid::from_raw(target_uid))
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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

        let output = cmd
            .output()
            .await
            .map_err(|e| Status::internal(format!("command failed: {}", e)))?;

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
                match tokio::fs::read(&file_path).await {
                    Ok(data) => {
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
                    Err(_) => {} // File not ready yet
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
                    if !msg.data.is_empty() {
                        if child_stdin.write_all(&msg.data).await.is_err() {
                            break;
                        }
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
        ))
    }

    #[tokio::test]
    async fn exec_in_job_returns_without_deadlock() {
        let svc = AgentService::new(test_reporter());
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
        let svc = AgentService::new(test_reporter());

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
        let svc = AgentService::new(test_reporter());
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hello-from-agent".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "hello-from-agent");
        assert!(resp.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_command_propagates_nonzero_exit_code() {
        let svc = AgentService::new(test_reporter());
        let req = Request::new(RunCommandRequest {
            command: vec!["false".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 1, "false exits 1");
    }

    #[tokio::test]
    async fn run_command_passes_environment() {
        let svc = AgentService::new(test_reporter());
        let mut env = HashMap::new();
        env.insert("SPUR_TEST_VAR".into(), "step-dispatched".into());
        let req = Request::new(RunCommandRequest {
            command: vec!["/bin/sh".into(), "-c".into(), "echo $SPUR_TEST_VAR".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: env,
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "step-dispatched");
    }

    #[tokio::test]
    async fn run_command_empty_command_is_rejected() {
        let svc = AgentService::new(test_reporter());
        let req = Request::new(RunCommandRequest {
            command: vec![],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn run_command_uses_provided_work_dir() {
        // The bug repro: the user's workflow is `salloc; srun hostname`.
        // hostname runs in whatever cwd the agent picks; we can't easily
        // assert it's a specific directory without mounting a tempdir as
        // the agent's cwd. Instead use `pwd` and assert it matches the
        // dir we passed.
        let svc = AgentService::new(test_reporter());
        let tmp = std::env::temp_dir();
        // Resolve symlinks (e.g., macOS /tmp -> /private/tmp).
        let tmp_canonical = std::fs::canonicalize(&tmp).unwrap_or(tmp.clone());
        let req = Request::new(RunCommandRequest {
            command: vec!["pwd".into()],
            uid: 0,
            gid: 0,
            work_dir: tmp_canonical.to_string_lossy().into_owned(),
            environment: HashMap::new(),
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        let observed_canonical = std::fs::canonicalize(resp.stdout.trim()).unwrap();
        assert_eq!(observed_canonical, tmp_canonical);
    }
}
