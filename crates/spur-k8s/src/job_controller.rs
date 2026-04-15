use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use backoff::future::retry;
use backoff::ExponentialBackoffBuilder;

use futures_util::{StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{self, finalizer, Event as FinalizerEvent};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::Client;
use tokio::sync::Mutex;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::crd::{to_core_job_spec, SpurJob, SpurJobStatus};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    CancelJobRequest, GetJobRequest, ReportJobStatusRequest, SubmitJobRequest,
};

const FINALIZER: &str = "spur.ai/cleanup";
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Other(String),
}

/// Shared state for the reconciler.
pub struct JobControllerCtx {
    pub client: Client,
    pub ctrl_client: Mutex<SlurmControllerClient<Channel>>,
    /// Track multi-pod completion: job_id → (expected_count, completed_count, any_failed)
    pub(crate) pod_tracker: Mutex<HashMap<u32, PodTracker>>,
}

pub(crate) struct PodTracker {
    expected: usize,
    completed: usize,
    failed: bool,
    exit_code: i32,
    message: String,
}

/// Reconcile a SpurJob: delegates to kube's finalizer for atomic cleanup management.
async fn reconcile(
    job: Arc<SpurJob>,
    ctx: Arc<JobControllerCtx>,
) -> Result<Action, ReconcileError> {
    let ns = job
        .metadata
        .namespace
        .clone()
        .ok_or_else(|| ReconcileError::Other("SpurJob has no namespace".into()))?;
    let api: Api<SpurJob> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, job, |event| {
        let api = api.clone();
        let ctx = ctx.clone();
        async move {
            match event {
                FinalizerEvent::Apply(job) => handle_job(job, &api, &ctx).await,
                FinalizerEvent::Cleanup(job) => handle_deletion(&job, &ctx).await,
            }
        }
    })
    .await
    .map_err(map_finalizer_err)
}

fn map_finalizer_err(e: finalizer::Error<ReconcileError>) -> ReconcileError {
    match e {
        finalizer::Error::ApplyFailed(e) => e,
        finalizer::Error::CleanupFailed(e) => e,
        finalizer::Error::AddFinalizer(e) => ReconcileError::Kube(e),
        finalizer::Error::RemoveFinalizer(e) => ReconcileError::Kube(e),
        finalizer::Error::UnnamedObject => ReconcileError::Other("unnamed SpurJob".into()),
        finalizer::Error::InvalidFinalizer => {
            ReconcileError::Other(format!("{FINALIZER} is not a valid finalizer name"))
        }
    }
}

/// Returns true if the SpurJob has not yet been submitted to spurctld.
fn should_submit(status: &SpurJobStatus) -> bool {
    status.spur_job_id.is_none()
}

/// Submit to spurctld, apply job-id label, and patch CRD status.
/// Re-reads from the API server first to guard against stale informer cache.
/// Returns `Ok(None)` if a prior reconcile already submitted.
async fn submit_to_controller(
    api: &Api<SpurJob>,
    ctx: &JobControllerCtx,
    name: &str,
    ns: &str,
    job: &SpurJob,
) -> Result<Option<u32>, ReconcileError> {
    // Fresh read from API server — informer cache may be stale after finalizer patch
    let fresh = api.get(name).await.map_err(ReconcileError::Kube)?;
    let fresh_status = fresh.status.clone().unwrap_or_default();
    if !should_submit(&fresh_status) {
        debug!(spurjob = %name, "already submitted by prior reconcile");
        return Ok(None);
    }

    let user = job
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("spur.ai/user"))
        .cloned()
        .unwrap_or_else(|| "k8s".to_string());

    let core_spec = to_core_job_spec(&job.spec, &user);
    let proto_spec = core_job_spec_to_proto(&core_spec);

    let mut ctrl = ctx.ctrl_client.lock().await;
    let job_id = match ctrl
        .submit_job(SubmitJobRequest {
            spec: Some(proto_spec),
        })
        .await
    {
        Ok(resp) => resp.into_inner().job_id,
        Err(e) => {
            error!(spurjob = %name, error = %e, "failed to submit SpurJob");
            return Err(ReconcileError::Grpc(e));
        }
    };
    drop(ctrl);

    info!(spurjob = %name, job_id, namespace = %ns, "SpurJob submitted");

    // Label first — VirtualAgent needs it for namespace resolution before dispatch.
    // Best-effort: status patch below is what prevents double-submit, so we must
    // not bail here. The poll path's ensure_job_id_label retries if this fails.
    ensure_job_id_label(&fresh, api, name, job_id).await.ok();

    let new_status = SpurJobStatus {
        state: "Pending".into(),
        spur_job_id: Some(job_id),
        ..fresh_status
    };
    patch_status(api, name, &new_status).await;

    Ok(Some(job_id))
}

