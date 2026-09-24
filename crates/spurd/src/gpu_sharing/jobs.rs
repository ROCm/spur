// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The placeholders of this node's Spur jobs: acquired at launch, released at
//! teardown, and checked periodically for orphans and for losses to pods.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use spur_core::resource::gpu_parent_key;
use spur_devices::cdi::SharingIdentity;
use spur_proto::proto::{GpuHoldReport, GpuHoldState};
use spur_sched::cons_tres::NodeAllocation;
use tokio::time::Instant;
use tonic::Status;
use tracing::{info, warn};

use super::placeholder::{self, AcquireError, PlaceholderSpec, Presence, RequestedGpu};
use super::{discover_identities, lock, GpuSharing, CONVERGE_INTERVAL};

/// Time the controller needs to receive the launch answer before its own
/// dispatch deadline passes.
const DEADLINE_MARGIN: Duration = Duration::from_secs(10);

/// A job attempt's placeholder as this node tracks it.
pub(super) enum Tracked {
    /// The launch is creating the placeholder or waiting for Kubernetes.
    Acquiring,
    /// Kubernetes allocated the claim; `spec.gpus` are the devices the job
    /// runs on. `conflict` says why Kubernetes no longer holds them for it.
    Held {
        spec: PlaceholderSpec,
        conflict: Option<String>,
    },
}

/// The Spur job attempt a placeholder is for.
pub struct PlaceholderJob<'a> {
    pub job_id: u32,
    pub run_attempt: u32,
    pub user: &'a str,
    pub account: &'a str,
}

/// Latest time a launch that the agent received at `received` may wait for
/// Kubernetes. `dispatch_timeout_secs` 0 means the controller does not limit
/// the launch; the default limit applies then.
pub fn launch_deadline(received: Instant, dispatch_timeout_secs: u64) -> Instant {
    let timeout = match dispatch_timeout_secs {
        0 => spur_core::config::ControllerConfig::default().dispatch_timeout_secs,
        secs => secs,
    };
    received + Duration::from_secs(timeout).saturating_sub(DEADLINE_MARGIN)
}

/// (job id, run attempt) of every reservation, launching or committed.
pub fn owned_attempts(allocation: &NodeAllocation) -> HashSet<(u32, u32)> {
    allocation
        .held_job_gpu_ids()
        .into_keys()
        .filter_map(|job| allocation.owner_attempt(job).map(|attempt| (job, attempt)))
        .collect()
}

fn requested_gpu(identity: &SharingIdentity) -> Option<RequestedGpu> {
    Some(RequestedGpu {
        stable_id: identity.stable_id,
        dra_device: identity.dra_device_name()?,
        selector_bdf: identity.selector_bdf.clone(),
        parent_key: gpu_parent_key(identity.stable_id),
    })
}

impl GpuSharing {
    fn requested_gpu_named(&self, dra_device: &str) -> Option<RequestedGpu> {
        let find = |identities: &HashMap<u64, SharingIdentity>| {
            identities
                .values()
                .find(|i| i.dra_device_name().as_deref() == Some(dra_device))
                .cloned()
        };
        let identity = {
            let mut cache = lock(&self.identities);
            if find(&cache.1).is_none() {
                cache.1 = discover_identities();
            }
            find(&cache.1)
        }?;
        requested_gpu(&identity)
    }