/// State-machine dispatcher: submit if no job_id, otherwise poll spurctld.
async fn handle_job(
    job: Arc<SpurJob>,
    api: &Api<SpurJob>,
    ctx: &JobControllerCtx,
) -> Result<Action, ReconcileError> {
    let name = job.metadata.name.clone().unwrap_or_default();
    let ns = job
        .metadata
        .namespace
        .clone()
        .ok_or_else(|| ReconcileError::Other("SpurJob has no namespace".into()))?;
    let status = job.status.clone().unwrap_or_default();

    if is_terminal(&status.state) {
        return Ok(Action::await_change());
    }

    // Phase 1: Submit (no spur_job_id yet)
    if should_submit(&status) {
        return match submit_to_controller(api, ctx, &name, &ns, &job).await? {
            Some(_job_id) => Ok(Action::requeue(Duration::from_secs(5))),
            None => Ok(Action::requeue(Duration::from_secs(2))),
        };
    }

    // Phase 2: Poll spurctld for state changes
    let job_id = status.spur_job_id.unwrap();

    // Fallback for jobs submitted before label was set in submit path
    ensure_job_id_label(&job, api, &name, job_id).await.ok();

    let mut ctrl = ctx.ctrl_client.lock().await;

    match ctrl.get_job(GetJobRequest { job_id }).await {
        Ok(resp) => {
            let info = resp.into_inner();
            let spur_state = proto_job_state_to_string(info.state);

            if spur_state != status.state {
                info!(spurjob = %name, job_id, state = %spur_state, "SpurJob status changed");
                let mut new_status = status.clone();
                new_status.state = spur_state.clone();
                if !info.nodelist.is_empty() {
                    new_status.assigned_nodes = info
                        .nodelist
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect();
                }
                patch_status(api, &name, &new_status).await;
            }

            if is_terminal(&spur_state) {
                Ok(Action::await_change())
            } else {
                Ok(Action::requeue(Duration::from_secs(5)))
            }
        }
        Err(e) => {
            warn!(spurjob = %name, job_id, error = %e, "failed to poll job status");
            Ok(Action::requeue(Duration::from_secs(10)))
        }
    }
}

/// Handle SpurJob deletion: cancel Spur job, clean up Pods/Services.
/// kube::runtime::finalizer removes spur.ai/cleanup automatically after this returns Ok.
async fn handle_deletion(job: &SpurJob, ctx: &JobControllerCtx) -> Result<Action, ReconcileError> {
    let name = job.metadata.name.clone().unwrap_or_default();
    let ns = job
        .metadata
        .namespace
        .clone()
        .ok_or_else(|| ReconcileError::Other("SpurJob has no namespace".into()))?;
    let status = job.status.clone().unwrap_or_default();

    info!(spurjob = %name, "handling SpurJob deletion");

    // Cancel the Spur job if it has an ID and isn't terminal
    if let Some(job_id) = status.spur_job_id {
        if !is_terminal(&status.state) {
            let mut ctrl = ctx.ctrl_client.lock().await;
            let _ = ctrl
                .cancel_job(CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: String::new(),
                })
                .await;
        }

        // Delete all Pods by label
        let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
        let lp = ListParams::default().labels(&format!("spur.ai/job-id={}", job_id));
        if let Ok(pod_list) = pods.list(&lp).await {
            for pod in pod_list {
                let pod_name = pod.metadata.name.unwrap_or_default();
                let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
            }
        }

        // Delete headless Service
        let services: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
        let svc_name = format!("spur-job-{}", job_id);
        let _ = services.delete(&svc_name, &DeleteParams::default()).await;
    }

    Ok(Action::await_change())
}

fn error_policy(_job: Arc<SpurJob>, error: &ReconcileError, _ctx: Arc<JobControllerCtx>) -> Action {
    error!(error = %error, "SpurJob reconciler error");
    // Exponential backoff capped at MAX_BACKOFF_SECS
    Action::requeue(Duration::from_secs(MAX_BACKOFF_SECS))
}

/// Start the SpurJob controller and Pod watcher.
pub async fn run(
    client: Client,
    controller_addr: String,
    operator_namespace: String,
) -> anyhow::Result<()> {
    let url = if controller_addr.starts_with("http") {
        controller_addr
    } else {
        format!("http://{}", controller_addr)
    };
    let ctrl_client = SlurmControllerClient::connect(url).await?;

    let ctx = Arc::new(JobControllerCtx {
        client: client.clone(),
        ctrl_client: Mutex::new(ctrl_client),
        pod_tracker: Mutex::new(HashMap::new()),
    });

    let spurjobs: Api<SpurJob> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());

    info!(namespace = %operator_namespace, "starting SpurJob controller");

    // Clean up orphan Pods on startup
    let cleanup_client = client.clone();
    tokio::spawn(async move {
        cleanup_orphan_pods(cleanup_client).await;
    });

    // Run pod watcher for completion callbacks in background
    let pod_ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = watch_pods(pod_ctx).await {
            error!(error = %e, "pod watcher exited");
        }
    });

    Controller::new(spurjobs, WatcherConfig::default())
        .owns(
            pods,
            WatcherConfig::default().labels("spur.ai/managed-by=spur-k8s-operator"),
        )
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(o) => debug!(resource = ?o, "reconciled"),
                Err(e) => error!(error = %e, "reconcile failed"),
            }
        })
        .await;

    Ok(())
}

/// Watch Pods labeled with spur.ai/job-id and report terminal states back to spurctld.
async fn watch_pods(ctx: Arc<JobControllerCtx>) -> anyhow::Result<()> {
    let pods: Api<Pod> = Api::all(ctx.client.clone());

    let stream = kube::runtime::watcher::watcher(
        pods,
        kube::runtime::watcher::Config::default().labels("spur.ai/managed-by=spur-k8s-operator"),
    );
    let mut stream = pin!(stream);

    while let Some(event) = stream.try_next().await? {
        if let kube::runtime::watcher::Event::Apply(pod)
        | kube::runtime::watcher::Event::InitApply(pod) = event
        {
            let labels = pod.metadata.labels.as_ref();
            let job_id_str = labels
                .and_then(|l| l.get("spur.ai/job-id"))
                .cloned()
                .unwrap_or_default();
            let job_id: u32 = match job_id_str.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };

            let phase = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");

            // Extract richer status from container statuses
            let (state, exit_code, message) = match phase {
                "Succeeded" => (3, 0, String::new()), // JOB_COMPLETED
                "Failed" => extract_failure_details(&pod),
                _ => continue,
            };

            let pod_name = pod.metadata.name.clone().unwrap_or_default();

            // Count how many pods this job expects (from peer_nodes)
            // For now, report each pod completion individually.
            // Multi-pod tracking: check if all pods for this job are done.
            let should_report = {
                let mut tracker = ctx.pod_tracker.lock().await;
                let entry = tracker.entry(job_id).or_insert_with(|| {
                    // We don't know the expected count here, so we'll report
                    // on first failure or let the pod watcher handle it
                    PodTracker {
                        expected: 0, // unknown
                        completed: 0,
                        failed: false,
                        exit_code: 0,
                        message: String::new(),
                    }
                });
                entry.completed += 1;
                if state == 4 {
                    // JOB_FAILED
                    entry.failed = true;
                    entry.exit_code = exit_code;
                    entry.message = message.clone();
                    // Report immediately on first failure
                    true
                } else if entry.expected > 0 && entry.completed >= entry.expected {
                    // All pods done
                    true
                } else {
                    // For single-pod jobs or unknown expected count, report immediately
                    entry.expected == 0
                }
            };

            if should_report {
                let final_state = {
                    let tracker = ctx.pod_tracker.lock().await;
                    if let Some(t) = tracker.get(&job_id) {
                        if t.failed {
                            4 // JOB_FAILED
                        } else {
                            state
                        }
                    } else {
                        state
                    }
                };

                let final_exit_code = {
                    let tracker = ctx.pod_tracker.lock().await;
                    tracker
                        .get(&job_id)
                        .map(|t| if t.failed { t.exit_code } else { exit_code })
                        .unwrap_or(exit_code)
                };

                let final_message = {
                    let mut tracker = ctx.pod_tracker.lock().await;
                    let msg = tracker
                        .get(&job_id)
                        .map(|t| {
                            if t.failed && !t.message.is_empty() {
                                t.message.clone()
                            } else {
                                format!("Pod {} {}", pod_name, phase)
                            }
                        })
                        .unwrap_or_else(|| format!("Pod {} {}", pod_name, phase));
                    // Clean up tracker for terminal jobs
                    if final_state == 3 || final_state == 4 {
                        tracker.remove(&job_id);
                    }
                    msg
                };

                info!(job_id, pod = %pod_name, phase, "reporting Pod completion to spurctld");

                let mut ctrl = ctx.ctrl_client.lock().await;
                let req = ReportJobStatusRequest {
                    job_id,
                    state: final_state,
                    exit_code: final_exit_code,
                    message: final_message,
                };
                if let Err(e) = ctrl.report_job_status(req).await {
                    error!(job_id, error = %e, "failed to report job status");
                }
            }
        }
    }

    Ok(())
}