    /// Makes Kubernetes allocate `gpus` to a placeholder of the job and returns
    /// the devices the job runs on, in the order of `gpus`. They differ from
    /// `gpus` only when Kubernetes picked other partitions of the same parent.
    /// Every error answers `ResourceExhausted`, so the job requeues.
    pub async fn acquire_placeholder(
        &self,
        job: PlaceholderJob<'_>,
        gpus: &[u64],
        deadline: Instant,
    ) -> Result<Vec<RequestedGpu>, Status> {
        let gpus = gpus
            .iter()
            .map(|&sid| {
                self.identity(sid)
                    .and_then(|i| requested_gpu(&i))
                    .ok_or_else(|| {
                        Status::resource_exhausted(format!(
                            "GPU {sid:#x} cannot be shared with Kubernetes"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let spec = PlaceholderSpec {
            job_id: job.job_id,
            run_attempt: job.run_attempt,
            user: job.user.to_string(),
            account: job.account.to_string(),
            node_name: self.node_name.clone(),
            pause_image: placeholder::PAUSE_IMAGE.to_string(),
            gpus,
        };
        let client = self
            .client()
            .await
            .map_err(|e| Status::resource_exhausted(format!("no Kubernetes API access: {e:#}")))?;
        let key = (job.job_id, job.run_attempt);
        lock(&self.placeholders).insert(key, Tracked::Acquiring);

        let allocated = placeholder::acquire(&client, &spec, deadline).await?;
        let chosen =
            placeholder::map_allocation(&spec, &allocated, |name| self.requested_gpu_named(name))
                .map_err(AcquireError::from)?;
        let held = PlaceholderSpec {
            gpus: chosen.clone(),
            ..spec
        };
        if let Some(tracked) = lock(&self.placeholders).get_mut(&key) {
            *tracked = Tracked::Held {
                spec: held,
                conflict: None,
            };
        }
        Ok(chosen)
    }

    /// Stops tracking the placeholder of a job attempt and deletes it in the
    /// background. `None` when there is nothing to delete or no runtime.
    pub fn release_placeholder(
        self: &Arc<Self>,
        job_id: u32,
        run_attempt: u32,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let tracked = lock(&self.placeholders)
            .remove(&(job_id, run_attempt))
            .is_some();
        // After an agent restart nothing is tracked, but a shared node may
        // still have the placeholder of a recovered job.
        if !tracked && !self.is_shared() {
            return None;
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let this = Arc::clone(self);
        Some(handle.spawn(async move {
            let result = match this.client().await {
                Ok(client) => placeholder::release(&client, job_id, run_attempt, &this.node_name)
                    .await
                    .map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!(job_id, run_attempt, error = %e,
                    "cannot delete the GPU placeholder; the orphan check deletes it later");
            }
        }))
    }

    pub async fn placeholder_loop(
        self: Arc<Self>,
        allocation: Arc<tokio::sync::Mutex<NodeAllocation>>,
    ) {
        let mut interval = tokio::time::interval(CONVERGE_INTERVAL);
        loop {
            interval.tick().await;
            self.check_placeholders(&allocation).await;
        }
    }

    /// Deletes placeholders whose job is not live on this node, and recreates
    /// or reports as conflict the placeholders of running jobs.
    async fn check_placeholders(&self, allocation: &tokio::sync::Mutex<NodeAllocation>) {
        let shared = self.is_shared();
        if !shared && lock(&self.placeholders).is_empty() {
            return;
        }
        // Tracked keys are read before the reservations: every placeholder is
        // tracked after its reservation exists, so one without a reservation
        // now belongs to a job that ended on a path that did not release it.
        let tracked: HashSet<(u32, u32)> = lock(&self.placeholders).keys().copied().collect();
        let owned = owned_attempts(&*allocation.lock().await);
        lock(&self.placeholders).retain(|key, _| owned.contains(key) || !tracked.contains(key));

        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "cannot check the GPU placeholders");
                return;
            }
        };
        let live = || {
            let mut live = owned;
            live.extend(lock(&self.placeholders).keys().copied());
            live
        };
        match placeholder::reconcile_orphans(&client, &self.node_name, live).await {
            Ok(deleted) if !deleted.is_empty() => {
                info!(
                    ?deleted,
                    "deleted GPU placeholders of jobs that are not live"
                )
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "cannot list the GPU placeholders"),
        }
        if shared {
            self.check_presence(&client).await;
        }
    }

    async fn check_presence(&self, client: &kube::Client) {
        let held: Vec<PlaceholderSpec> = lock(&self.placeholders)
            .values()
            .filter_map(|t| match t {
                Tracked::Held { spec, .. } => Some(spec.clone()),
                Tracked::Acquiring => None,
            })
            .collect();
        for spec in held {
            let found = match placeholder::ensure_present(client, &spec).await {
                Ok(Presence::Conflict(reason)) => Some(reason),
                Ok(Presence::Held | Presence::Pending) => None,
                Err(e) => {
                    warn!(job_id = spec.job_id, error = %e, "cannot check the GPU placeholder");
                    continue;
                }
            };
            let changed = match lock(&self.placeholders).get_mut(&(spec.job_id, spec.run_attempt)) {
                Some(Tracked::Held { conflict, .. }) if *conflict != found => {
                    if let Some(reason) = &found {
                        warn!(
                            job_id = spec.job_id,
                            reason, "Kubernetes gave the job's GPUs away"
                        );
                    }
                    *conflict = found;
                    true
                }
                _ => false,
            };
            if changed {
                self.notify_change();
            }
        }
    }

    /// stable_id -> (job id, reason) for every GPU of a job in conflict.
    pub(super) fn conflicts(&self) -> HashMap<u64, (u32, String)> {
        lock(&self.placeholders)
            .values()
            .filter_map(|t| match t {
                Tracked::Held {
                    spec,
                    conflict: Some(reason),
                } => Some((spec, reason)),
                _ => None,
            })
            .flat_map(|(spec, reason)| {
                spec.gpus
                    .iter()
                    .map(|g| (g.stable_id, (spec.job_id, reason.clone())))
            })
            .collect()
    }
}

/// Reports each GPU in `conflicts` as CONFLICT with its job and reason.
pub(super) fn mark_conflicts(report: &mut GpuHoldReport, conflicts: &HashMap<u64, (u32, String)>) {
    for hold in &mut report.gpus {
        if let Some((job_id, reason)) = conflicts.get(&hold.stable_id) {
            hold.state = GpuHoldState::GpuHoldConflict as i32;
            hold.job_id = *job_id;
            hold.reason = reason.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{identity, placeholder_api, FakeApiServer, NODE};
    use super::*;
    use http::Method;
    use spur_core::resource::ResourceSet;
    use spur_proto::proto::GpuHold;

    const PODS: &str = "/api/v1/namespaces/spur-system/pods";
    const CLAIMS: &str = "/apis/resource.k8s.io/v1/namespaces/spur-system/resourceclaims";

    fn spx(n: u64) -> SharingIdentity {
        identity(
            n << 11,
            n as u32,
            128 + n as u32,
            &format!("0000:{n:02x}:00.0"),
        )
    }

    fn cpx(k: u64) -> SharingIdentity {
        identity(
            (0x59 << 11) | k,
            10 + k as u32,
            144 + k as u32,
            "0000:59:00.0",
        )
    }

    fn job(job_id: u32) -> PlaceholderJob<'static> {
        PlaceholderJob {
            job_id,
            run_attempt: 1,
            user: "alice",
            account: "research",
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(300)
    }

    /// Pod and claim are deleted concurrently, so the order is not fixed.
    fn deletes(server: &FakeApiServer) -> HashSet<String> {
        server
            .requests()
            .into_iter()
            .filter(|r| r.method == Method::DELETE)
            .map(|r| r.path)
            .collect()
    }

    fn held_gpus(sharing: &GpuSharing, key: (u32, u32)) -> Option<Vec<u64>> {
        match lock(&sharing.placeholders).get(&key)? {
            Tracked::Held { spec, .. } => Some(spec.gpus.iter().map(|g| g.stable_id).collect()),
            Tracked::Acquiring => None,
        }
    }

    #[test]
    fn launch_deadline_leaves_the_controller_a_margin() {
        let now = Instant::now();

        assert_eq!(launch_deadline(now, 60), now + Duration::from_secs(50));
        assert_eq!(launch_deadline(now, 0), now + Duration::from_secs(290));
        assert_eq!(launch_deadline(now, 5), now);
    }

    #[tokio::test]
    async fn acquired_placeholder_is_tracked_until_released() {
        let server = placeholder_api(&["gpu-1-129"], &[]);
        let sharing = GpuSharing::shared_for_test(&server, vec![spx(1), spx(2)]);

        let chosen = sharing
            .acquire_placeholder(job(7), &[1 << 11], deadline())
            .await
            .expect("acquired");

        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].dra_device, "gpu-1-129");
        assert_eq!(held_gpus(&sharing, (7, 1)), Some(vec![1 << 11]));
        assert_eq!(sharing.live_placeholders(), 1);
        let claim = server
            .requests()
            .into_iter()
            .find(|r| r.method == Method::POST && r.path == CLAIMS)
            .expect("claim created");
        assert_eq!(
            claim.body["metadata"]["labels"]["spur.amd.com/user"],
            "alice"
        );

        sharing
            .release_placeholder(7, 1)
            .expect("release runs")
            .await
            .expect("release task");

        assert_eq!(sharing.live_placeholders(), 0);
        let name = format!("spur-job-7-1-{NODE}");
        assert_eq!(
            deletes(&server),
            HashSet::from([format!("{PODS}/{name}"), format!("{CLAIMS}/{name}")])
        );
    }

    #[tokio::test]
    async fn cpx_job_runs_on_the_partitions_kubernetes_picked() {
        let server = placeholder_api(&["gpu-13-147", "gpu-10-144"], &[]);
        let sharing = GpuSharing::shared_for_test(&server, (0..4).map(cpx).collect());
        let requested = [cpx(0).stable_id, cpx(1).stable_id];

        let chosen = sharing
            .acquire_placeholder(job(8), &requested, deadline())
            .await
            .expect("acquired");

        let chosen: Vec<u64> = chosen.iter().map(|g| g.stable_id).collect();
        assert_eq!(chosen, vec![cpx(0).stable_id, cpx(3).stable_id]);
        assert_eq!(held_gpus(&sharing, (8, 1)), Some(chosen));
    }

    #[tokio::test]
    async fn gpu_without_a_dra_name_is_refused_before_any_api_call() {
        let server = placeholder_api(&[], &[]);
        let mut no_card = spx(1);
        no_card.card_id = None;
        let sharing = GpuSharing::shared_for_test(&server, vec![no_card]);

        let err = sharing
            .acquire_placeholder(job(9), &[1 << 11], deadline())
            .await
            .expect_err("unshareable");

        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        assert!(server.requests().is_empty());
        assert_eq!(sharing.live_placeholders(), 0);
    }

    #[tokio::test]
    async fn release_on_a_node_that_never_shared_does_nothing() {
        let server = placeholder_api(&[], &[]);
        let sharing = GpuSharing::shared_for_test(&server, vec![]);
        sharing.set_desired(false);

        assert!(sharing.release_placeholder(7, 1).is_none());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn check_drops_ended_jobs_deletes_orphans_and_reports_a_lost_gpu() {
        let server = placeholder_api(&["gpu-2-130"], &[(7, 1), (9, 0)]);
        let sharing = GpuSharing::shared_for_test(&server, vec![spx(1), spx(2)]);
        let held = |job_id| Tracked::Held {
            spec: PlaceholderSpec {
                job_id,
                run_attempt: 1,
                user: String::new(),
                account: String::new(),
                node_name: NODE.into(),
                pause_image: placeholder::PAUSE_IMAGE.into(),
                gpus: vec![requested_gpu(&spx(1)).expect("shareable")],
            },
            conflict: None,
        };
        lock(&sharing.placeholders).insert((7, 1), held(7));
        lock(&sharing.placeholders).insert((8, 1), held(8));
        let mut allocation = NodeAllocation::new(NODE.into(), &ResourceSet::default());
        allocation
            .allocate_for_job(7, 1, 0, 0, &[])
            .expect("reserved");
        let allocation = tokio::sync::Mutex::new(allocation);

        sharing.check_placeholders(&allocation).await;

        assert_eq!(sharing.live_placeholders(), 1, "job 8 has no reservation");
        let orphan = format!("spur-job-9-0-{NODE}");
        assert_eq!(
            deletes(&server),
            HashSet::from([format!("{PODS}/{orphan}"), format!("{CLAIMS}/{orphan}")])
        );
        let conflicts = sharing.conflicts();
        let (job_id, reason) = &conflicts[&(1 << 11)];
        assert_eq!(*job_id, 7);
        assert!(reason.contains("gpu-2-130"), "{reason}");
    }

    #[test]
    fn conflicts_override_the_hold_state() {
        let hold = |stable_id, state: GpuHoldState| GpuHold {
            stable_id,
            state: state as i32,
            ..Default::default()
        };
        let mut report = GpuHoldReport {
            generation: 1,
            gpus: vec![
                hold(1, GpuHoldState::GpuHoldSpurJob),
                hold(2, GpuHoldState::GpuHoldFree),
            ],
            unshareable_reason: String::new(),
        };

        mark_conflicts(&mut report, &HashMap::from([(1, (7, "lost".to_string()))]));

        assert_eq!(report.gpus[0].state, GpuHoldState::GpuHoldConflict as i32);
        assert_eq!(
            (report.gpus[0].job_id, report.gpus[0].reason.as_str()),
            (7, "lost")
        );
        assert_eq!(report.gpus[1].state, GpuHoldState::GpuHoldFree as i32);
    }
}