/// Extract failure details from a Failed pod's container statuses.
fn extract_failure_details(pod: &Pod) -> (i32, i32, String) {
    let status = match pod.status.as_ref() {
        Some(s) => s,
        None => return (4, 1, "Pod failed (no status)".into()),
    };

    if let Some(container_statuses) = &status.container_statuses {
        for cs in container_statuses {
            if let Some(state) = &cs.state {
                if let Some(terminated) = &state.terminated {
                    let exit_code = terminated.exit_code;
                    let reason = terminated.reason.clone().unwrap_or_default();
                    let message = terminated.message.clone().unwrap_or_default();

                    if reason == "OOMKilled" {
                        return (
                            4,
                            exit_code,
                            "OOMKilled: container exceeded memory limit".into(),
                        );
                    }

                    let msg = if !message.is_empty() {
                        format!("{}: {}", reason, message)
                    } else if !reason.is_empty() {
                        reason
                    } else {
                        format!("exit_code={}", exit_code)
                    };
                    return (4, exit_code, msg);
                }
                if let Some(waiting) = &state.waiting {
                    let reason = waiting.reason.clone().unwrap_or_default();
                    if reason == "ImagePullBackOff" || reason == "ErrImagePull" {
                        return (4, 1, format!("Image pull failed: {}", reason));
                    }
                }
            }
        }
    }

    (4, 1, "Pod failed".into())
}

/// Clean up orphan Pods on startup — Pods with spur labels but no matching SpurJob.
async fn cleanup_orphan_pods(client: Client) {
    let pods: Api<Pod> = Api::all(client.clone());
    let spurjobs: Api<SpurJob> = Api::all(client.clone());

    let lp = ListParams::default().labels("spur.ai/managed-by=spur-k8s-operator");
    let pod_list = match pods.list(&lp).await {
        Ok(list) => list,
        Err(e) => {
            warn!(error = %e, "failed to list pods for orphan cleanup");
            return;
        }
    };

    let job_list = match spurjobs.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(e) => {
            warn!(error = %e, "failed to list SpurJobs for orphan cleanup");
            return;
        }
    };

    let active_job_ids: std::collections::HashSet<String> = job_list
        .iter()
        .filter_map(|j| {
            j.status
                .as_ref()
                .and_then(|s| s.spur_job_id)
                .map(|id| id.to_string())
        })
        .collect();

    for pod in pod_list {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let pod_ns = match pod.metadata.namespace.as_deref() {
            Some(ns) => ns.to_string(),
            None => continue,
        };
        let job_id = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("spur.ai/job-id"))
            .cloned()
            .unwrap_or_default();

        if !job_id.is_empty() && !active_job_ids.contains(&job_id) {
            // Check if pod is in terminal state
            let phase = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");

            if phase == "Succeeded" || phase == "Failed" {
                info!(pod = %pod_name, namespace = %pod_ns, job_id, "cleaning up orphan Pod");
                let ns_api: Api<Pod> = Api::namespaced(client.clone(), &pod_ns);
                let _ = ns_api.delete(&pod_name, &DeleteParams::default()).await;
            }
        }
    }
}

fn has_job_id_label(job: &SpurJob) -> bool {
    job.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("spur.ai/job-id"))
        .is_some()
}

/// Ensure `spur.ai/job-id` is set on the SpurJob, retrying on transient API errors.
/// Returns Ok if the label is already present or was applied successfully.
async fn ensure_job_id_label(
    job: &SpurJob,
    api: &Api<SpurJob>,
    name: &str,
    job_id: u32,
) -> Result<(), kube::Error> {
    if has_job_id_label(job) {
        return Ok(());
    }

    let patch = serde_json::json!({
        "metadata": { "labels": { "spur.ai/job-id": job_id.to_string() } }
    });

    let backoff = ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(200))
        .with_max_elapsed_time(Some(Duration::from_secs(3)))
        .build();

    retry(backoff, || async {
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map(|_| ())
            .map_err(backoff::Error::transient)
    })
    .await
    .inspect(|_| info!(spurjob = %name, job_id, "applied job-id label"))
    .inspect_err(|e| warn!(spurjob = %name, job_id, error = %e, "failed to apply job-id label"))
}

async fn patch_status(api: &Api<SpurJob>, name: &str, status: &SpurJobStatus) {
    let patch = serde_json::json!({ "status": status });
    let pp = PatchParams::apply("spur-k8s-operator");
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        error!(spurjob = %name, error = %e, "failed to patch SpurJob status");
    }
}

fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "Completed" | "Failed" | "Cancelled" | "Timeout" | "NodeFail"
    )
}

fn proto_job_state_to_string(state: i32) -> String {
    match state {
        0 => "Pending",
        1 => "Running",
        2 => "Completing",
        3 => "Completed",
        4 => "Failed",
        5 => "Cancelled",
        6 => "Timeout",
        7 => "NodeFail",
        8 => "Preempted",
        9 => "Suspended",
        _ => "Unknown",
    }
    .to_string()
}

/// Convert a core JobSpec into proto JobSpec for gRPC submission.
fn core_job_spec_to_proto(spec: &spur_core::job::JobSpec) -> spur_proto::proto::JobSpec {
    spur_proto::proto::JobSpec {
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
        time_min: spec.time_min.map(|d| prost_types::Duration {
            seconds: d.num_seconds(),
            nanos: 0,
        }),
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
        mail_type: Vec::new(),
        mail_user: String::new(),
        interactive: false,
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
        open_mode: spec.open_mode.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // --- proto_job_state_to_string ---

    #[test]
    fn test_proto_job_state_to_string() {
        assert_eq!(proto_job_state_to_string(0), "Pending");
        assert_eq!(proto_job_state_to_string(1), "Running");
        assert_eq!(proto_job_state_to_string(3), "Completed");
        assert_eq!(proto_job_state_to_string(4), "Failed");
        assert_eq!(proto_job_state_to_string(99), "Unknown");
    }

    #[test]
    fn test_proto_job_state_all_values() {
        assert_eq!(proto_job_state_to_string(2), "Completing");
        assert_eq!(proto_job_state_to_string(5), "Cancelled");
        assert_eq!(proto_job_state_to_string(6), "Timeout");
        assert_eq!(proto_job_state_to_string(7), "NodeFail");
        assert_eq!(proto_job_state_to_string(8), "Preempted");
        assert_eq!(proto_job_state_to_string(9), "Suspended");
    }

    #[test]
    fn test_proto_job_state_negative() {
        assert_eq!(proto_job_state_to_string(-1), "Unknown");
    }

    // --- is_terminal ---

    #[test]
    fn test_is_terminal() {
        assert!(is_terminal("Completed"));
        assert!(is_terminal("Failed"));
        assert!(is_terminal("Cancelled"));
        assert!(!is_terminal("Running"));
        assert!(!is_terminal("Pending"));
    }

    #[test]
    fn test_is_terminal_timeout() {
        assert!(is_terminal("Timeout"));
    }

    #[test]
    fn test_is_terminal_nodefail() {
        assert!(is_terminal("NodeFail"));
    }

    #[test]
    fn test_is_terminal_non_terminal_states() {
        assert!(!is_terminal("Completing"));
        assert!(!is_terminal("Preempted"));
        assert!(!is_terminal("Suspended"));
        assert!(!is_terminal("Unknown"));
        assert!(!is_terminal(""));
    }

    // --- extract_failure_details ---

    #[test]
    fn test_extract_failure_details_oom() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 137,
                            reason: Some("OOMKilled".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 137);
        assert!(message.contains("OOMKilled"));
    }

    #[test]
    fn test_extract_failure_details_exit_code_nonzero() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 42,
                            reason: None,
                            message: None,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 42);
        assert!(message.contains("exit_code=42"));
    }

    #[test]
    fn test_extract_failure_details_with_reason_and_message() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 1,
                            reason: Some("Error".into()),
                            message: Some("segfault in main".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 1);
        assert_eq!(message, "Error: segfault in main");
    }

    #[test]
    fn test_extract_failure_details_reason_only() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 2,
                            reason: Some("DeadlineExceeded".into()),
                            message: None,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (_, _, message) = extract_failure_details(&pod);
        assert_eq!(message, "DeadlineExceeded");
    }

    #[test]
    fn test_extract_failure_details_image_pull_backoff() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("ImagePullBackOff".into()),
                            message: Some("Back-off pulling image".into()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 1);
        assert!(message.contains("ImagePullBackOff"));
    }

    #[test]
    fn test_extract_failure_details_err_image_pull() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "spur-job".into(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("ErrImagePull".into()),
                            message: None,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };

        let (state, _, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert!(message.contains("ErrImagePull"));
    }

    #[test]
    fn test_extract_failure_details_no_status() {
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: None,
        };
        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 1);
        assert_eq!(message, "Pod failed (no status)");
    }

    #[test]
    fn test_extract_failure_details_no_container_statuses() {
        use k8s_openapi::api::core::v1::PodStatus;
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: None,
                ..Default::default()
            }),
        };
        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 1);
        assert_eq!(message, "Pod failed");
    }

    #[test]
    fn test_extract_failure_details_empty_container_statuses() {
        use k8s_openapi::api::core::v1::PodStatus;
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Failed".into()),
                container_statuses: Some(vec![]),
                ..Default::default()
            }),
        };
        let (state, exit_code, message) = extract_failure_details(&pod);
        assert_eq!(state, 4);
        assert_eq!(exit_code, 1);
        assert_eq!(message, "Pod failed");
    }

    // --- core_job_spec_to_proto ---

    #[test]
    fn test_core_job_spec_to_proto_basic() {
        let spec = spur_core::job::JobSpec {
            name: "test-job".into(),
            user: "alice".into(),
            num_nodes: 2,
            num_tasks: 4,
            cpus_per_task: 8,
            ..Default::default()
        };
        let proto = core_job_spec_to_proto(&spec);
        assert_eq!(proto.name, "test-job");
        assert_eq!(proto.user, "alice");
        assert_eq!(proto.num_nodes, 2);
        assert_eq!(proto.num_tasks, 4);
        assert_eq!(proto.cpus_per_task, 8);
    }

    #[test]
    fn test_core_job_spec_to_proto_optional_fields() {
        let spec = spur_core::job::JobSpec {
            name: "with-opts".into(),
            partition: Some("gpu".into()),
            account: Some("research".into()),
            qos: Some("high".into()),
            priority: Some(100),
            ..Default::default()
        };
        let proto = core_job_spec_to_proto(&spec);
        assert_eq!(proto.partition, "gpu");
        assert_eq!(proto.account, "research");
        assert_eq!(proto.qos, "high");
        assert_eq!(proto.priority, 100);
    }

    #[test]
    fn test_core_job_spec_to_proto_none_fields_default() {
        let spec = spur_core::job::JobSpec::default();
        let proto = core_job_spec_to_proto(&spec);
        assert_eq!(proto.partition, "");
        assert_eq!(proto.account, "");
        assert_eq!(proto.qos, "");
        assert_eq!(proto.priority, 0);
        assert!(proto.time_limit.is_none());
    }

    #[test]
    fn test_core_job_spec_to_proto_container_fields() {
        let spec = spur_core::job::JobSpec {
            container_image: Some("pytorch:latest".into()),
            container_mounts: vec!["/data:/data:ro".into()],
            container_mount_home: true,
            container_readonly: true,
            ..Default::default()
        };
        let proto = core_job_spec_to_proto(&spec);
        assert_eq!(proto.container_image, "pytorch:latest");
        assert_eq!(proto.container_mounts, vec!["/data:/data:ro"]);
        assert!(proto.container_mount_home);
        assert!(proto.container_readonly);
    }

    #[test]
    fn test_core_job_spec_to_proto_time_limit() {
        let spec = spur_core::job::JobSpec {
            time_limit: Some(chrono::Duration::seconds(7200)),
            ..Default::default()
        };
        let proto = core_job_spec_to_proto(&spec);
        let tl = proto.time_limit.unwrap();
        assert_eq!(tl.seconds, 7200);
        assert_eq!(tl.nanos, 0);
    }

    #[test]
    fn test_core_job_spec_to_proto_gres_and_deps() {
        let spec = spur_core::job::JobSpec {
            gres: vec!["gpu:mi300x:8".into()],
            dependency: vec!["afterok:42".into()],
            array_spec: Some("0-99%10".into()),
            ..Default::default()
        };
        let proto = core_job_spec_to_proto(&spec);
        assert_eq!(proto.gres, vec!["gpu:mi300x:8"]);
        assert_eq!(proto.dependency, vec!["afterok:42"]);
        assert_eq!(proto.array_spec, "0-99%10");
    }

    // --- map_finalizer_err ---

    #[test]
    fn test_map_finalizer_err_unnamed_object() {
        let err = map_finalizer_err(finalizer::Error::UnnamedObject);
        assert!(
            matches!(&err, ReconcileError::Other(msg) if msg == "unnamed SpurJob"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn test_map_finalizer_err_invalid_finalizer_contains_name() {
        let err = map_finalizer_err(finalizer::Error::InvalidFinalizer);
        let ReconcileError::Other(msg) = &err else {
            panic!("expected Other, got {err:?}");
        };
        assert!(
            msg.contains(FINALIZER),
            "message should name the finalizer: {msg}"
        );
        assert!(
            msg.contains("not a valid"),
            "message should say it's invalid: {msg}"
        );
    }

    #[test]
    fn test_map_finalizer_err_apply_failed_is_passthrough() {
        let inner = ReconcileError::Other("apply failure".into());
        let err = map_finalizer_err(finalizer::Error::ApplyFailed(inner));
        assert!(matches!(&err, ReconcileError::Other(msg) if msg == "apply failure"));
    }

    #[test]
    fn test_map_finalizer_err_cleanup_failed_is_passthrough() {
        let inner = ReconcileError::Other("cleanup failure".into());
        let err = map_finalizer_err(finalizer::Error::CleanupFailed(inner));
        assert!(matches!(&err, ReconcileError::Other(msg) if msg == "cleanup failure"));
    }

    // --- has_job_id_label ---

    fn make_spurjob(labels: Option<BTreeMap<String, String>>, namespace: Option<&str>) -> SpurJob {
        SpurJob {
            metadata: kube::api::ObjectMeta {
                name: Some("test-job".into()),
                namespace: namespace.map(String::from),
                labels: labels,
                ..Default::default()
            },
            spec: crate::crd::SpurJobSpec {
                name: "test".into(),
                image: "test:latest".into(),
                gpus: Default::default(),
                num_nodes: 1,
                tasks_per_node: 1,
                cpus_per_task: 1,
                memory_per_node: None,
                time_limit: None,
                command: vec![],
                args: vec![],
                env: Default::default(),
                partition: None,
                account: None,
                volumes: vec![],
                host_network: false,
                tolerations: vec![],
                node_selector: Default::default(),
                priority_class: None,
                service_account: None,
                array_spec: None,
                dependencies: vec![],
            },
            status: None,
        }
    }

    #[test]
    fn test_has_job_id_label_present() {
        let labels = BTreeMap::from([("spur.ai/job-id".into(), "42".into())]);
        let job = make_spurjob(Some(labels), Some("default"));
        assert!(has_job_id_label(&job));
    }

    #[test]
    fn test_has_job_id_label_absent() {
        let job = make_spurjob(Some(BTreeMap::new()), Some("default"));
        assert!(!has_job_id_label(&job));
    }

    #[test]
    fn test_has_job_id_label_none_labels() {
        let job = make_spurjob(None, Some("default"));
        assert!(!has_job_id_label(&job));
    }

    #[test]
    fn test_has_job_id_label_other_labels_only() {
        let labels = BTreeMap::from([
            ("spur.ai/managed-by".into(), "spur-k8s-operator".into()),
            ("app".into(), "training".into()),
        ]);
        let job = make_spurjob(Some(labels), Some("default"));
        assert!(!has_job_id_label(&job));
    }

    #[test]
    fn test_has_job_id_label_among_others() {
        let labels = BTreeMap::from([
            ("spur.ai/managed-by".into(), "spur-k8s-operator".into()),
            ("spur.ai/job-id".into(), "99".into()),
        ]);
        let job = make_spurjob(Some(labels), Some("default"));
        assert!(has_job_id_label(&job));
    }

    // --- namespace extraction (reconcile error path) ---

    #[test]
    fn test_namespace_missing_produces_error() {
        let job = make_spurjob(None, None);
        let result = job
            .metadata
            .namespace
            .clone()
            .ok_or_else(|| ReconcileError::Other("SpurJob has no namespace".into()));
        assert!(
            matches!(&result, Err(ReconcileError::Other(msg)) if msg == "SpurJob has no namespace")
        );
    }

    #[test]
    fn test_namespace_present_is_extracted() {
        let job = make_spurjob(None, Some("ml-team"));
        let result = job
            .metadata
            .namespace
            .clone()
            .ok_or_else(|| ReconcileError::Other("SpurJob has no namespace".into()));
        assert_eq!(result.unwrap(), "ml-team");
    }

    // --- should_submit (re-read guard) ---

    #[test]
    fn test_should_submit_when_no_job_id() {
        let status = SpurJobStatus::default();
        assert!(should_submit(&status));
    }

    #[test]
    fn test_should_not_submit_when_job_id_present() {
        let status = SpurJobStatus {
            spur_job_id: Some(42),
            ..Default::default()
        };
        assert!(!should_submit(&status));
    }

    #[test]
    fn test_should_submit_ignores_state() {
        let status = SpurJobStatus {
            state: "Running".into(),
            spur_job_id: None,
            ..Default::default()
        };
        assert!(should_submit(&status));
    }

    #[test]
    fn test_should_not_submit_regardless_of_state() {
        let status = SpurJobStatus {
            state: "Pending".into(),
            spur_job_id: Some(1),
            ..Default::default()
        };
        assert!(!should_submit(&status));
    }
}
