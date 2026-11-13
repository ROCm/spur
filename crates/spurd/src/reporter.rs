// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
use spur_devices::{resolve_link_type, DeviceRegistry, LinkType};
use spur_proto::proto::{
    RegisterAgentRequest, ResourceSet as ProtoResourceSet, RunningJobStatus, StepdRecoveryRequest,
    StepdRecoveryResponse,
};
use spur_sched::cons_tres::NodeAllocation;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

fn boot_time_from_stat(stat: &str) -> Option<prost_types::Timestamp> {
    let seconds = stat.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "btime")
            .then(|| fields.next()?.parse::<i64>().ok())
            .flatten()
    })?;
    (seconds > 0).then_some(prost_types::Timestamp { seconds, nanos: 0 })
}

/// Channel dial/keepalive and RPC/failover timeouts a [`NodeReporter`] applies to every
/// controller connection. A caller with its own config (spurd's `[spurd]` section) should
/// build this from it; [`Default`] reproduces today's fixed values for tests and any
/// caller with no config of its own.
#[derive(Debug, Clone, Copy)]
pub struct ReporterTimeouts {
    pub channel: spur_client::ChannelTimeouts,
    /// Ceiling on a single register/heartbeat/deregister/recovery RPC — these are fast,
    /// bounded calls, unlike a controller-to-agent launch, so a hung one means wedged.
    pub controller_rpc_timeout: Duration,
    /// How long a controller endpoint that just failed is deprioritized, so it can't
    /// "recapture" every reconnect attempt before the next one is due.
    pub failover_cooldown: Duration,
}

impl Default for ReporterTimeouts {
    fn default() -> Self {
        Self {
            channel: spur_client::ChannelTimeouts::default(),
            controller_rpc_timeout: Duration::from_secs(10),
            failover_cooldown: Duration::from_secs(60),
        }
    }
}

/// Lock a `std::sync::Mutex`, recovering the inner state on poison rather than
/// panicking — none of this module's critical sections can leave data inconsistent.
fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Same recovery as [`lock_recover`], for a `RwLock` read guard.
fn read_recover<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Same recovery as [`lock_recover`], for a `RwLock` write guard.
fn write_recover<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The run a held job belongs to, so a reclaim can target that run alone.
pub trait HeldRun {
    fn run_attempt(&self) -> u32;
}

/// Source of the (job id, run attempt) pairs this node currently holds. The
/// controller decides from its own authoritative state whether any is stale.
pub trait HeldJobs: Send + Sync {
    fn held_jobs(&self) -> Vec<(u32, u32)>;
}

pub trait RecoveryInventory: Send + Sync {
    fn no_held_jobs(&self) -> Option<bool>;
}

impl<T: HeldRun + Send> HeldJobs for Mutex<HashMap<u32, T>> {
    fn held_jobs(&self) -> Vec<(u32, u32)> {
        match self.try_lock() {
            Ok(jobs) => jobs
                .iter()
                .map(|(job_id, job)| (*job_id, job.run_attempt()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Discovers and reports node resources to the controller.
pub struct NodeReporter {
    pub hostname: String,
    pub controller_addr: String,
    pub resources: RwLock<ResourceSet>,
    pub node_address: spur_net::NodeAddress,
    pub labels: HashMap<String, String>,
    pub free_memory_mb: AtomicU64,
    pub cpu_load: AtomicU64,
    pub join_token: String,
    /// The WireGuard interface this node's mesh key is read from (e.g. "spur0"). The public key is
    /// re-read on every register/heartbeat via [`wg_pubkey`](Self::wg_pubkey) so a key that appears
    /// or changes after startup (late mesh join, `spur0` recreated) reaches the controller.
    pub wg_iface: String,
    /// Directory holding `<wg_iface>.conf` (e.g. `/etc/wireguard`), so `apply_mesh` can read which
    /// peers were persisted by `spur net add-peer` and exempt them from the k0s reconcile's prune.
    pub wg_config_dir: std::path::PathBuf,
    node_token: RwLock<String>,
    boot_time: Option<prost_types::Timestamp>,
    /// Job ids this node holds, reported each heartbeat so the controller can
    /// reclaim allocations it no longer tracks. Shares the agent's running map.
    held_jobs: Arc<dyn HeldJobs>,
    recovery_inventory: std::sync::OnceLock<Arc<dyn RecoveryInventory>>,
    /// The live per-node allocation, read each heartbeat for the translated GPU
    /// stable_ids each held job occupies so the controller can converge its
    /// used-view. Authoritative post-translation source (an adopted legacy job's
    /// `owners` entry holds the current stable_ids, unlike its running-map
    /// descriptor). Wired once, after the AgentService is built.
    allocation: std::sync::OnceLock<Arc<Mutex<NodeAllocation>>>,
    /// k0s node status the heartbeat carries; wired once after the K0sAgent is built.
    k0s_status: std::sync::OnceLock<Arc<crate::cluster::K0sNodeState>>,
    /// Endpoint a dial/RPC most recently failed against, and until when to prefer a
    /// different one. Left stale once the cooldown lapses; never read past then.
    last_failed_endpoint: std::sync::Mutex<Option<(String, Instant)>>,
    /// Holds the delivered core layout and applies it to the cgroup root.
    spec: std::sync::Mutex<crate::spec::SpecEnforcer>,
    spec_root: std::path::PathBuf,
    /// The held layout's spec CPUs, readable without waiting on root I/O under
    /// `spec`, since allocation reads it while holding the allocation lock.
    spec_cpus: RwLock<Vec<u32>>,
    /// Whether spurd's threads are currently pinned off spec cores, so the mask
    /// is widened back to all online CPUs when the setting is removed.
    daemons_pinned: std::sync::atomic::AtomicBool,
    timeouts: ReporterTimeouts,
    baseline_dir: Option<std::path::PathBuf>,
    /// Committed with `resources`: the repartitioned GPUs that set withholds.
    fences: RwLock<Fences>,
}

impl NodeReporter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hostname: String,
        controller_addr: String,
        mut resources: ResourceSet,
        node_address: spur_net::NodeAddress,
        labels: HashMap<String, String>,
        join_token: String,
        wg_iface: String,
        wg_config_dir: std::path::PathBuf,
        held_jobs: Arc<dyn HeldJobs>,
        timeouts: ReporterTimeouts,
    ) -> Self {
        if resources.fabric_binding_version > 0 && resources.generation == 0 {
            // Zero denotes legacy inventory. A fresh boot epoch also fences
            // bindings retained by supervisors from a previous agent instance.
            resources.generation = ((uuid::Uuid::new_v4().as_u128() as u64) >> 1).max(1);
        }
        Self {
            hostname,
            controller_addr,
            resources: RwLock::new(resources),
            node_address,
            labels,
            free_memory_mb: AtomicU64::new(0),
            cpu_load: AtomicU64::new(0),
            join_token,
            wg_iface,
            wg_config_dir,
            node_token: RwLock::new(String::new()),
            boot_time: std::fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|stat| boot_time_from_stat(&stat)),
            held_jobs,
            recovery_inventory: std::sync::OnceLock::new(),
            allocation: std::sync::OnceLock::new(),
            k0s_status: std::sync::OnceLock::new(),
            last_failed_endpoint: std::sync::Mutex::new(None),
            spec: std::sync::Mutex::new(crate::spec::SpecEnforcer::new(
                std::path::PathBuf::from(crate::executor::CGROUP_ROOT),
                None,
                true,
            )),
            spec_root: std::path::PathBuf::from(crate::executor::CGROUP_ROOT),
            spec_cpus: RwLock::new(Vec::new()),
            daemons_pinned: std::sync::atomic::AtomicBool::new(false),
            timeouts,
            baseline_dir: None,
            fences: RwLock::new(Fences::default()),
        }
    }

    /// The fences the startup inventory was reported with.
    pub fn with_fences(mut self, fences: Fences) -> Self {
        self.fences = RwLock::new(fences);
        self
    }

    pub fn fences(&self) -> Fences {
        read_recover(&self.fences).clone()
    }

    /// Commit fences whose reported set the controller already has.
    pub fn commit_fences(&self, fences: &Fences) -> bool {
        if let Some(dir) = &self.baseline_dir {
            if !save_fences(dir, fences) {
                return false;
            }
        }
        *write_recover(&self.fences) = fences.clone();
        true
    }

    /// Persist every controller-acked inventory under `dir`, so a restart can
    /// tell a re-encoded or repartitioned GPU from the one a job was given.
    pub fn with_baseline_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.baseline_dir = Some(dir);
        self
    }

    /// The learned node-identity token, read live (it is set at registration and
    /// may change on re-registration). Empty when node identity is not in use.
    pub fn node_token(&self) -> String {
        read_recover(&self.node_token).clone()
    }

    /// Where spec enforcement writes, the marker that records spurd's own root
    /// controls, and whether this node's `[cgroup]` is enabled.
    pub fn with_spec_enforcement(
        mut self,
        root: std::path::PathBuf,
        marker: std::path::PathBuf,
        cgroup_enabled: bool,
    ) -> Self {
        self.spec = std::sync::Mutex::new(crate::spec::SpecEnforcer::new(
            root.clone(),
            Some(marker),
            cgroup_enabled,
        ));
        self.spec_root = root;
        self
    }

    /// Adopt a core layout the controller delivered. Returns true when the held
    /// layout changed.
    pub fn adopt_core_layout(
        &self,
        layout: Option<spur_proto::proto::CoreLayout>,
        revision: Option<spur_proto::proto::LayoutRevision>,
        delivery: crate::spec::Delivery,
    ) -> bool {
        let mut spec = lock_recover(&self.spec);
        let changed = spec.adopt(
            layout.map(Into::into),
            revision.map(Into::into).unwrap_or_default(),
            delivery,
        );
        if changed {
            *write_recover(&self.spec_cpus) = spec.spec_cpus();
        }
        changed
    }

    /// Write the held layout to the cgroup root now if it is not in effect yet,
    /// and report whether the node may start workloads.
    pub async fn ensure_spec_enforced(self: &Arc<Self>) -> bool {
        if lock_recover(&self.spec).needs_apply() {
            let reporter = Arc::clone(self);
            if let Err(e) = tokio::task::spawn_blocking(move || reporter.apply_spec_layout()).await
            {
                warn!(error = %e, "spec layout apply task failed");
            }
        }
        lock_recover(&self.spec).enforcement_healthy()
    }

    pub fn spec_enforcement_error(&self) -> Option<String> {
        lock_recover(&self.spec).enforcement_error()
    }

    pub fn holds_spec_cores(&self) -> bool {
        !read_recover(&self.spec_cpus).is_empty()
    }

    /// CPU IDs reserved for node services, for the local ledger to exclude.
    pub fn held_spec_cpus(&self) -> Vec<u32> {
        read_recover(&self.spec_cpus).clone()
    }

    /// Apply the held layout to the cgroup root and pin spurd off spec cores.
    /// Blocking: cgroup writes can reclaim synchronously.
    fn apply_spec_layout(&self) {
        let root = self.spec_root.as_path();
        let live = read_live_job_cpusets(root);
        let total = read_mem_total_mb();
        let daemon_cpus = {
            let mut spec = lock_recover(&self.spec);
            match spec.apply(&live, total) {
                Ok(crate::spec::MemReservation::Pending) => {
                    debug!("node spec memory reservation pending: jobs still run under the root")
                }
                Ok(_) => {}
                Err(e) => warn!(
                    error = %e,
                    "failed to apply node spec layout to cgroup root; node not enforcing"
                ),
            }
            spec.daemon_cpuset()
        };
        match daemon_cpus {
            Some(cpus) => {
                if let Err(e) = pin_current_process_threads(&cpus) {
                    warn!(error = %e, "failed to pin spurd off spec cores");
                } else {
                    self.daemons_pinned.store(true, Ordering::Relaxed);
                }
            }
            None => {
                if self.daemons_pinned.swap(false, Ordering::Relaxed) {
                    if let Some(online) =
                        discover_cpu_topology(Path::new("/sys")).map(|t| t.online_cpus())
                    {
                        if let Err(e) = pin_current_process_threads(&online) {
                            warn!(error = %e, "failed to widen spurd back onto all CPUs");
                        }
                    }
                }
            }
        }
    }

    /// Controller endpoints for the next dial. Skips a host that failed within the last
    /// [`ReporterTimeouts::failover_cooldown`], provided there's another one to prefer instead.
    fn dial_endpoints(&self) -> String {
        let guard = lock_recover(&self.last_failed_endpoint);
        match &*guard {
            Some((bad, until)) if Instant::now() < *until => {
                spur_client::deprioritize(&self.controller_addr, bad)
            }
            _ => self.controller_addr.clone(),
        }
    }

    /// Record that `endpoint` just failed to dial or answer, so the next attempt
    /// (from any RPC on this reporter) prefers a different host for a while.
    fn note_dial_failure(&self, endpoint: &str) {
        if endpoint.is_empty() {
            return;
        }
        *lock_recover(&self.last_failed_endpoint) = Some((
            endpoint.to_string(),
            Instant::now() + self.timeouts.failover_cooldown,
        ));
    }

    /// Connect, run `op`, bound to [`ReporterTimeouts::controller_rpc_timeout`]. A transport
    /// failure or timeout deprioritizes the endpoint that was actually dialed; an app-level
    /// rejection doesn't.
    async fn with_controller<T, F, Fut>(
        &self,
        op: F,
    ) -> Result<T, crate::controller_auth::ConnectAuthError>
    where
        F: FnOnce(crate::controller_auth::ControllerClient) -> Fut,
        Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        self.with_controller_timeout(self.timeouts.controller_rpc_timeout, op)
            .await
    }

    /// Same as [`Self::with_controller`] with an explicit bound, so tests can exercise
    /// the real timeout/failover mechanism without waiting out the production value.
    async fn with_controller_timeout<T, F, Fut>(
        &self,
        timeout: Duration,
        op: F,
    ) -> Result<T, crate::controller_auth::ConnectAuthError>
    where
        F: FnOnce(crate::controller_auth::ControllerClient) -> Fut,
        Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let endpoints = self.dial_endpoints();
        // `connect_channel_tracked` may skip past a dead-fast entry before dialing
        // succeeds; this records which one actually answered the dial so a later
        // failure blames that host, not just whichever entry is listed first.
        let dialed = Arc::new(std::sync::Mutex::new(None::<String>));
        let record_dialed = Arc::clone(&dialed);
        let channel_timeouts = self.timeouts.channel;
        let attempt = async move {
            let (channel, endpoint) =
                spur_client::connect_channel_tracked_with_timeouts(&endpoints, channel_timeouts)
                    .await
                    .map_err(crate::controller_auth::ConnectAuthError::Transport)?;
            *lock_recover(&record_dialed) = Some(endpoint);
            let client = crate::controller_auth::wrap(channel)
                .await
                .map_err(crate::controller_auth::ConnectAuthError::Status)?;
            op(client)
                .await
                .map(tonic::Response::into_inner)
                .map_err(crate::controller_auth::ConnectAuthError::Status)
        };
        let blame = move || lock_recover(&dialed).clone().unwrap_or_default();
        match tokio::time::timeout(timeout, attempt).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                if is_transport_failure(&e) {
                    self.note_dial_failure(&blame());
                }
                Err(e)
            }
            Err(_elapsed) => {
                self.note_dial_failure(&blame());
                Err(crate::controller_auth::ConnectAuthError::Status(
                    tonic::Status::deadline_exceeded(format!(
                        "controller RPC timed out after {timeout:?}"
                    )),
                ))
            }
        }
    }

    /// Wire the live per-node allocation so heartbeats can report each held job's
    /// translated GPU stable_ids. Called once, after the AgentService is built.
    pub fn set_allocation(&self, allocation: Arc<Mutex<NodeAllocation>>) {
        let _ = self.allocation.set(allocation);
    }

    /// Per-held-job translated GPU stable_ids for this heartbeat. Empty when the
    /// allocation is unwired or its lock is momentarily contended — a heartbeat
    /// then omits the field and the controller keeps its prior view, which the
    /// next heartbeat corrects.
    fn held_job_gpu_ids(&self) -> HashMap<u32, Vec<u64>> {
        match self.allocation.get().and_then(|a| a.try_lock().ok()) {
            Some(alloc) => alloc.held_job_gpu_ids(),
            None => HashMap::new(),
        }
    }

    /// Wire the k0s node-status source so heartbeats carry `spur_k8s_node_*` fields. Called once,
    /// after the K0sAgent is constructed. No-op on a node without k0s (field stays unset).
    pub fn set_k0s_status(&self, status: Arc<crate::cluster::K0sNodeState>) {
        let _ = self.k0s_status.set(status);
    }

    /// This node's current WireGuard mesh public key (empty if the interface has no key / no mesh).
    /// Read live from the interface so a key that appears or changes after startup is picked up.
    fn wg_pubkey(&self) -> String {
        spur_net::wireguard::interface_public_key(&self.wg_iface).unwrap_or_default()
    }

    /// (job id, run attempt) pairs this heartbeat would report, from the shared running map.
    pub fn held_jobs(&self) -> Vec<(u32, u32)> {
        self.held_jobs.held_jobs()
    }

    pub fn snapshot_resources(&self) -> ResourceSet {
        read_recover(&self.resources).clone()
    }

    pub fn resource_generation(&self) -> u64 {
        read_recover(&self.resources).generation
    }

    pub async fn publish_resources(
        &self,
        allocation: &Mutex<NodeAllocation>,
        fresh: ResourceSet,
        fences: &Fences,
    ) {
        // Dispatch checks generation while reserving under this same mutex.
        let _allocation = allocation.lock().await;
        if let Some(dir) = &self.baseline_dir {
            // Fences first: a baseline already reduced to the pinned ids
            // cannot reveal the repartition on its own after a crash.
            if save_fences(dir, fences) {
                save_baseline(dir, &fresh);
            }
        }
        *write_recover(&self.fences) = fences.clone();
        self.update_resources(fresh);
    }

    /// Swap the reported inventory if its schedulable content changed. Ignores
    /// `generation` so a pure generation bump does not itself count as a change.
    pub fn update_resources(&self, fresh: ResourceSet) -> bool {
        let mut cur = write_recover(&self.resources);
        let changed = cur.cpus != fresh.cpus
            || cur.memory_mb != fresh.memory_mb
            || cur.gpus != fresh.gpus
            || cur.generic != fresh.generic
            || cur.fabric_binding_version != fresh.fabric_binding_version
            || cur.block_file_revision != fresh.block_file_revision
            || cur.topology != fresh.topology;
        *cur = fresh;
        changed
    }

    /// Register with the controller, reporting the reporter's current inventory.
    pub async fn register(&self) -> anyhow::Result<()> {
        self.register_with(&self.snapshot_resources()).await
    }

    pub fn set_recovery_inventory(&self, inventory: Arc<dyn RecoveryInventory>) {
        let _ = self.recovery_inventory.set(inventory);
    }

    #[cfg(test)]
    pub(crate) fn registration_request(&self, resources: &ResourceSet) -> RegisterAgentRequest {
        self.recovery_registration_request(resources, None)
    }

    pub(crate) fn recovery_registration_request(
        &self,
        resources: &ResourceSet,
        recovery_down_since: Option<prost_types::Timestamp>,
    ) -> RegisterAgentRequest {
        let mut labels = self.labels.clone();
        labels.insert("spur.stepd".into(), "1".into());

        RegisterAgentRequest {
            hostname: self.hostname.clone(),
            recovery_down_since,
            boot_time: self.boot_time,
            no_held_jobs: self.recovery_inventory.get().and_then(|i| i.no_held_jobs()),
            resources: Some(resource_to_proto(resources)),
            version: env!("CARGO_PKG_VERSION").into(),
            address: self.node_address.ip.clone(),
            port: self.node_address.port as u32,
            wg_pubkey: self.wg_pubkey(),
            labels,
            join_token: self.join_token.clone(),
            topology: resources.topology.as_ref().map(Into::into),
            capabilities: vec![spur_core::node::CAP_SIGNAL_JOB.into()],
        }
    }

    /// Register with the controller reporting a specific inventory. The refresh
    /// task uses this to converge on a fresh set WITHOUT first committing it to
    /// the reporter baseline, so a failed register leaves the baseline unchanged
    /// and the next tick re-detects the same delta and retries.
    pub async fn register_with(&self, resources: &ResourceSet) -> anyhow::Result<()> {
        self.register_for_recovery(resources, None).await
    }

    async fn register_for_recovery(
        &self,
        resources: &ResourceSet,
        recovery_down_since: Option<prost_types::Timestamp>,
    ) -> anyhow::Result<()> {
        let ticket = lock_recover(&self.spec).begin_registration();
        let req = self.recovery_registration_request(resources, recovery_down_since);
        let inner = self
            .with_controller(move |mut client| async move { client.register_agent(req).await })
            .await
            .context("registration failed")?;

        if inner.accepted {
            warn_without_node_identity(&inner.node_token);
            if !inner.node_token.is_empty() {
                *write_recover(&self.node_token) = inner.node_token;
            }
            // The registration reply is the authoritative layout source; adopt it
            // so spec enforcement starts before the first heartbeat.
            self.adopt_core_layout(
                inner.core_layout,
                inner.layout_revision,
                crate::spec::Delivery::Registration {
                    layout_error: !inner.layout_error.is_empty(),
                    ticket,
                },
            );
            info!("registered with controller");
        } else {
            anyhow::bail!("controller rejected registration: {}", inner.message);
        }

        Ok(())
    }

    /// Notify the controller that this agent is shutting down.
    pub async fn deregister(&self, reason: &str) -> anyhow::Result<()> {
        let req = spur_proto::proto::DeregisterAgentRequest {
            hostname: self.hostname.clone(),
            node_token: read_recover(&self.node_token).clone(),
            reason: reason.to_string(),
        };
        self.with_controller(move |mut client| async move { client.deregister_agent(req).await })
            .await
            .context("deregistration RPC failed")?;

        info!("deregistered from controller");
        Ok(())
    }

    pub async fn report_stepd_recovery(
        &self,
        job_id: u32,
        run_attempt: u32,
        step_id: spur_core::step::StepId,
        stale_descriptor: bool,
    ) -> anyhow::Result<StepdRecoveryResponse> {
        let node_token = self
            .node_token
            .read()
            .map_err(|_| anyhow::anyhow!("runtime recovery node token lock poisoned"))?
            .clone();
        let req = StepdRecoveryRequest {
            hostname: self.hostname.clone(),
            job_id,
            run_attempt,
            node_token,
            stale_descriptor,
            step_id,
        };
        self.with_controller(
            move |mut client| async move { client.report_stepd_recovery(req).await },
        )
        .await
        .context("runtime recovery report failed")
    }

    /// Periodic heartbeat loop.
    pub async fn heartbeat_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            spur_core::config::HEARTBEAT_INTERVAL_SECS,
        ));

        loop {
            interval.tick().await;

            let (load, free_mem) = read_system_metrics();
            self.cpu_load.store(load as u64, Ordering::Relaxed);
            self.free_memory_mb.store(free_mem, Ordering::Relaxed);
            let current_token = read_recover(&self.node_token).clone();
            let running_jobs = build_running_jobs(self.held_jobs(), &self.held_job_gpu_ids());
            let reporter = Arc::clone(&self);
            if let Err(e) = tokio::task::spawn_blocking(move || reporter.apply_spec_layout()).await
            {
                warn!(error = %e, "spec layout apply task failed");
            }
            let (core_layout_fingerprint, spec_enforcement_error) = {
                let spec = lock_recover(&self.spec);
                (
                    spec.held_fingerprint(),
                    spec.enforcement_error().unwrap_or_default(),
                )
            };
            let req = spur_proto::proto::HeartbeatRequest {
                hostname: self.hostname.clone(),
                cpu_load: load,
                free_memory_mb: free_mem,
                running_jobs,
                node_token: current_token,
                wg_pubkey: self.wg_pubkey(),
                k0s_status: self.k0s_status.get().map(|s| {
                    let (unit_active, restart_count, install_secs) = s.take_for_heartbeat();
                    spur_proto::proto::K0sNodeStatus {
                        unit_active,
                        restart_count,
                        install_duration_seconds: install_secs,
                    }
                }),
                core_layout_fingerprint,
                spec_enforcement_error,
            };

            match self.send_heartbeat(req).await {
                Ok(resp) => {
                    debug!(load, free_mem, "heartbeat sent");
                    // The controller asks for re-registration when the layout it
                    // expects differs from what this node reported enforcing, so
                    // a changed spec policy is redelivered without a restart.
                    if resp.reregister {
                        info!("controller requested re-registration; re-registering");
                        if let Err(e) = self
                            .register_for_recovery(
                                &self.snapshot_resources(),
                                resp.recovery_down_since,
                            )
                            .await
                        {
                            warn!(error = %e, "controller-requested re-registration failed");
                        }
                    }
                }
                Err(crate::controller_auth::ConnectAuthError::Status(status))
                    if should_reregister(&status) =>
                {
                    warn!(
                        error = %status,
                        "controller does not recognize this node; re-registering"
                    );
                    if let Err(e) = self.register().await {
                        warn!(error = %e, "re-registration after heartbeat rejection failed");
                    }
                }
                Err(e) => warn!(error = %e, "heartbeat failed"),
            }
        }
    }

    /// One heartbeat. A transport failure is retried at once when another controller
    /// is configured: the failed one was just deprioritized, so the retry reaches a
    /// different host instead of leaving this node silent until the next beat. With
    /// the failover cooldown off nothing is deprioritized, so there is no retry. Each
    /// attempt is bounded by `with_controller`'s timeout.
    async fn send_heartbeat(
        &self,
        req: spur_proto::proto::HeartbeatRequest,
    ) -> Result<spur_proto::proto::HeartbeatResponse, crate::controller_auth::ConnectAuthError>
    {
        let retry = req.clone();
        let e = match self
            .with_controller(move |mut client| async move { client.heartbeat(req).await })
            .await
        {
            Ok(resp) => return Ok(resp),
            Err(e) => e,
        };
        let another_host = spur_client::parse_endpoints(&self.controller_addr).len() > 1
            && !self.timeouts.failover_cooldown.is_zero();
        if !is_transport_failure(&e) || !another_host {
            return Err(e);
        }
        warn!(error = %e, "heartbeat did not reach the controller; retrying on another controller");
        self.with_controller(move |mut client| async move { client.heartbeat(retry).await })
            .await
    }
}

/// The heartbeat's per-job status list. Each job carries its run and translated GPU
/// stable_ids, so the controller can reclaim that run and converge its used-view.
fn build_running_jobs(
    held: Vec<(u32, u32)>,
    gpu_ids: &HashMap<u32, Vec<u64>>,
) -> Vec<RunningJobStatus> {
    held.into_iter()
        .map(|(job_id, run_attempt)| RunningJobStatus {
            job_id,
            run_attempt,
            gpu_stable_ids: gpu_ids.get(&job_id).cloned().unwrap_or_default(),
            ..Default::default()
        })
        .collect()
}

/// A `NOT_FOUND` heartbeat means the controller lost this node's registration
/// (e.g. it restarted and dropped the record) while this agent kept running.
/// The controller never proactively tells an agent to re-register, so without
/// this the agent would heartbeat into the same rejection forever.
fn should_reregister(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::NotFound
}

/// Whether a connect/RPC failure means the dialed endpoint is unreachable or wedged,
/// as opposed to a healthy host answering with an application-level rejection.
fn is_transport_failure(e: &crate::controller_auth::ConnectAuthError) -> bool {
    match e {
        crate::controller_auth::ConnectAuthError::Transport(_) => true,
        // No controller handler returns ResourceExhausted; tonic surfaces an h2
        // GOAWAY(ENHANCE_YOUR_CALM) as it, from this connection or from the one a
        // follower forwards over.
        crate::controller_auth::ConnectAuthError::Status(status) => {
            spur_proto::controller_rpc_retryable(status)
                || status.code() == tonic::Code::ResourceExhausted
        }
    }
}

/// Jobs are supervised either way; without a signing key the controller takes a
/// recovery report on trust rather than dropping a live supervisor.
fn warn_without_node_identity(node_token: &str) {
    if node_token.is_empty() {
        warn!(
            "no node identity issued ([auth] jwt_key unset): recovery reports are accepted \
             unverified; set jwt_key to have this node prove its identity"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryDelta {
    Unchanged,
    FreeCapacityChanged,
    AllocatedDevicesLost,
}

/// Default cadence of the periodic inventory-refresh task.
pub const DEFAULT_INVENTORY_REFRESH_SECS: u64 = 60;

/// What a debounced refresh tick resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshAction {
    /// Nothing to do: inventory unchanged, or a change not yet seen twice.
    Wait,
    /// Apply the fresh set as new free capacity, then converge the controller.
    ApplyCapacity,
    /// A held device vanished; converge so the controller's advertised GRES
    /// reflects the shrunk inventory. This does not drain the affected job.
    ReportLost,
}

/// Fingerprint of a schedulable set, used to debounce on inventory identity.
pub type InventoryFingerprint = spur_core::resource::InventoryFingerprint;

/// Debounced per-tick decision. A non-`Unchanged` delta acts only when it
/// repeats AND the fresh inventory identity matches the previous tick's, so two
/// DIFFERENT partial reads that both classify the same delta never converge on
/// a topology observed only once. Returns the action plus the delta and
/// fingerprint to carry into the next tick.
pub fn next_refresh_action(
    delta: &InventoryDelta,
    fresh_fp: &InventoryFingerprint,
    last_delta: &InventoryDelta,
    last_fp: &InventoryFingerprint,
) -> (RefreshAction, InventoryDelta) {
    // Reset on unchanged; arm (but don't act) on a first sighting or when either
    // the delta or the observed inventory differs from the prior tick.
    if *delta == InventoryDelta::Unchanged || delta != last_delta || fresh_fp != last_fp {
        return (RefreshAction::Wait, delta.clone());
    }
    let action = match delta {
        InventoryDelta::FreeCapacityChanged => RefreshAction::ApplyCapacity,
        InventoryDelta::AllocatedDevicesLost => RefreshAction::ReportLost,
        InventoryDelta::Unchanged => RefreshAction::Wait,
    };
    (action, InventoryDelta::Unchanged)
}

/// Whether the reporter baseline should be advanced to the just-converged set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineCommit {
    Commit,
    Keep,
}

/// Resolve the baseline commit and debounce carry after a converge attempt.
///
/// The reporter baseline is the source `classify` compares against, so it must
/// advance ONLY once the controller has acknowledged the fresh set. On success
/// commit and reset the carry; on failure keep the old baseline (so the next
/// tick re-detects the same delta) and re-arm that delta so the retry fires on
/// the very next tick instead of re-serving the seen-twice debounce.
pub fn post_converge(
    acted_delta: &InventoryDelta,
    registered_ok: bool,
) -> (BaselineCommit, InventoryDelta) {
    if registered_ok {
        (BaselineCommit::Commit, InventoryDelta::Unchanged)
    } else {
        (BaselineCommit::Keep, acted_delta.clone())
    }
}

/// A zero, empty, or unparseable value falls back to the default; zero would
/// otherwise panic `tokio::time::interval`.
fn parse_refresh_secs(raw: Option<String>) -> u64 {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_INVENTORY_REFRESH_SECS)
}

/// Cadence of the inventory-refresh task, from `SPUR_INVENTORY_REFRESH_SECS`
/// (so e2e can drive convergence fast) or the default.
pub fn inventory_refresh_interval() -> std::time::Duration {
    std::time::Duration::from_secs(parse_refresh_secs(
        std::env::var("SPUR_INVENTORY_REFRESH_SECS").ok(),
    ))
}

/// Classify how the reported inventory changed between refreshes. Distinguishes a device that
/// is merely absent (free capacity shrank) from one that is absent AND currently allocated to a
/// job (the refresh task must fence/hold instead of silently losing track of it).
pub fn classify(
    old: &ResourceSet,
    new: &ResourceSet,
    allocated_stable_ids: &std::collections::HashSet<u64>,
) -> InventoryDelta {
    if old.cpus == new.cpus
        && old.memory_mb == new.memory_mb
        && old.gpus == new.gpus
        && old.generic == new.generic
        && old.fabric_binding_version == new.fabric_binding_version
        && old.block_file_revision == new.block_file_revision
        && old.topology == new.topology
    {
        return InventoryDelta::Unchanged;
    }
    let new_ids: std::collections::HashSet<u64> = new.gpus.iter().map(|g| g.stable_id).collect();
    let old_ids: Vec<u64> = old.gpus.iter().map(|g| g.stable_id).collect();
    let new_list: Vec<u64> = new.gpus.iter().map(|g| g.stable_id).collect();
    // A held id that survives a repartition names different silicon now.
    let layout = spur_core::gpu_identity::diff_layout(&old_ids, &new_list);
    let lost_held = allocated_stable_ids
        .iter()
        .any(|&id| !layout.is_intact(id, &new_ids));
    if lost_held {
        InventoryDelta::AllocatedDevicesLost
    } else {
        InventoryDelta::FreeCapacityChanged
    }
}

/// Physical-GPU identity (BDF anchor) of a stable_id: mask off the low
/// partition bits so two ids on the same silicon compare equal.
fn bdf_anchor(stable_id: u64) -> u64 {
    spur_core::gpu_identity::anchor(stable_id)
}

/// Held ids moved to the current encoding. `baseline` renames only held
/// entries: fully renamed it would equal `fresh` and hide the rename.
pub struct RefreshPlan {
    pub renamed: HashMap<u64, u64>,
    pub baseline: ResourceSet,
    pub held: HashSet<u64>,
}

pub fn plan_refresh(old: &ResourceSet, fresh: &ResourceSet, held: &HashSet<u64>) -> RefreshPlan {
    let ids = |set: &ResourceSet| set.gpus.iter().map(|g| g.stable_id).collect::<Vec<_>>();
    let renamed = spur_core::gpu_identity::diff_layout(&ids(old), &ids(fresh)).safe_renames();
    // After a failed register the ledger already holds the new id.
    let held_renamed: HashMap<u64, u64> = renamed
        .iter()
        .filter(|(id, to)| held.contains(id) || held.contains(to))
        .map(|(&id, &to)| (id, to))
        .collect();
    let held = held
        .iter()
        .map(|id| renamed.get(id).copied().unwrap_or(*id))
        .collect();
    RefreshPlan {
        baseline: old.with_renamed_gpus(&held_renamed),
        renamed,
        held,
    }
}

pub const BASELINE_FILE: &str = "inventory-baseline.json";

/// The last inventory the controller acknowledged. Absent or unreadable both
/// mean "no baseline": startup then falls back to the legacy id rule.
pub fn load_baseline(dir: &std::path::Path) -> Option<ResourceSet> {
    let contents = std::fs::read(dir.join(BASELINE_FILE)).ok()?;
    match serde_json::from_slice(&contents) {
        Ok(set) => Some(set),
        Err(error) => {
            warn!(%error, "unreadable inventory baseline; ignoring it");
            None
        }
    }
}

/// Best-effort: a missing baseline only costs the next restart its precision.
pub fn save_baseline(dir: &std::path::Path, set: &ResourceSet) {
    let written = serde_json::to_vec(set)
        .map_err(std::io::Error::other)
        .and_then(|contents| crate::stepd::publish_private(dir, BASELINE_FILE, &contents));
    if let Err(error) = written {
        warn!(%error, dir = %dir.display(), "could not save the inventory baseline");
    }
}

pub const FENCES_FILE: &str = "inventory-fences.json";

/// GPUs repartitioned under running jobs, by BDF anchor: each offers only what
/// its pre-change holders still hold until they exit.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Fences {
    #[serde(default)]
    pub anchors: std::collections::BTreeMap<u64, Vec<GpuResource>>,
}

impl Fences {
    pub fn pinned_by_anchor(&self) -> std::collections::BTreeMap<u64, Vec<u64>> {
        self.anchors
            .iter()
            .map(|(&anchor, pinned)| (anchor, pinned.iter().map(|g| g.stable_id).collect()))
            .collect()
    }

    pub fn pinned_ids(&self) -> HashSet<u64> {
        self.anchors
            .values()
            .flatten()
            .map(|g| g.stable_id)
            .collect()
    }

    fn pruned(&self, held: &HashSet<u64>) -> Fences {
        let anchors = self
            .anchors
            .iter()
            .map(|(&anchor, pinned)| {
                let kept: Vec<GpuResource> = pinned
                    .iter()
                    .filter(|g| held.contains(&g.stable_id))
                    .cloned()
                    .collect();
                (anchor, kept)
            })
            .filter(|(_, kept)| !kept.is_empty())
            .collect();
        Fences { anchors }
    }

    fn pin(&mut self, anchor: u64, gpu: &GpuResource) {
        let pinned = self.anchors.entry(anchor).or_default();
        if !pinned.iter().any(|g| g.stable_id == gpu.stable_id) {
            pinned.push(gpu.clone());
            pinned.sort_by_key(|g| g.stable_id);
        }
    }

    /// `fresh` with every fenced GPU reduced to its still-pinned entries, in
    /// the order `reconcile_free_pool` reports them.
    pub fn apply(&self, fresh: &ResourceSet) -> ResourceSet {
        let pinned_ids = self.pinned_ids();
        let mut effective = fresh.clone();
        effective.gpus.retain(|g| {
            !self.anchors.contains_key(&bdf_anchor(g.stable_id))
                && !pinned_ids.contains(&g.stable_id)
        });
        let mut pinned: Vec<&GpuResource> = self.anchors.values().flatten().collect();
        pinned.sort_by_key(|g| g.stable_id);
        effective
            .gpus
            .extend(pinned.into_iter().map(|g| pinned_gpu(g, fresh)));
        effective
    }
}

/// A fence older than this likely means a pinned id whose holder leaked.
pub const FENCE_WARN_AFTER: Duration = Duration::from_secs(60 * 60);

/// When each fenced GPU was first seen, so one held far longer than a job
/// should run is surfaced once instead of withholding the GPU silently.
#[derive(Default)]
pub struct FenceAges {
    since: HashMap<u64, Instant>,
    warned: HashSet<u64>,
}

impl FenceAges {
    /// Anchors of `fences` older than `limit` not yet reported.
    pub fn overdue(&mut self, fences: &Fences, now: Instant, limit: Duration) -> Vec<u64> {
        self.since
            .retain(|anchor, _| fences.anchors.contains_key(anchor));
        self.warned
            .retain(|anchor| fences.anchors.contains_key(anchor));
        let mut overdue = Vec::new();
        for &anchor in fences.anchors.keys() {
            let since = *self.since.entry(anchor).or_insert(now);
            if now.saturating_duration_since(since) >= limit && self.warned.insert(anchor) {
                overdue.push(anchor);
            }
        }
        overdue
    }
}

/// Absent or unreadable both mean "no fences", as for the baseline.
pub fn load_fences(dir: &std::path::Path) -> Fences {
    let Ok(contents) = std::fs::read(dir.join(FENCES_FILE)) else {
        return Fences::default();
    };
    serde_json::from_slice(&contents).unwrap_or_else(|error| {
        warn!(%error, "unreadable inventory fences; ignoring them");
        Fences::default()
    })
}

/// Best-effort like `save_baseline`; returns whether the fences were saved.
pub fn save_fences(dir: &std::path::Path, fences: &Fences) -> bool {
    let written = serde_json::to_vec(fences)
        .map_err(std::io::Error::other)
        .and_then(|contents| crate::stepd::publish_private(dir, FENCES_FILE, &contents));
    if let Err(error) = &written {
        warn!(%error, dir = %dir.display(), "could not save the inventory fences");
    }
    written.is_ok()
}

/// One refresh tick's view: `effective` is `fresh` with fenced GPUs reduced to
/// their pinned entries, and is what `delta` classifies against `old`.
pub struct TickPlan {
    pub plan: RefreshPlan,
    pub fences: Fences,
    pub effective: ResourceSet,
    pub delta: InventoryDelta,
    fresh_anchors: HashSet<u64>,
    /// Held ids `fresh` no longer names as the same silicon.
    lost_held: HashSet<u64>,
}

impl TickPlan {
    /// Ids in `ledger_ids` to refuse this tick: every old id of a GPU
    /// repartitioned under a holder, from its first sighting.
    pub fn ledger_fenced(&self, committed: &Fences, ledger_ids: &[u64]) -> HashSet<u64> {
        let anchors: HashSet<u64> = committed
            .anchors
            .keys()
            .chain(self.fences.anchors.keys())
            .copied()
            .collect();
        let mut fenced: HashSet<u64> = ledger_ids
            .iter()
            .copied()
            .filter(|&id| {
                let anchor = spur_core::gpu_identity::current_anchor(id, &self.fresh_anchors);
                anchors.contains(&anchor)
            })
            .collect();
        fenced.extend(committed.pinned_ids());
        fenced.extend(self.fences.pinned_ids());
        fenced
    }

    /// Ids reported from the pre-change set rather than the fresh discovery.
    pub fn pinned_ids(&self) -> HashSet<u64> {
        let mut pinned = self.fences.pinned_ids();
        if self.delta == InventoryDelta::AllocatedDevicesLost {
            pinned.extend(&self.lost_held);
        }
        pinned
    }

    /// The set to report: a held device the effective set lost is pinned.
    pub fn reported(&self) -> ResourceSet {
        match self.delta {
            InventoryDelta::AllocatedDevicesLost => {
                reconcile_free_pool(&self.effective, &self.plan.baseline, &self.plan.held)
            }
            _ => self.effective.clone(),
        }
    }
}

/// A repartition is only visible against the layout before it; once the
/// report shrinks to the pinned ids it is not, so the fence must carry it.
pub fn plan_tick(
    old: &ResourceSet,
    fresh: &ResourceSet,
    held: &HashSet<u64>,
    fences: &Fences,
) -> TickPlan {
    // A pinned id must not read as a re-encoding of the GPU's new layout.
    let plan = plan_refresh(old, &fences.apply(fresh), held);
    let mut next = fences.pruned(&plan.held);
    for (anchor, gpu) in repartitioned_held(old, fresh, &plan) {
        next.pin(anchor, gpu);
    }
    let effective = next.apply(fresh);
    let delta = refresh_delta(old, &effective, &plan);
    let ids = |set: &ResourceSet| set.gpus.iter().map(|g| g.stable_id).collect::<Vec<_>>();
    let layout = spur_core::gpu_identity::diff_layout(&ids(&plan.baseline), &ids(fresh));
    let fresh_ids: HashSet<u64> = ids(fresh).into_iter().collect();
    let lost_held = plan
        .held
        .iter()
        .copied()
        .filter(|&id| !layout.is_intact(id, &fresh_ids))
        .collect();
    TickPlan {
        plan,
        fences: next,
        effective,
        delta,
        fresh_anchors: fresh.gpus.iter().map(|g| bdf_anchor(g.stable_id)).collect(),
        lost_held,
    }
}

/// Install `reported` on the ledger, gated by the fences it was built from so
/// the layout just offered is dispatchable; a deferral keeps the wider gate.
pub fn apply_to_ledger(
    ledger: &mut NodeAllocation,
    tick: &TickPlan,
    reported: &ResourceSet,
    spec_cpus: &[u32],
) -> spur_sched::cons_tres::CapacityChange {
    ledger.rename_gpus(&tick.plan.renamed);
    let change = ledger.update_capacity(reported);
    // The rebuild resets the per-CPU allocation bitmap, so re-apply the
    // spec-core exclusions to the current layout.
    ledger.set_spec_exclusions(spec_cpus);
    if change == spur_sched::cons_tres::CapacityChange::Applied {
        let ids: Vec<u64> = ledger.gpus.iter().map(|g| g.stable_id).collect();
        ledger.set_fenced(tick.ledger_fenced(&tick.fences, &ids));
    }
    change
}

/// Held entries of `plan.baseline` on GPUs `fresh` repartitioned, by anchor.
fn repartitioned_held<'a>(
    old: &ResourceSet,
    fresh: &ResourceSet,
    plan: &'a RefreshPlan,
) -> Vec<(u64, &'a GpuResource)> {
    let ids = |set: &ResourceSet| set.gpus.iter().map(|g| g.stable_id).collect::<Vec<_>>();
    let layout = spur_core::gpu_identity::diff_layout(&ids(old), &ids(fresh));
    let anchors: HashSet<u64> = fresh.gpus.iter().map(|g| bdf_anchor(g.stable_id)).collect();
    plan.baseline
        .gpus
        .iter()
        .filter(|g| plan.held.contains(&g.stable_id))
        .map(|g| {
            let anchor = spur_core::gpu_identity::current_anchor(g.stable_id, &anchors);
            (anchor, g)
        })
        .filter(|(anchor, _)| layout.repartitioned.contains(anchor))
        .collect()
}

pub struct StartupInventory {
    pub reported: ResourceSet,
    pub renamed: HashMap<u64, u64>,
    pub fences: Fences,
}

/// Whether anything changed is judged on the unrenamed `old`: once its held
/// ids are renamed it can equal `fresh`, and the node would never converge.
pub fn refresh_delta(old: &ResourceSet, fresh: &ResourceSet, plan: &RefreshPlan) -> InventoryDelta {
    classify(old, fresh, &plan.held)
}

/// What to report at startup while adopted jobs hold `held`, and how their
/// recorded ids map onto `fresh`, via the refresh path against the baseline.
pub fn startup_inventory(
    fresh: &ResourceSet,
    baseline: Option<&ResourceSet>,
    held: &HashSet<u64>,
    fences: &Fences,
) -> StartupInventory {
    let Some(baseline) = baseline else {
        return startup_inventory_without_baseline(fresh, held, fences);
    };
    let tick = plan_tick(baseline, fresh, held, fences);
    StartupInventory {
        reported: tick.reported(),
        renamed: tick.plan.renamed,
        fences: tick.fences,
    }
}

/// Without a baseline a repartition cannot be detected, so a held id missing
/// from `fresh` is pinned on its GPU if that GPU is still there.
fn startup_inventory_without_baseline(
    fresh: &ResourceSet,
    held: &HashSet<u64>,
    fences: &Fences,
) -> StartupInventory {
    let fences = fences.pruned(held);
    let fresh = &fences.apply(fresh);
    let fresh_ids: HashSet<u64> = fresh.gpus.iter().map(|g| g.stable_id).collect();
    let renamed: HashMap<u64, u64> = held
        .iter()
        .filter(|id| !fresh_ids.contains(id))
        .filter_map(|&id| {
            Some((
                id,
                spur_core::gpu_identity::legacy_partition_id(id, &fresh_ids)?,
            ))
        })
        .collect();
    let current = |id: &u64| renamed.get(id).copied().unwrap_or(*id);
    let pinned: Vec<GpuResource> = held
        .iter()
        .map(current)
        .filter(|id| !fresh_ids.contains(id))
        .filter_map(|id| {
            let gpu = fresh
                .gpus
                .iter()
                .find(|g| bdf_anchor(g.stable_id) == bdf_anchor(id))?;
            Some(GpuResource {
                stable_id: id,
                ..gpu.clone()
            })
        })
        .collect();
    if pinned.is_empty() {
        return StartupInventory {
            reported: fresh.clone(),
            renamed,
            fences,
        };
    }
    let pinned_anchors: HashSet<u64> = pinned.iter().map(|g| bdf_anchor(g.stable_id)).collect();
    let mut synthetic = fresh.clone();
    synthetic.gpus.extend(pinned);
    // Only GPUs with a pinned id are fenced; other held GPUs keep their siblings.
    let fenced: HashSet<u64> = held
        .iter()
        .map(current)
        .filter(|&id| pinned_anchors.contains(&bdf_anchor(id)))
        .collect();
    StartupInventory {
        reported: reconcile_free_pool(fresh, &synthetic, &fenced),
        renamed,
        fences,
    }
}

/// Plain GPUs without fabric evidence stay untouched, so a failed scan does not churn their inventory.
pub fn mark_fabric_unavailable(resources: &mut ResourceSet, declared: bool) {
    for gpu in &mut resources.gpus {
        let observed = gpu.topology.as_ref().is_some_and(|t| t.fabric.is_some());
        if declared || observed {
            mark_gpu_fabric_unavailable(gpu);
        }
    }
}

pub fn retain_native_fabric_loss(old: &ResourceSet, fresh: &mut ResourceSet) {
    let observed: std::collections::HashMap<_, _> = old
        .gpus
        .iter()
        .filter_map(|gpu| {
            let fabric = gpu.topology.as_ref()?.fabric.as_ref()?;
            (fabric.source != "device-discovery").then_some((gpu.stable_id, fabric))
        })
        .collect();
    for gpu in &mut fresh.gpus {
        let Some(previous) = observed.get(&gpu.stable_id) else {
            continue;
        };
        let topology = gpu.topology.get_or_insert_with(Default::default);
        if topology.fabric.is_none() {
            // A vanished native source is not an administrator-declared fabric.
            topology.fabric = Some(spur_devices::topology::FabricObservation::unavailable(
                &previous.source,
                "fabric",
                spur_devices::topology::ObservationIssueKind::Missing,
            ));
        }
    }
}

fn mark_gpu_fabric_unavailable(gpu: &mut GpuResource) {
    let topology = gpu.topology.get_or_insert_with(Default::default);
    let source = topology
        .fabric
        .as_ref()
        .map_or("device-discovery", |fabric| fabric.source.as_str());
    topology.fabric = Some(spur_devices::topology::FabricObservation::unavailable(
        source,
        "device",
        spur_devices::topology::ObservationIssueKind::Unavailable,
    ));
}

/// Converge only the free pool without ever advertising silicon a job holds.
/// Keep a fresh device ONLY if its BDF anchor is not shared by any held device;
/// a fresh partition on a held GPU's BDF is a repartition of held silicon and
/// must not be offered as free (that is the physical double-book the reviewer
/// named). Then re-insert every held device from `baseline` (the last-reported
/// set, which still carries it) so the held device stays present and
/// `total_resources ⊇ allocated` holds. Net: for a physical GPU with a held
/// allocation only the baseline held device(s) are advertised; GPUs with no
/// held allocation flow through unchanged.
/// Pure: `fresh`'s cpus/memory/generation are preserved; pinned held GPUs are
/// appended in ascending stable_id order for a deterministic fingerprint.
pub fn reconcile_free_pool(
    fresh: &ResourceSet,
    baseline: &ResourceSet,
    held_ids: &std::collections::HashSet<u64>,
) -> ResourceSet {
    let held_bdfs =
        spur_core::gpu_identity::held_anchors(held_ids, fresh.gpus.iter().map(|g| g.stable_id));

    let mut reconciled = fresh.clone();
    reconciled
        .gpus
        .retain(|g| !held_bdfs.contains(&bdf_anchor(g.stable_id)));

    let present: std::collections::HashSet<u64> =
        reconciled.gpus.iter().map(|g| g.stable_id).collect();
    let mut missing: Vec<u64> = held_ids
        .iter()
        .copied()
        .filter(|id| !present.contains(id))
        .collect();
    missing.sort_unstable();

    for id in missing {
        if let Some(gpu) = baseline.gpus.iter().find(|g| g.stable_id == id) {
            reconciled.gpus.push(pinned_gpu(gpu, fresh));
        }
    }
    reconciled
}

/// Accounting keeps the held identity; observations describe the current device.
fn pinned_gpu(held: &GpuResource, fresh: &ResourceSet) -> GpuResource {
    let mut pinned = held.clone();
    match fresh.gpus.iter().find(|g| g.stable_id == held.stable_id) {
        Some(current) => pinned.topology = current.topology.clone(),
        None => mark_gpu_fabric_unavailable(&mut pinned),
    }
    pinned
}

/// Discover local node resources from sysfs / /proc + device registry.
pub fn discover_resources(registry: &DeviceRegistry) -> ResourceSet {
    let cpus = discover_cpus();
    let memory_mb = discover_memory_mb();
    let gpus = gpus_from_registry(registry);

    ResourceSet {
        cpus,
        memory_mb,
        gpus,
        generic: generic_from_registry(registry),
        // Bumped by the periodic refresh task on topology rebuild, not here.
        generation: 0,
        fabric_binding_version: spur_core::gpu_binding::GPU_PROPERTIES_CAPABILITY,
        block_file_revision: None,
        // Real online topology so the local allocation ledger indexes jobs by
        // real CPU IDs, not a synthetic 0..cpus that can hand out offline CPUs.
        topology: discover_cpu_topology(Path::new("/sys")),
    }
}

pub fn refresh_block_file(resources: &mut ResourceSet, path: Option<&std::path::Path>) {
    if spur_core::fabric::has_native_fabric(resources) {
        resources.block_file_revision = None;
        return;
    }
    resources.block_file_revision =
        path.and_then(|path| match spur_core::block_file::BlockFile::read(path) {
            Ok(file) => Some(file.revision),
            Err(error) => {
                warn!(%error, "block file unavailable; declared block placement disabled");
                None
            }
        });
}

fn build_peer_gpus(links: Option<&[i32]>, gpu_device_ids: &[u32]) -> Vec<u32> {
    let Some(links) = links else {
        return Vec::new();
    };
    links
        .iter()
        .enumerate()
        .filter_map(|(j, &w)| {
            if w > 0 && j < gpu_device_ids.len() {
                Some(gpu_device_ids[j])
            } else {
                None
            }
        })
        .collect()
}

/// Convert injectable GPU registry entries to `GpuResource` for the scheduler.
fn gpus_from_registry(registry: &DeviceRegistry) -> Vec<GpuResource> {
    let gpu_entries: Vec<_> = registry
        .list()
        .iter()
        .filter(|e| e.is_injectable() && e.gres_name == "gpu")
        .collect();

    let gpu_device_ids: Vec<u32> = gpu_entries.iter().map(|e| e.device_id).collect();

    gpu_entries
        .iter()
        .map(|entry| GpuResource {
            device_id: entry.device_id,
            gpu_type: entry.resource_type.clone().unwrap_or_default(),
            memory_mb: entry.memory_mb,
            peer_gpus: build_peer_gpus(entry.links.as_deref(), &gpu_device_ids),
            link_type: link_type_to_gpu(resolve_link_type(entry)),
            topology: entry.topology.clone(),
            stable_id: entry.stable_id,
        })
        .collect()
}

fn generic_from_registry(registry: &DeviceRegistry) -> HashMap<String, u64> {
    let mut generic = HashMap::new();
    for entry in registry.list() {
        if !entry.is_countable_pool() {
            continue;
        }
        let key = match &entry.resource_type {
            Some(t) if !t.is_empty() => format!("{}:{}", entry.gres_name, t),
            _ => entry.gres_name.clone(),
        };
        *generic.entry(key).or_insert(0) += entry.capacity;
    }
    generic
}

fn link_type_to_gpu(lt: LinkType) -> GpuLinkType {
    match lt {
        LinkType::Xgmi => GpuLinkType::XGMI,
        LinkType::Nvlink => GpuLinkType::NVLink,
        LinkType::Pcie => GpuLinkType::PCIe,
    }
}

/// Count online CPUs from sysfs.
fn discover_cpus() -> u32 {
    // Try /sys/devices/system/cpu/online first
    if let Ok(online) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        if let Some(count) = parse_cpu_range(online.trim()) {
            return count;
        }
    }

    // Fallback: count /proc/cpuinfo processors
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        return cpuinfo
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count() as u32;
    }

    // Last resort
    num_cpus()
}

/// This node's online CPUs grouped by physical core, from sysfs. `None` when
/// the online set or a CPU's `core_id` is unreadable.
fn discover_cpu_topology(sys_root: &Path) -> Option<spur_core::cpu_topology::CpuTopology> {
    use spur_core::cpu_topology::{CpuRecord, CpuTopology};

    let cpu_base = sys_root.join("devices/system/cpu");
    let online = std::fs::read_to_string(cpu_base.join("online")).ok()?;
    let cpu_ids = spur_core::cpu_topology::parse_cpu_list(online.trim()).ok()?;

    let numa = numa_membership(&sys_root.join("devices/system/node"));
    let mut records = Vec::with_capacity(cpu_ids.len());
    for cpu in cpu_ids {
        let topo = cpu_base.join(format!("cpu{cpu}")).join("topology");
        let core_id = read_sysfs_u32(&topo.join("core_id"))?;
        let socket = read_sysfs_u32(&topo.join("physical_package_id")).unwrap_or(0);
        records.push(CpuRecord {
            cpu,
            socket,
            core_id,
            numa_node: numa.get(&cpu).copied(),
        });
    }
    Some(CpuTopology::from_cpus(&records))
}

fn read_sysfs_u32(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The `cpuset.cpus` of each live job cgroup under the spur root, for the
/// enforcer to reconcile the root against (so a shrinking root never empties a
/// running job's set). A job with no cpuset bound contributes nothing.
fn read_live_job_cpusets(root: &Path) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.starts_with("job_") {
            continue;
        }
        if let Ok(list) = std::fs::read_to_string(entry.path().join("cpuset.cpus")) {
            if let Ok(cpus) = spur_core::cpu_topology::parse_cpu_list(list.trim()) {
                if !cpus.is_empty() {
                    out.push(cpus);
                }
            }
        }
    }
    out
}

/// Pin every thread of spurd to `cpus`. Best-effort: the caller logs a failure.
fn pin_current_process_threads(cpus: &[u32]) -> std::io::Result<()> {
    let mut set = nix::sched::CpuSet::new();
    for &cpu in cpus {
        set.set(cpu as usize)
            .map_err(|e| std::io::Error::other(format!("cpu {cpu}: {e}")))?;
    }
    for tid in current_process_tids() {
        // A thread that died since we listed it is fine to skip.
        let _ = nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(tid), &set);
    }
    Ok(())
}

fn current_process_tids() -> Vec<i32> {
    let mut tids = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
        for entry in entries.flatten() {
            if let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            {
                tids.push(tid);
            }
        }
    }
    tids
}

/// Map each online CPU to its NUMA node by reading `node<N>/cpulist`.
fn numa_membership(node_base: &Path) -> std::collections::HashMap<u32, u32> {
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(node_base) else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(node) = name
            .to_str()
            .and_then(|n| n.strip_prefix("node"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(list) = std::fs::read_to_string(entry.path().join("cpulist")) {
            if let Ok(cpus) = spur_core::cpu_topology::parse_cpu_list(list.trim()) {
                for cpu in cpus {
                    map.insert(cpu, node);
                }
            }
        }
    }
    map
}

/// Parse "0-191" or "0-63,128-191" into a total count.
fn parse_cpu_range(s: &str) -> Option<u32> {
    let mut count = 0u32;
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start_s, end_s)) = part.split_once('-') {
            let start: u32 = start_s.parse().ok()?;
            let end: u32 = end_s.parse().ok()?;
            count += end - start + 1;
        } else {
            let _: u32 = part.parse().ok()?;
            count += 1;
        }
    }
    Some(count)
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// Read total memory from /proc/meminfo.
fn discover_memory_mb() -> u64 {
    read_mem_total_mb().unwrap_or(0)
}

/// `MemTotal` in MB, `None` when `/proc/meminfo` cannot be read or parsed.
fn read_mem_total_mb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let rest = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kb = rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    Some(kb / 1024)
}

/// Read current load average and free memory.
fn read_system_metrics() -> (u32, u64) {
    let load = read_load_avg();
    let free_mem = read_free_memory_mb();
    (load, free_mem)
}

fn read_load_avg() -> u32 {
    if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(first) = loadavg.split_whitespace().next() {
            if let Ok(load) = first.parse::<f64>() {
                return (load * 100.0) as u32;
            }
        }
    }
    0
}

fn read_free_memory_mb() -> u64 {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let rest = rest.trim();
                if let Some(kb_str) = rest.strip_suffix("kB") {
                    if let Ok(kb) = kb_str.trim().parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
    }
    0
}

pub fn allocations_to_proto(
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
        generation: r.generation,
        gpu_binding: r.gpu_binding.as_ref().map(spur_core::gpu_binding::to_proto),
        fabric_binding: r
            .fabric_binding
            .as_ref()
            .map(spur_core::fabric_binding::to_proto),
    }
}

pub fn resource_to_proto(r: &ResourceSet) -> ProtoResourceSet {
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
                topology: g
                    .topology
                    .as_ref()
                    .map(spur_core::device_topology::to_proto),
                stable_id: g.stable_id,
            })
            .collect(),
        generic: r.generic.clone(),
        generation: r.generation,
        fabric_binding_version: r.fabric_binding_version,
        block_file_revision: r.block_file_revision.clone(),
    }
}

#[cfg(test)]
mod tests {
    fn observed_topology(domain: &str) -> spur_devices::topology::DeviceTopology {
        use spur_devices::topology::*;
        DeviceTopology {
            pci_bdf: Some("0001:04:00.0".into()),
            numa_node: None,
            pci_root: Some("pci0001:00".into()),
            vram_bytes: Some(309_237_645_312),
            fabric: Some(FabricObservation {
                source: "amd-ualink".into(),
                status: ObservationStatus::Consistent,
                physical_domain_id: Some("00000000-0000-4000-8000-000000000001".into()),
                fabric_domain_id: Some(domain.into()),
                endpoint_id: Some("63".into()),
                physical_capacity: Some(72),
                fabric_capacity: Some(4),
                active_endpoints: vec!["60".into(), "61".into(), "62".into(), "63".into()],
                local_endpoints: vec!["60".into(), "61".into(), "62".into(), "63".into()],
                endpoint_state: Some("active".into()),
                transport: Some("UALoE".into()),
                issues: vec![],
            }),
        }
    }

    #[test]
    fn membership_only_refresh_is_debounced_by_the_observed_membership() {
        let mut baseline = ResourceSet {
            gpus: vec![gpu(0, 128)],
            ..Default::default()
        };
        baseline.gpus[0].topology = Some(observed_topology("15"));
        let mut first = baseline.clone();
        first.gpus[0].topology = Some(observed_topology("16"));
        let delta = classify(&baseline, &first, &Default::default());
        assert_eq!(delta, InventoryDelta::FreeCapacityChanged);
        let mut next = first.clone();
        next.gpus[0].topology = Some(observed_topology("17"));
        assert_eq!(
            next_refresh_action(
                &delta,
                &next.schedulable_fingerprint(),
                &delta,
                &first.schedulable_fingerprint()
            )
            .0,
            RefreshAction::Wait
        );
        assert_eq!(
            next_refresh_action(
                &delta,
                &next.schedulable_fingerprint(),
                &delta,
                &next.schedulable_fingerprint()
            )
            .0,
            RefreshAction::ApplyCapacity
        );
    }

    #[test]
    fn pinned_accounting_does_not_pin_stale_observations() {
        use spur_devices::topology::ObservationStatus;
        let mut baseline = ResourceSet {
            gpus: vec![gpu(0, 128)],
            ..Default::default()
        };
        baseline.gpus[0].topology = Some(observed_topology("15"));
        let mut fresh = baseline.clone();
        fresh.gpus[0].topology = Some(observed_topology("16"));
        let held = [128].into_iter().collect();
        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        assert_eq!(reconciled.gpus[0].topology, fresh.gpus[0].topology);
        fresh.gpus[0].topology = None;
        assert!(reconcile_free_pool(&fresh, &baseline, &held).gpus[0]
            .topology
            .is_none());
        fresh.gpus.clear();
        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        assert_eq!(reconciled.gpus[0].stable_id, 128);
        let fabric = reconciled.gpus[0]
            .topology
            .as_ref()
            .unwrap()
            .fabric
            .as_ref()
            .unwrap();
        assert_eq!(fabric.status, ObservationStatus::Unavailable);
        assert!(fabric.fabric_domain_id.is_none());
    }

    #[test]
    fn full_discovery_failure_keeps_capacity_but_invalidates_fabric() {
        use spur_devices::topology::ObservationStatus;
        let mut baseline = ResourceSet {
            gpus: vec![gpu(0, 128)],
            cpus: 8,
            memory_mb: 1024,
            ..Default::default()
        };
        baseline.gpus[0].topology = Some(observed_topology("15"));
        let mut unavailable = baseline.clone();
        mark_fabric_unavailable(&mut unavailable, false);
        assert_eq!(unavailable.gpus[0].stable_id, 128);
        assert_eq!(unavailable.cpus, 8);
        assert_eq!(unavailable.memory_mb, 1024);
        assert_eq!(
            unavailable.gpus[0]
                .topology
                .as_ref()
                .unwrap()
                .fabric
                .as_ref()
                .unwrap()
                .status,
            ObservationStatus::Unavailable
        );
        assert_ne!(
            unavailable.schedulable_fingerprint(),
            baseline.schedulable_fingerprint()
        );
        let previous = unavailable.clone();
        mark_fabric_unavailable(&mut unavailable, false);
        assert_eq!(previous, unavailable);
    }

    #[test]
    fn discovery_failure_leaves_plain_gpu_inventory_unchanged() {
        let baseline = ResourceSet {
            gpus: vec![gpu(0, 128)],
            cpus: 8,
            memory_mb: 1024,
            generation: 7,
            ..Default::default()
        };
        let mut retained = baseline.clone();
        retained.generation += 1;
        mark_fabric_unavailable(&mut retained, false);
        assert!(retained.gpus[0].topology.is_none());
        assert_eq!(
            classify(&baseline, &retained, &Default::default()),
            InventoryDelta::Unchanged
        );
        assert_eq!(
            retained.schedulable_fingerprint(),
            baseline.schedulable_fingerprint()
        );
        mark_fabric_unavailable(&mut retained, true);
        assert!(retained.gpus[0].topology.is_some());
    }

    fn declaration_only_node(
        with_topology: bool,
    ) -> (spur_core::block_file::BlockFile, spur_core::node::Node) {
        let file = spur_core::block_file::BlockFile::parse("BlockName=a Nodes=n0").unwrap();
        let mut inventory = ResourceSet {
            cpus: 8,
            memory_mb: 1024,
            generation: 7,
            fabric_binding_version: 2,
            block_file_revision: Some(file.revision.clone()),
            gpus: vec![gpu(0, 256), gpu(1, 512)],
            ..Default::default()
        };
        if with_topology {
            for gpu in &mut inventory.gpus {
                gpu.topology = Some(spur_core::resource::DeviceTopology {
                    numa_node: Some(1),
                    ..Default::default()
                });
            }
        }
        let mut node = spur_core::node::Node::new("n0".into(), inventory);
        node.state = spur_core::node::NodeState::Idle;
        node.last_heartbeat = Some("2023-11-14T22:13:20Z".parse().unwrap());
        (file, node)
    }

    fn declaration_only_view(
        file: &spur_core::block_file::BlockFile,
        node: &spur_core::node::Node,
    ) -> spur_core::fabric::FabricView {
        file.view(
            "test",
            std::slice::from_ref(node),
            node.last_heartbeat.unwrap(),
            90,
            true,
        )
    }

    #[test]
    fn ignored_file_changes_do_not_trigger_native_inventory_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("topology.conf");
        let file = spur_core::block_file::BlockFile::parse("BlockName=a Nodes=n0").unwrap();
        let (_, mut node) = declaration_only_node(false);
        node.total_resources.gpus.truncate(1);
        node.total_resources.gpus[0].topology = Some(observed_topology("15"));
        refresh_block_file(&mut node.total_resources, Some(&path));
        let baseline = node.total_resources.clone();
        assert!(baseline.block_file_revision.is_none());
        for text in [
            Some(file.to_text()),
            Some("BlockName=b Nodes=elsewhere".into()),
            Some("malformed".into()),
            None,
        ] {
            if let Some(text) = text {
                std::fs::write(&path, text).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            refresh_block_file(&mut node.total_resources, Some(&path));
            assert_eq!(
                classify(&baseline, &node.total_resources, &Default::default()),
                InventoryDelta::Unchanged
            );
            assert_eq!(
                baseline.schedulable_fingerprint(),
                node.total_resources.schedulable_fingerprint()
            );
        }
        node.total_resources.gpus[0].topology = None;
        std::fs::write(&path, file.to_text()).unwrap();
        refresh_block_file(&mut node.total_resources, Some(&path));
        assert_eq!(
            node.total_resources.block_file_revision.as_deref(),
            Some(file.revision.as_str())
        );
        std::fs::remove_file(&path).unwrap();
        refresh_block_file(&mut node.total_resources, Some(&path));
        assert!(node.total_resources.block_file_revision.is_none());
    }

    #[test]
    fn vanished_native_source_stays_unavailable_until_discovery_recovers() {
        let (file, mut node) = declaration_only_node(true);
        node.total_resources.gpus[0].topology = Some(observed_topology("15"));
        let baseline = node.total_resources.clone();
        node.total_resources.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric = None;
        retain_native_fabric_loss(&baseline, &mut node.total_resources);
        assert!(spur_core::fabric::has_native_fabric(&node.total_resources));
        let lost = node.total_resources.clone();
        let fabric = lost.gpus[0]
            .topology
            .as_ref()
            .unwrap()
            .fabric
            .as_ref()
            .unwrap();
        assert_eq!(
            fabric.status,
            spur_core::fabric::ObservationStatus::Unavailable
        );
        assert_eq!(lost.gpus[0].stable_id, baseline.gpus[0].stable_id);
        let view = declaration_only_view(&file, &node);
        assert!(view.blocks.is_empty());
        assert!(view.notices[0].warning);
        node.total_resources.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric = None;
        retain_native_fabric_loss(&lost, &mut node.total_resources);
        assert_eq!(node.total_resources, lost);
        let mut recovered = baseline.clone();
        retain_native_fabric_loss(&lost, &mut recovered);
        assert_eq!(recovered, baseline);
        let mut declaration = baseline.clone();
        for gpu in &mut declaration.gpus {
            gpu.topology = None;
        }
        let mut unchanged = declaration.clone();
        retain_native_fabric_loss(&declaration, &mut unchanged);
        assert_eq!(unchanged, declaration);
    }

    #[test]
    fn declaration_only_full_dropout_invalidates_every_retained_gpu() {
        for with_topology in [false, true] {
            let (file, mut node) = declaration_only_node(with_topology);
            assert!(declaration_only_view(&file, &node).blocks[0]
                .issues
                .is_empty());
            let baseline = node.total_resources.clone();
            let mut unavailable = baseline.clone();
            unavailable.generation += 1;
            mark_fabric_unavailable(&mut unavailable, true);
            assert_eq!(
                classify(&baseline, &unavailable, &Default::default()),
                InventoryDelta::FreeCapacityChanged
            );
            assert_eq!(
                unavailable
                    .gpus
                    .iter()
                    .map(|g| g.stable_id)
                    .collect::<Vec<_>>(),
                [256, 512]
            );
            assert_eq!((unavailable.cpus, unavailable.memory_mb), (8, 1024));
            for gpu in &unavailable.gpus {
                let topology = gpu.topology.as_ref().unwrap();
                assert_eq!(
                    topology.fabric.as_ref().unwrap().status,
                    spur_devices::topology::ObservationStatus::Unavailable
                );
                assert_eq!(topology.numa_node, with_topology.then_some(1));
            }
            let fp = unavailable.schedulable_fingerprint();
            let delta = classify(&baseline, &unavailable, &Default::default());
            assert_eq!(
                next_refresh_action(&delta, &fp, &delta, &fp).0,
                RefreshAction::ApplyCapacity
            );
            let repeated = unavailable.clone();
            mark_fabric_unavailable(&mut unavailable, true);
            assert_eq!(repeated, unavailable);
            node.total_resources = unavailable;
            assert!(!declaration_only_view(&file, &node).blocks[0]
                .issues
                .is_empty());
        }
    }

    #[test]
    fn declaration_only_partial_loss_keeps_accounting_but_invalidates_missing_held_gpu() {
        for with_topology in [false, true] {
            let (file, mut node) = declaration_only_node(with_topology);
            let view = declaration_only_view(&file, &node);
            let baseline = node.total_resources.clone();
            let allocation = spur_core::resource::ResourceAllocations {
                generation: baseline.generation,
                devices: [(
                    "gpu".into(),
                    vec![spur_core::resource::AllocatedDevice::injectable(256)],
                )]
                .into(),
                ..Default::default()
            };
            let binding = spur_core::fabric_binding::FabricBinding::for_allocation(
                &view.blocks[0],
                "n0",
                &allocation,
            )
            .unwrap();
            binding
                .validate("test", "n0", &baseline, &allocation)
                .unwrap();
            let held = [256].into_iter().collect();
            let mut fresh = baseline.clone();
            fresh.gpus.retain(|g| g.stable_id != 256);
            assert_eq!(
                classify(&baseline, &fresh, &held),
                InventoryDelta::AllocatedDevicesLost
            );
            let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
            assert_ne!(
                reconciled.schedulable_fingerprint(),
                baseline.schedulable_fingerprint()
            );
            assert_eq!(reconciled.gpus.len(), 2);
            assert_eq!(
                reconciled.gpus.iter().find(|g| g.stable_id == 512),
                baseline.gpus.iter().find(|g| g.stable_id == 512)
            );
            let missing = reconciled.gpus.iter().find(|g| g.stable_id == 256).unwrap();
            assert_eq!(
                missing
                    .topology
                    .as_ref()
                    .unwrap()
                    .fabric
                    .as_ref()
                    .unwrap()
                    .status,
                spur_devices::topology::ObservationStatus::Unavailable
            );
            assert!(binding
                .validate("test", "n0", &reconciled, &allocation)
                .is_err());
            node.total_resources = reconciled;
            assert!(!declaration_only_view(&file, &node).blocks[0]
                .issues
                .is_empty());
            let repeated = reconcile_free_pool(&fresh, &node.total_resources, &held);
            assert_eq!(
                repeated.schedulable_fingerprint(),
                node.total_resources.schedulable_fingerprint()
            );
            let recovered = reconcile_free_pool(&baseline, &node.total_resources, &held);
            assert_eq!(
                recovered.schedulable_fingerprint(),
                baseline.schedulable_fingerprint()
            );
        }
    }

    #[test]
    fn topology_survives_cdi_registry_and_agent_registration() {
        let expected = observed_topology("15");
        let mut metadata = spur_devices::DeviceMetadata::from_annotations(&Default::default());
        metadata.topology = Some(expected.clone());
        metadata.stable_id = Some(128);
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: metadata.to_annotations(),
                container_edits: None,
            }],
            container_edits: None,
        };
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut registry = DeviceRegistry::new();
        registry.populate(&cache, &GresCache::from_entries(&[]));
        let inventory = ResourceSet {
            gpus: gpus_from_registry(&registry),
            ..Default::default()
        };
        assert_eq!(inventory.gpus[0].topology.as_ref(), Some(&expected));
        let proto = resource_to_proto(&inventory);
        let restored =
            spur_core::device_topology::from_proto(proto.gpus[0].topology.clone().unwrap());
        assert_eq!(restored, expected);
    }

    use super::*;
    use spur_devices::cdi::annotations;
    use spur_devices::cdi::cache::CdiCache;
    use spur_devices::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, DeviceNode};
    use spur_devices::{DeviceRegistry, GresCache, GresEntry};

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn discover_cpu_topology_groups_real_ids_by_core_and_numa() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path();
        let cpu = sys.join("devices/system/cpu");
        // Two cores, two hyperthreads each, split across two sockets/NUMA nodes.
        write_file(&cpu.join("online"), "0-1,64-65\n");
        for (id, core, pkg) in [(0u32, 0u32, 0u32), (1, 0, 0), (64, 0, 1), (65, 0, 1)] {
            write_file(
                &cpu.join(format!("cpu{id}/topology/core_id")),
                &core.to_string(),
            );
            write_file(
                &cpu.join(format!("cpu{id}/topology/physical_package_id")),
                &pkg.to_string(),
            );
        }
        let node = sys.join("devices/system/node");
        write_file(&node.join("node0/cpulist"), "0-1");
        write_file(&node.join("node1/cpulist"), "64-65");

        let topo = discover_cpu_topology(sys).expect("topology");
        assert_eq!(topo.online_cpus(), vec![0, 1, 64, 65]);
        assert_eq!(topo.cores.len(), 2);
        assert_eq!(topo.cores[0].cpus, vec![0, 1]);
        assert_eq!(topo.cores[0].socket, 0);
        assert_eq!(topo.cores[0].numa_node, Some(0));
        assert_eq!(topo.cores[1].cpus, vec![64, 65]);
        assert_eq!(topo.cores[1].socket, 1);
        assert_eq!(topo.cores[1].numa_node, Some(1));
    }

    #[test]
    fn discover_cpu_topology_is_none_without_online_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_cpu_topology(dir.path()).is_none());
    }

    #[test]
    fn read_live_job_cpusets_reads_each_job_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(&root.join("job_7_0/cpuset.cpus"), "0-2");
        write_file(&root.join("job_9_1/cpuset.cpus"), "4,5");
        write_file(&root.join("not_a_job/cpuset.cpus"), "8");
        let mut sets = read_live_job_cpusets(root);
        sets.sort();
        assert_eq!(sets, vec![vec![0, 1, 2], vec![4, 5]]);
    }

    fn stable_registry(devices: &[(u64, &str)]) -> DeviceRegistry {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: devices
                .iter()
                .map(|&(stable_id, path)| CdiDevice {
                    name: format!("{stable_id:x}"),
                    annotations: [(annotations::STABLE_ID.into(), stable_id.to_string())].into(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: path.into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: None,
        };
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut reg = DeviceRegistry::new();
        reg.populate(&cache, &GresCache::from_entries(&[]));
        reg
    }

    #[test]
    fn a_renamed_held_gpu_injects_its_new_entry_while_a_lost_one_keeps_its_old() {
        use std::collections::HashSet;
        let old_rank1 = (0x11 << 16) | (1 << 8);
        let old = rset(
            8,
            1,
            vec![
                gpu(0, sid(0x11, 0)),
                gpu(1, old_rank1),
                gpu(2, sid(0x12, 0)),
            ],
        );
        let fresh = rset(8, 2, vec![gpu(0, sid(0x11, 0)), gpu(1, sid(0x11, 1))]);
        let held: HashSet<u64> = [old_rank1, sid(0x12, 0)].into();
        let tick = plan_tick(&old, &fresh, &held, &Fences::default());
        assert_eq!(tick.delta, InventoryDelta::AllocatedDevicesLost);
        assert_eq!(tick.pinned_ids(), HashSet::from([sid(0x12, 0)]));

        let previous = stable_registry(&[
            (sid(0x11, 0), "/dev/dri/renderD128"),
            (old_rank1, "/dev/dri/renderD129"),
            (sid(0x12, 0), "/dev/dri/renderD130"),
        ]);
        let candidate = stable_registry(&[
            (sid(0x11, 0), "/dev/dri/renderD200"),
            (sid(0x11, 1), "/dev/dri/renderD201"),
        ]);
        let reported: HashSet<u64> = tick.reported().gpus.iter().map(|g| g.stable_id).collect();
        let published = candidate.with_pinned(&previous, "gpu", &reported, &tick.pinned_ids());

        let path = |id: u64| {
            published.resolve_by_ids("gpu", &[id]).unwrap()[0]
                .device_paths
                .clone()
        };
        assert_eq!(path(sid(0x11, 1)), vec!["/dev/dri/renderD201".to_string()]);
        assert_eq!(path(sid(0x12, 0)), vec!["/dev/dri/renderD130".to_string()]);
    }

    #[test]
    fn test_gpus_from_registry_link_type() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [
                    (annotations::GPU_TYPE.into(), "mi300x".into()),
                    (annotations::LINK_TYPE.into(), "xgmi".into()),
                ]
                .into(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/dri/renderD128".into(),
                        host_path: None,
                        r#type: None,
                        major: None,
                        minor: None,
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut reg = DeviceRegistry::new();
        reg.populate(&cache, &GresCache::from_entries(&[]));

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].link_type, GpuLinkType::XGMI);
    }

    #[test]
    fn gpus_from_registry_sets_stable_id_from_annotation() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [
                    (annotations::GPU_TYPE.into(), "mi300x".into()),
                    (annotations::STABLE_ID.into(), "129".into()),
                ]
                .into(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/dri/renderD129".into(),
                        host_path: None,
                        r#type: None,
                        major: None,
                        minor: None,
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut reg = DeviceRegistry::new();
        reg.populate(&cache, &GresCache::from_entries(&[]));

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].stable_id, 129);
    }

    #[test]
    fn test_build_peer_gpus_from_links() {
        let gpu_device_ids = [0, 1, 2, 3];
        let links = [-1, 4, 4, 0];
        assert_eq!(build_peer_gpus(Some(&links), &gpu_device_ids), vec![1, 2]);
    }

    #[test]
    fn test_build_peer_gpus_none_links() {
        let gpu_device_ids = [0, 1, 2];
        assert!(build_peer_gpus(None, &gpu_device_ids).is_empty());
    }

    #[test]
    fn test_gpus_from_registry_populates_peer_gpus() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-130]".into()),
            links: Some("-1,4,4,0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 3);
        for gpu in &gpus {
            assert_eq!(gpu.peer_gpus, vec![1, 2]);
        }
    }

    #[test]
    fn test_gpus_from_registry_link_type_inferred_from_gres_links() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-129]".into()),
            links: Some("-1,4,4,0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].link_type, GpuLinkType::XGMI);
        assert_eq!(gpus[1].link_type, GpuLinkType::XGMI);
    }

    #[test]
    fn test_parse_cpu_range() {
        assert_eq!(parse_cpu_range("0-191"), Some(192));
        assert_eq!(parse_cpu_range("0-63,128-191"), Some(128));
        assert_eq!(parse_cpu_range("0"), Some(1));
        assert_eq!(parse_cpu_range("0-3"), Some(4));
    }

    #[test]
    fn test_discover_cpus() {
        let cpus = discover_cpus();
        assert!(cpus > 0);
    }

    #[test]
    fn test_discover_memory() {
        let mem = discover_memory_mb();
        assert!(mem > 0);
    }

    #[test]
    fn test_discover_resources_includes_countable_gres() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "bandwidth".into(),
            r#type: Some("lustre".into()),
            count: Some(4096),
            flags: vec!["count_only".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let resources = discover_resources(&reg);
        assert_eq!(resources.generic.get("bandwidth:lustre"), Some(&4096));
    }

    #[test]
    fn should_reregister_on_not_found() {
        assert!(should_reregister(&tonic::Status::not_found(
            "node x not found — is the node registered?"
        )));
    }

    #[test]
    fn should_not_reregister_on_other_errors() {
        assert!(!should_reregister(&tonic::Status::unavailable(
            "transport error"
        )));
        assert!(!should_reregister(&tonic::Status::unauthenticated(
            "node token required"
        )));
        assert!(!should_reregister(&tonic::Status::internal("boom")));
    }

    #[test]
    fn a_missing_node_credential_warns_but_does_not_block_registration() {
        // Supervision no longer depends on the credential; only the
        // controller-verified recovery handshake does.
        warn_without_node_identity("");
        warn_without_node_identity("node-credential");
    }

    #[test]
    fn build_running_jobs_carries_each_jobs_translated_gpu_ids() {
        // A held job reports the (translated) stable_ids its allocation owns; a
        // held job with no GPUs reports an empty list.
        let held = vec![(7u32, 1u32), (9u32, 3u32)];
        let gpu_ids: HashMap<u32, Vec<u64>> = [(7u32, vec![0x63_0000u64, 0x83_0000u64])]
            .into_iter()
            .collect();

        let mut jobs = build_running_jobs(held, &gpu_ids);
        jobs.sort_by_key(|j| j.job_id);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].job_id, 7);
        assert_eq!(jobs[0].gpu_stable_ids, vec![0x63_0000, 0x83_0000]);
        assert_eq!(jobs[1].job_id, 9);
        assert_eq!(
            (jobs[0].run_attempt, jobs[1].run_attempt),
            (1, 3),
            "each job names the run it holds"
        );
        assert!(
            jobs[1].gpu_stable_ids.is_empty(),
            "a job with no GPUs carries an empty list"
        );
    }

    #[test]
    fn held_job_gpu_ids_reads_translated_ids_from_the_allocation() {
        use spur_sched::cons_tres::NodeAllocation;
        // The authoritative post-translation source is the allocation's owners:
        // an adopted job's restore recorded the current stable_ids there, so the
        // reporter reads those, not the legacy positional descriptor ids.
        let sid_a: u64 = 0x63_0000;
        let sid_b: u64 = 0x83_0000;
        let inv = rset(8, 0, vec![gpu(0, sid_a), gpu(1, sid_b)]);
        let mut node = NodeAllocation::new("n".into(), &inv);
        node.restore_for_job(7, 0, &[], 0, &[sid_a, sid_b]).unwrap();

        let reporter = test_reporter(inv);
        reporter.set_allocation(Arc::new(Mutex::new(node)));

        let map = reporter.held_job_gpu_ids();
        let mut ids = map.get(&7).cloned().unwrap_or_default();
        ids.sort_unstable();
        assert_eq!(ids, vec![sid_a, sid_b]);
    }

    #[test]
    fn held_job_gpu_ids_empty_when_allocation_unwired() {
        let reporter = test_reporter(rset(8, 0, vec![]));
        assert!(reporter.held_job_gpu_ids().is_empty());
    }

    struct CellRun(std::cell::Cell<u32>);

    impl HeldRun for CellRun {
        fn run_attempt(&self) -> u32 {
            self.0.get()
        }
    }

    #[test]
    fn held_jobs_accepts_send_but_not_sync_values() {
        // Cell is Send but !Sync; this fails to compile if the bound re-tightens to Sync.
        let map: Mutex<HashMap<u32, CellRun>> =
            Mutex::new(HashMap::from([(7, CellRun(std::cell::Cell::new(2)))]));
        assert_eq!(map.held_jobs(), vec![(7, 2)]);
    }

    fn test_reporter(resources: ResourceSet) -> NodeReporter {
        test_reporter_with_addr(resources, "http://localhost:6817".into())
    }

    fn test_reporter_with_addr(resources: ResourceSet, controller_addr: String) -> NodeReporter {
        NodeReporter::new(
            "test-node".into(),
            controller_addr,
            resources,
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "test-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            HashMap::new(),
            String::new(),
            String::new(),
            std::path::PathBuf::from("/etc/wireguard"),
            Arc::new(Mutex::new(HashMap::<u32, CellRun>::new())),
            ReporterTimeouts::default(),
        )
    }

    #[test]
    fn registration_reports_boot_without_certifying_an_uninitialized_inventory() {
        let mut reporter = test_reporter(rset(8, 1024, vec![]));
        reporter.boot_time = boot_time_from_stat("cpu 1 2 3\nbtime 1700000000\nprocesses 9\n");
        let request = reporter.registration_request(&reporter.snapshot_resources());
        assert_eq!(
            request.boot_time,
            Some(prost_types::Timestamp {
                seconds: 1700000000,
                nanos: 0
            })
        );
        assert_eq!(request.no_held_jobs, None);
        for stat in ["", "btime invalid", "btime 0", "btime -1"] {
            assert_eq!(boot_time_from_stat(stat), None);
        }
    }

    #[test]
    fn registration_request_advertises_signal_job() {
        let reporter = test_reporter(rset(8, 0, vec![]));
        let req = reporter.registration_request(&rset(8, 0, vec![]));
        assert_eq!(req.capabilities, vec!["signal-job".to_string()]);
    }

    #[test]
    fn dial_endpoints_prefers_original_order_once_the_cooldown_lapses() {
        let reporter = test_reporter_with_addr(rset(8, 0, vec![]), "http://a:1,http://b:1".into());
        *reporter.last_failed_endpoint.lock().unwrap() =
            Some(("http://a:1".into(), Instant::now() - Duration::from_secs(1)));
        assert_eq!(reporter.dial_endpoints(), "http://a:1,http://b:1");
    }

    #[test]
    fn dial_endpoints_deprioritizes_a_still_cooling_down_endpoint() {
        let reporter = test_reporter_with_addr(rset(8, 0, vec![]), "http://a:1,http://b:1".into());
        *reporter.last_failed_endpoint.lock().unwrap() = Some((
            "http://a:1".into(),
            Instant::now() + Duration::from_secs(30),
        ));
        assert_eq!(reporter.dial_endpoints(), "http://b:1,http://a:1");
    }

    /// Accept a TCP connection and never read or write on it again — the shape of
    /// the production incident this fix targets.
    async fn stuck_listener() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _stream = stream;
            std::future::pending::<()>().await
        });
        addr
    }

    /// A real tonic server that doesn't implement SlurmController, so any RPC on it
    /// answers fast with an error instead of hanging — a stand-in for a healthy peer.
    async fn responsive_non_controller_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let (_health_reporter, health) = tonic_health::server::health_reporter();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(health)
                .serve_with_incoming(incoming)
                .await;
        });
        addr
    }

    async fn heartbeat_op(
        mut client: crate::controller_auth::ControllerClient,
    ) -> Result<tonic::Response<spur_proto::proto::HeartbeatResponse>, tonic::Status> {
        client
            .heartbeat(spur_proto::proto::HeartbeatRequest::default())
            .await
    }

    /// The RPC status code behind a dial/app-level failure, so a test can prove
    /// which peer actually answered instead of only inferring it from timing.
    fn err_code(err: &crate::controller_auth::ConnectAuthError) -> tonic::Code {
        match err {
            crate::controller_auth::ConnectAuthError::Status(s) => s.code(),
            crate::controller_auth::ConnectAuthError::Transport(_) => {
                panic!("expected an app-level status, got a transport error: {err}")
            }
        }
    }

    /// Bind then release a port, yielding an address that refuses connections fast —
    /// distinct from `stuck_listener`, which accepts but never answers.
    async fn refusing_addr() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        listener.local_addr().expect("local addr")
    }

    #[tokio::test]
    async fn rpc_against_a_stuck_peer_errors_out_instead_of_hanging_forever() {
        let stuck = stuck_listener().await;
        let reporter = test_reporter_with_addr(rset(8, 0, vec![]), format!("http://{stuck}"));

        let result = reporter
            .with_controller_timeout(Duration::from_millis(300), heartbeat_op)
            .await;

        assert!(result.is_err());
    }

    /// Proves `ReporterTimeouts.controller_rpc_timeout` actually reaches the public
    /// `with_controller` path, not just the test-only explicit-override seam above: a
    /// reporter built with a short configured timeout must give up well before the
    /// 10-second module default would.
    #[tokio::test]
    async fn a_configured_rpc_timeout_bounds_the_public_with_controller_call() {
        let stuck = stuck_listener().await;
        let mut reporter = test_reporter_with_addr(rset(8, 0, vec![]), format!("http://{stuck}"));
        reporter.timeouts.controller_rpc_timeout = Duration::from_millis(200);

        let start = Instant::now();
        let result = reporter
            .with_controller(move |mut client| async move {
                client
                    .heartbeat(spur_proto::proto::HeartbeatRequest::default())
                    .await
            })
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a stuck peer must not satisfy the RPC");
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail close to the configured 200ms, not the 10s module default; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_timed_out_rpc_makes_the_next_dial_prefer_the_other_host() {
        let stuck = stuck_listener().await;
        let healthy = responsive_non_controller_server().await;
        let reporter = test_reporter_with_addr(
            rset(8, 0, vec![]),
            format!("http://{stuck},http://{healthy}"),
        );

        let first = reporter
            .with_controller_timeout(Duration::from_millis(300), heartbeat_op)
            .await;
        assert!(first.is_err(), "the stuck peer must not answer");

        assert_eq!(
            reporter.dial_endpoints(),
            format!("http://{healthy},http://{stuck}"),
            "the endpoint that just timed out must be deprioritized"
        );

        let start = Instant::now();
        let second = reporter
            .with_controller_timeout(Duration::from_millis(300), heartbeat_op)
            .await;
        let err = second.expect_err("unimplemented on the stand-in server");
        assert_eq!(
            err_code(&err),
            tonic::Code::Unimplemented,
            "must have reached the healthy stand-in, not the stuck host"
        );
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "a dial that reaches the responsive host must not wait out the stuck one's timeout"
        );
    }

    /// A controller stand-in that answers every RPC with `code` and counts the calls,
    /// so a test can tell whether a heartbeat was retried and which host it reached.
    /// Every call counts, including the native-auth Ping `wrap` sends first once a
    /// test in this binary has configured it.
    async fn controller_answering(
        code: tonic::Code,
    ) -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        #[derive(Clone)]
        struct Answer {
            code: tonic::Code,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl tonic::server::NamedService for Answer {
            const NAME: &'static str = "slurm.SlurmController";
        }
        impl tonic::codegen::Service<http::Request<tonic::body::Body>> for Answer {
            type Response = http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(
                &mut self,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: http::Request<tonic::body::Body>) -> Self::Future {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(
                    tonic::Status::new(self.code, "stand-in answer").into_http()
                ))
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answer = Answer {
            code,
            calls: Arc::clone(&calls),
        };
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(answer)
                .serve_with_incoming(incoming)
                .await;
        });
        (addr, calls)
    }

    #[test]
    fn resource_exhausted_counts_as_a_transport_failure() {
        let status =
            |code| crate::controller_auth::ConnectAuthError::Status(tonic::Status::new(code, ""));
        assert!(is_transport_failure(&status(
            tonic::Code::ResourceExhausted
        )));
        for code in [
            tonic::Code::NotFound,
            tonic::Code::PermissionDenied,
            tonic::Code::Unauthenticated,
            tonic::Code::InvalidArgument,
        ] {
            assert!(
                !is_transport_failure(&status(code)),
                "{code:?} is the controller's answer, not a broken path"
            );
        }
    }

    #[tokio::test]
    async fn a_heartbeat_refused_with_resource_exhausted_fails_over_within_the_beat() {
        let (exhausted, exhausted_calls) =
            controller_answering(tonic::Code::ResourceExhausted).await;
        let (healthy, healthy_calls) = controller_answering(tonic::Code::Unimplemented).await;
        let reporter = test_reporter_with_addr(
            rset(8, 0, vec![]),
            format!("http://{exhausted},http://{healthy}"),
        );

        let err = reporter
            .send_heartbeat(spur_proto::proto::HeartbeatRequest::default())
            .await
            .expect_err("the healthy stand-in answers Unimplemented");

        assert_eq!(
            err_code(&err),
            tonic::Code::Unimplemented,
            "the retry must have reached the healthy controller"
        );
        assert_eq!(exhausted_calls.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            reporter.dial_endpoints(),
            format!("http://{healthy},http://{exhausted}"),
            "later beats must keep preferring the controller that answered"
        );
    }

    #[tokio::test]
    async fn a_heartbeat_to_a_single_controller_is_not_retried() {
        let (only, calls) = controller_answering(tonic::Code::ResourceExhausted).await;
        let reporter = test_reporter_with_addr(rset(8, 0, vec![]), format!("http://{only}"));

        let err = reporter
            .send_heartbeat(spur_proto::proto::HeartbeatRequest::default())
            .await
            .expect_err("the only controller refuses");

        assert_eq!(err_code(&err), tonic::Code::ResourceExhausted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_heartbeat_is_not_retried_with_the_failover_cooldown_off() {
        let (exhausted, exhausted_calls) =
            controller_answering(tonic::Code::ResourceExhausted).await;
        let (healthy, healthy_calls) = controller_answering(tonic::Code::Unimplemented).await;
        let mut reporter = test_reporter_with_addr(
            rset(8, 0, vec![]),
            format!("http://{exhausted},http://{healthy}"),
        );
        reporter.timeouts.failover_cooldown = Duration::ZERO;

        let err = reporter
            .send_heartbeat(spur_proto::proto::HeartbeatRequest::default())
            .await
            .expect_err("the first controller refuses");

        assert_eq!(err_code(&err), tonic::Code::ResourceExhausted);
        assert_eq!(exhausted_calls.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_heartbeat_the_controller_rejects_is_not_retried_elsewhere() {
        let (rejecting, rejecting_calls) = controller_answering(tonic::Code::NotFound).await;
        let (other, other_calls) = controller_answering(tonic::Code::Unimplemented).await;
        let reporter = test_reporter_with_addr(
            rset(8, 0, vec![]),
            format!("http://{rejecting},http://{other}"),
        );

        let err = reporter
            .send_heartbeat(spur_proto::proto::HeartbeatRequest::default())
            .await
            .expect_err("the controller does not know this node");

        assert_eq!(
            err_code(&err),
            tonic::Code::NotFound,
            "a NotFound must reach the re-register path unchanged"
        );
        assert_eq!(rejecting_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    /// A dead-fast entry can precede the wedged host; blame must still land on the
    /// one actually dialed, and the reorder must reach past it to the healthy third.
    #[tokio::test]
    async fn failover_reaches_a_healthy_host_behind_a_wedged_middle_entry() {
        let dead = refusing_addr().await;
        let stuck = stuck_listener().await;
        let healthy = responsive_non_controller_server().await;
        let reporter = test_reporter_with_addr(
            rset(8, 0, vec![]),
            format!("http://{dead},http://{stuck},http://{healthy}"),
        );

        let first = reporter
            .with_controller_timeout(Duration::from_millis(300), heartbeat_op)
            .await;
        assert!(first.is_err(), "the wedged middle host must not answer");

        assert_eq!(
            reporter.dial_endpoints(),
            format!("http://{dead},http://{healthy},http://{stuck}"),
            "blame must land on the host actually dialed (stuck), not the dead-fast one"
        );

        let start = Instant::now();
        let second = reporter
            .with_controller_timeout(Duration::from_millis(300), heartbeat_op)
            .await;
        let err = second.expect_err("unimplemented on the stand-in server");
        assert_eq!(
            err_code(&err),
            tonic::Code::Unimplemented,
            "must have reached the healthy stand-in, not the still-wedged host"
        );
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "the next dial must reach the healthy host instead of the still-wedged one"
        );
    }

    #[test]
    fn a_topology_only_change_is_classified_and_fingerprinted() {
        use spur_core::cpu_topology::{CpuRecord, CpuTopology};
        let with_online = |ids: &[u32]| {
            let records: Vec<CpuRecord> = ids
                .iter()
                .enumerate()
                .map(|(i, &cpu)| CpuRecord {
                    cpu,
                    socket: 0,
                    core_id: i as u32,
                    numa_node: None,
                })
                .collect();
            ResourceSet {
                cpus: ids.len() as u32,
                memory_mb: 1024,
                topology: Some(CpuTopology::from_cpus(&records)),
                ..Default::default()
            }
        };
        let old = with_online(&[0, 1, 2, 3]);
        let new = with_online(&[0, 1, 3, 4]);

        assert_eq!(
            classify(&old, &new, &Default::default()),
            InventoryDelta::FreeCapacityChanged
        );
        assert_ne!(old.schedulable_fingerprint(), new.schedulable_fingerprint());
    }

    #[test]
    fn classify_detects_free_growth_and_allocated_loss() {
        use std::collections::HashSet;
        let g = |d: u32, s: u64| GpuResource {
            device_id: d,
            gpu_type: "mi300x".into(),
            memory_mb: 0,
            peer_gpus: vec![],
            link_type: GpuLinkType::XGMI,
            topology: None,
            stable_id: s,
        };
        let rs = |gpus: Vec<GpuResource>| ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 8,
            memory_mb: 1024,
            gpus,
            generic: Default::default(),
            generation: 0,
            topology: None,
        };
        let two = rs(vec![g(0, 128), g(1, 129)]);
        let three = rs(vec![g(0, 128), g(1, 129), g(2, 130)]);
        let one = rs(vec![g(0, 128)]);

        // unchanged
        assert_eq!(
            classify(&two, &two, &HashSet::new()),
            InventoryDelta::Unchanged
        );
        // grew, nothing allocated -> free capacity change
        assert_eq!(
            classify(&two, &three, &HashSet::new()),
            InventoryDelta::FreeCapacityChanged
        );
        // 129 removed but not allocated -> free capacity change
        assert_eq!(
            classify(&two, &one, &HashSet::new()),
            InventoryDelta::FreeCapacityChanged
        );
        // 129 removed AND held -> allocated loss
        let held: HashSet<u64> = [129].into_iter().collect();
        assert_eq!(
            classify(&two, &one, &held),
            InventoryDelta::AllocatedDevicesLost
        );
    }

    fn gpu(device_id: u32, stable_id: u64) -> GpuResource {
        GpuResource {
            device_id,
            gpu_type: "mi300x".into(),
            memory_mb: 0,
            peer_gpus: vec![],
            link_type: GpuLinkType::XGMI,
            topology: None,
            stable_id,
        }
    }

    fn rset(cpus: u32, generation: u64, gpus: Vec<GpuResource>) -> ResourceSet {
        ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus,
            memory_mb: 1024,
            gpus,
            generic: Default::default(),
            generation,
            topology: None,
        }
    }

    /// stable_id encoded like `encode_stable_id`: BDF anchor (`bus` in the high
    /// bits) plus a low-8-bit partition rank. Two ids with the same `bus` share
    /// physical silicon.
    fn sid(bus: u64, partition: u64) -> u64 {
        (bus << 16) | (partition & spur_devices::cdi::STABLE_ID_PARTITION_MASK)
    }

    #[test]
    fn positional_gpu_ids_change_count_without_fencing_the_node() {
        use std::collections::HashSet;
        let positional = |n: u32| rset(8, 1, (0..n).map(|i| gpu(i, i as u64)).collect());
        let (old, fresh) = (positional(8), positional(7));
        let held: HashSet<u64> = [0].into_iter().collect();
        assert_eq!(
            classify(&old, &fresh, &held),
            InventoryDelta::FreeCapacityChanged
        );
        let dropped: HashSet<u64> = [7].into_iter().collect();
        assert_eq!(
            classify(&old, &fresh, &dropped),
            InventoryDelta::AllocatedDevicesLost
        );
    }

    #[test]
    fn reconcile_pins_missing_held_and_keeps_fresh_free() {
        use std::collections::HashSet;
        // Job holds a device on BDF 5. The fresh scan dropped it; free devices on
        // BDFs 7 and 9 remain/appear (different silicon, so they flow through).
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        let fresh = rset(8, 2, vec![gpu(0, sid(7, 0)), gpu(1, sid(9, 0))]);
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);

        // Held device pinned back AND the fresh free devices kept; total ⊇ allocated.
        let ids: Vec<u64> = reconciled.gpus.iter().map(|g| g.stable_id).collect();
        assert_eq!(reconciled.gpus.len(), 3);
        assert!(ids.contains(&sid(5, 0)), "held device must be pinned");
        assert!(ids.contains(&sid(7, 0)) && ids.contains(&sid(9, 0)));
        // generation is taken from fresh, not baseline.
        assert_eq!(reconciled.generation, 2);
    }

    #[test]
    fn reconcile_drops_fresh_partition_sharing_a_held_bdf() {
        use std::collections::HashSet;
        // The reviewer's scenario: the held GPU (BDF 5) is repartitioned into two
        // CPX slices sid(5,0) and sid(5,1); an unrelated free GPU sits on BDF 7.
        // The two BDF-5 slices overlap held silicon and MUST NOT be advertised as
        // free; only the baseline held device and the BDF-7 free device remain.
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        let fresh = rset(
            8,
            2,
            vec![gpu(0, sid(5, 0)), gpu(1, sid(5, 1)), gpu(2, sid(7, 0))],
        );
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);

        let ids: Vec<u64> = reconciled.gpus.iter().map(|g| g.stable_id).collect();
        assert!(ids.contains(&sid(5, 0)), "held device stays advertised");
        assert!(
            !ids.contains(&sid(5, 1)),
            "a fresh partition on the held BDF must be suppressed"
        );
        assert!(
            ids.contains(&sid(7, 0)),
            "an unrelated free GPU still flows through"
        );
        assert_eq!(reconciled.gpus.len(), 2);
        // No BDF-5 device other than the pinned held one is present.
        let bdf5 = ids.iter().filter(|&&id| id & 0xFF_0000 == 5 << 16).count();
        assert_eq!(bdf5, 1);
    }

    #[test]
    fn reconcile_does_not_over_suppress_free_on_a_different_bdf() {
        use std::collections::HashSet;
        // Held device on BDF 5 is unchanged; a FREE device on BDF 7 was replaced
        // by one on BDF 9. The held silicon is untouched, so the free-pool change
        // must flow through: 7 gone, 9 present, held 5 still advertised.
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        let fresh = rset(8, 2, vec![gpu(0, sid(5, 0)), gpu(1, sid(9, 0))]);
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        let ids: Vec<u64> = reconciled.gpus.iter().map(|g| g.stable_id).collect();
        assert_eq!(reconciled.gpus.len(), 2);
        assert!(ids.contains(&sid(5, 0)) && ids.contains(&sid(9, 0)));
        assert!(
            !ids.contains(&sid(7, 0)),
            "the freed BDF-7 device converged out"
        );
    }

    #[test]
    fn reconcile_does_not_double_insert_present_held() {
        use std::collections::HashSet;
        // The held id is still in fresh; it must appear exactly once (dropped by
        // the BDF filter, then re-inserted from baseline).
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        let fresh = rset(8, 2, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        assert_eq!(reconciled.gpus.len(), 2);
        let count_held = reconciled
            .gpus
            .iter()
            .filter(|g| g.stable_id == sid(5, 0))
            .count();
        assert_eq!(count_held, 1);
    }

    #[test]
    fn reconcile_of_a_held_repartition_stabilizes_after_unavailable_update() {
        use std::collections::HashSet;
        // Repeated scans must not re-register an already-unavailable held device.
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        // The held render node vanished, replaced by a CPX slice sid(5,1).
        let fresh = rset(8, 2, vec![gpu(0, sid(5, 1)), gpu(1, sid(7, 0))]);
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        assert_ne!(
            reconciled.schedulable_fingerprint(),
            baseline.schedulable_fingerprint()
        );
        let repeated = reconcile_free_pool(&fresh, &reconciled, &held);
        assert_eq!(
            repeated.schedulable_fingerprint(),
            reconciled.schedulable_fingerprint()
        );
    }

    /// One MI300X in CPX (bus 0x11, ranks 0..7) beside one SPX GPU (bus 0x12).
    fn cpx_node(generation: u64) -> ResourceSet {
        let mut gpus: Vec<GpuResource> = (0..8).map(|r| gpu(r, sid(0x11, r as u64))).collect();
        gpus.push(gpu(8, sid(0x12, 0)));
        rset(8, generation, gpus)
    }

    #[test]
    fn a_held_cpx_partition_on_unchanged_inventory_never_reaches_the_guard() {
        use std::collections::HashSet;
        let baseline = cpx_node(1);
        let fresh = cpx_node(2);
        let held: HashSet<u64> = [sid(0x11, 0)].into_iter().collect();

        let delta = classify(&baseline, &fresh, &held);
        let fp = fresh.schedulable_fingerprint();
        let (action, _) = next_refresh_action(&delta, &fp, &delta, &fp);

        assert_eq!(delta, InventoryDelta::Unchanged);
        assert_eq!(action, RefreshAction::Wait);
    }

    #[test]
    fn a_free_change_on_another_gpu_applies_capacity_with_a_held_cpx_partition() {
        use std::collections::HashSet;
        let baseline = cpx_node(1);
        let mut fresh = cpx_node(2);
        fresh.gpus.retain(|g| g.stable_id != sid(0x12, 0));
        let held: HashSet<u64> = [sid(0x11, 0)].into_iter().collect();

        let delta = classify(&baseline, &fresh, &held);
        let fp = fresh.schedulable_fingerprint();
        let (first, armed) = next_refresh_action(
            &delta,
            &fp,
            &InventoryDelta::Unchanged,
            &baseline.schedulable_fingerprint(),
        );
        let (second, _) = next_refresh_action(&delta, &fp, &armed, &fp);

        assert_eq!(delta, InventoryDelta::FreeCapacityChanged);
        assert_eq!(first, RefreshAction::Wait);
        assert_eq!(second, RefreshAction::ApplyCapacity);
    }

    #[test]
    fn a_changed_held_partition_id_is_renamed_and_keeps_the_new_siblings() {
        use std::collections::HashSet;
        // The upgrade case: the baseline carries the old encoding, where the
        // rank sat in the function bits, so rank 1 has its own BDF anchor.
        let old_rank1 = (0x11 << 16) | (1 << 8);
        let mut legacy: Vec<GpuResource> = (0..8)
            .map(|f| gpu(f as u32, (0x11 << 16) | (f << 8)))
            .collect();
        legacy.push(gpu(8, sid(0x12, 0)));
        let baseline = rset(8, 1, legacy);
        let fresh = cpx_node(2);
        let held: HashSet<u64> = [sid(0x11, 0), old_rank1].into_iter().collect();

        let plan = plan_refresh(&baseline, &fresh, &held);

        assert_eq!(plan.renamed[&old_rank1], sid(0x11, 1));
        assert_eq!(
            classify(&plan.baseline, &fresh, &plan.held),
            InventoryDelta::FreeCapacityChanged
        );
        assert_eq!(refresh_once(&baseline, &fresh, &held).gpus.len(), 9);
    }

    fn layout_node(partitions: u64) -> ResourceSet {
        let mut gpus: Vec<GpuResource> = (0..partitions)
            .map(|r| gpu(r as u32, sid(0x11, r)))
            .collect();
        gpus.push(gpu(partitions as u32, sid(0x12, 0)));
        rset(8, 1, gpus)
    }

    /// Mirrors the refresh loop: plan, classify, act; returns the reported set.
    fn refresh_once(
        old: &ResourceSet,
        fresh: &ResourceSet,
        held: &std::collections::HashSet<u64>,
    ) -> ResourceSet {
        plan_tick(old, fresh, held, &Fences::default()).reported()
    }

    fn overlaps(old_parts: u64, p: u64, new_parts: u64, q: u64) -> bool {
        let (a0, a1) = (p * 8 / old_parts, (p + 1) * 8 / old_parts);
        let (b0, b1) = (q * 8 / new_parts, (q + 1) * 8 / new_parts);
        a0 < b1 && b0 < a1
    }

    #[test]
    fn no_partition_change_offers_silicon_a_job_still_holds() {
        use std::collections::HashSet;
        for a in [1u64, 2, 4, 8] {
            for b in [1u64, 2, 4, 8] {
                if a == b {
                    continue;
                }
                for p in [0, a - 1] {
                    let old = layout_node(a);
                    let mut fresh = layout_node(b);
                    fresh.generation = 2;
                    let held: HashSet<u64> = [sid(0x11, p)].into();
                    let reported = refresh_once(&old, &fresh, &held);
                    let offered: Vec<u64> = reported
                        .gpus
                        .iter()
                        .map(|g| g.stable_id)
                        .filter(|id| !held.contains(id) && id >> 16 == 0x11)
                        .collect();
                    for id in offered {
                        assert!(
                            !overlaps(a, p, b, id & 0xFF),
                            "{a}->{b} held p{p}: offered overlapping {id:#x}"
                        );
                    }
                    assert!(
                        reported.gpus.iter().any(|g| g.stable_id == sid(0x11, p)),
                        "{a}->{b}: held device must stay reported"
                    );
                    assert!(reported.gpus.iter().any(|g| g.stable_id == sid(0x12, 0)));
                }
            }
        }
    }

    #[test]
    fn a_partition_change_with_nothing_held_adopts_the_fresh_ids() {
        use std::collections::HashSet;
        for (a, b) in [(1u64, 8u64), (8, 1), (2, 4), (4, 2), (8, 2), (1, 2)] {
            let old = layout_node(a);
            let fresh = layout_node(b);
            let held = HashSet::new();
            assert_eq!(
                classify(&old, &fresh, &held),
                InventoryDelta::FreeCapacityChanged
            );
            assert_eq!(refresh_once(&old, &fresh, &held).gpus, fresh.gpus);
        }
    }

    #[test]
    fn the_legacy_encoding_renames_held_partitions_and_keeps_the_free_ones() {
        use std::collections::HashSet;
        let old_rank1 = (0x11 << 16) | (1 << 8);
        let mut legacy: Vec<GpuResource> = (0..8)
            .map(|f| gpu(f as u32, (0x11 << 16) | (f << 8)))
            .collect();
        legacy.push(gpu(8, sid(0x12, 0)));
        let baseline = rset(8, 1, legacy);
        let fresh = cpx_node(2);
        let held: HashSet<u64> = [old_rank1].into();
        let plan = plan_refresh(&baseline, &fresh, &held);
        assert_eq!(plan.renamed[&old_rank1], sid(0x11, 1));
        assert_eq!(plan.held, HashSet::from([sid(0x11, 1)]));
        assert_eq!(
            classify(&plan.baseline, &fresh, &plan.held),
            InventoryDelta::FreeCapacityChanged
        );
    }

    #[test]
    fn a_re_encoding_converges_when_every_changed_id_is_held() {
        use std::collections::HashSet;
        // DPX with partition 1 held: partition 0's id is the same in both
        // encodings, so the only changed id is the held one.
        let old_rank1 = (0x11 << 16) | (1 << 8);
        let baseline = rset(8, 1, vec![gpu(0, sid(0x11, 0)), gpu(1, old_rank1)]);
        let fresh = rset(8, 2, vec![gpu(0, sid(0x11, 0)), gpu(1, sid(0x11, 1))]);
        let held: HashSet<u64> = [old_rank1].into();
        let plan = plan_refresh(&baseline, &fresh, &held);
        assert_eq!(plan.held, HashSet::from([sid(0x11, 1)]));
        assert_eq!(
            refresh_delta(&baseline, &fresh, &plan),
            InventoryDelta::FreeCapacityChanged
        );
        assert_eq!(refresh_once(&baseline, &fresh, &held).gpus, fresh.gpus);
    }

    #[test]
    fn a_legacy_held_partition_on_a_repartitioned_gpu_keeps_its_id_and_hides_the_gpu() {
        use std::collections::HashSet;
        let legacy_rank3 = (0x11 << 16) | (3 << 8);
        let mut legacy: Vec<GpuResource> = (0..8)
            .map(|f| gpu(f as u32, (0x11 << 16) | (f << 8)))
            .collect();
        legacy.push(gpu(8, sid(0x12, 0)));
        let baseline = rset(8, 1, legacy);
        let mut fresh = layout_node(4);
        fresh.generation = 2;
        let held: HashSet<u64> = [legacy_rank3].into();

        let plan = plan_refresh(&baseline, &fresh, &held);
        let reported = refresh_once(&baseline, &fresh, &held);

        assert!(plan.renamed.is_empty());
        assert_eq!(plan.held, held);
        let ids: Vec<u64> = reported.gpus.iter().map(|g| g.stable_id).collect();
        assert!(ids.contains(&legacy_rank3), "held device stays reported");
        assert!(
            !ids.iter().any(|id| id >> 16 == 0x11 && *id != legacy_rank3),
            "no fresh partition of the held GPU is offered: {ids:#x?}"
        );
        assert!(ids.contains(&sid(0x12, 0)));
    }

    #[test]
    fn a_job_on_unchanged_partition_zero_hides_no_sibling() {
        use std::collections::HashSet;
        let baseline = cpx_node(1);
        let fresh = cpx_node(2);
        let held: HashSet<u64> = [sid(0x11, 0)].into();
        assert_eq!(refresh_once(&baseline, &fresh, &held).gpus.len(), 9);
    }

    /// The refresh loop with every register acked: what the controller has,
    /// the fences committed with it, and the node's ledger.
    struct Agent {
        reported: ResourceSet,
        fences: Fences,
        ledger: spur_sched::cons_tres::NodeAllocation,
    }

    impl Agent {
        fn new(inventory: ResourceSet, holders: &[(u32, u64)]) -> Self {
            let mut ledger = spur_sched::cons_tres::NodeAllocation::new("n".into(), &inventory);
            for &(job, id) in holders {
                ledger.allocate_for_job(job, 1, 0, 0, &[id]).unwrap();
                assert!(ledger.commit_job(job, 1));
            }
            Self {
                reported: inventory,
                fences: Fences::default(),
                ledger,
            }
        }

        fn ledger_ids(&self) -> Vec<u64> {
            self.ledger.gpus.iter().map(|g| g.stable_id).collect()
        }

        fn held(&self) -> std::collections::HashSet<u64> {
            self.ledger.allocated_gpu_ids().into_iter().collect()
        }

        fn tick(&mut self, fresh: &ResourceSet) {
            self.tick_racing(fresh, |_| {});
        }

        /// A tick the debounce holds back: only the ledger's gate moves.
        fn observe(&mut self, fresh: &ResourceSet) {
            let tick = plan_tick(&self.reported, fresh, &self.held(), &self.fences);
            self.ledger
                .set_fenced(tick.ledger_fenced(&self.fences, &self.ledger_ids()));
        }

        /// One tick, with `race` run on the ledger between planning and applying.
        fn tick_racing(
            &mut self,
            fresh: &ResourceSet,
            race: impl FnOnce(&mut spur_sched::cons_tres::NodeAllocation),
        ) {
            let tick = plan_tick(&self.reported, fresh, &self.held(), &self.fences);
            self.ledger
                .set_fenced(tick.ledger_fenced(&self.fences, &self.ledger_ids()));
            if tick.delta == InventoryDelta::Unchanged {
                self.fences = tick.fences;
                return;
            }
            race(&mut self.ledger);
            let reported = tick.reported();
            let change = apply_to_ledger(&mut self.ledger, &tick, &reported, &[]);
            if change != spur_sched::cons_tres::CapacityChange::Applied {
                return;
            }
            self.reported = reported;
            self.fences = tick.fences;
        }

        /// Free ids on GPU `bus` a dispatch could be handed.
        fn offered(&self, bus: u64) -> Vec<u64> {
            let held = self.held();
            self.reported
                .gpus
                .iter()
                .map(|g| g.stable_id)
                .filter(|id| id >> 16 == bus && !held.contains(id))
                .collect()
        }

        fn on_gpu(&self, bus: u64) -> Vec<u64> {
            self.reported
                .gpus
                .iter()
                .map(|g| g.stable_id)
                .filter(|id| id >> 16 == bus)
                .collect()
        }
    }

    #[test]
    fn a_repartitioned_gpu_stays_fenced_until_its_last_old_holder_exits() {
        let dpx = layout_node(2);
        let mut agent = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        for _ in 0..4 {
            agent.tick(&dpx);
            assert_eq!(agent.on_gpu(0x11), vec![sid(0x11, 0), sid(0x11, 1)]);
        }

        // DPX rank 0 overlaps the old CPX rank 1 job 2 still runs on.
        assert!(agent.ledger.release_job(1));
        for _ in 0..4 {
            agent.tick(&dpx);
            assert_eq!(agent.on_gpu(0x11), vec![sid(0x11, 1)]);
            assert!(agent.offered(0x11).is_empty());
        }
        assert!(agent
            .reported
            .gpus
            .iter()
            .any(|g| g.stable_id == sid(0x12, 0)));

        assert!(agent.ledger.release_job(2));
        agent.tick(&dpx);
        assert_eq!(agent.reported.gpus, dpx.gpus);
        assert_eq!(agent.fences, Fences::default());
    }

    #[test]
    fn a_tick_pins_the_held_ids_whose_silicon_changed_and_not_intact_ones() {
        let held: HashSet<u64> = [sid(0x11, 0), sid(0x11, 1), sid(0x12, 0)].into();
        let tick = plan_tick(&layout_node(8), &layout_node(2), &held, &Fences::default());
        assert_eq!(tick.delta, InventoryDelta::AllocatedDevicesLost);
        assert_eq!(
            tick.pinned_ids(),
            HashSet::from([sid(0x11, 0), sid(0x11, 1)])
        );
        let none_held = plan_tick(
            &layout_node(8),
            &layout_node(2),
            &HashSet::new(),
            &Fences::default(),
        );
        assert!(none_held.pinned_ids().is_empty());
    }

    /// The repartitioned GPU sorts last, so a fence that keeps every old
    /// partition reports exactly what was reported before.
    fn split_last_gpu(partitions: u64) -> ResourceSet {
        let mut gpus = vec![gpu(0, sid(0x11, 0))];
        gpus.extend((0..partitions).map(|r| gpu(r as u32 + 1, sid(0x12, r))));
        rset(8, 1, gpus)
    }

    #[test]
    fn a_fence_that_changes_nothing_reported_still_guards_a_released_id() {
        let cpx = split_last_gpu(8);
        let mut agent = Agent::new(split_last_gpu(2), &[(1, sid(0x12, 0)), (2, sid(0x12, 1))]);
        let tick = plan_tick(&agent.reported, &cpx, &agent.held(), &agent.fences);
        assert_eq!(tick.delta, InventoryDelta::Unchanged);
        agent.tick(&cpx);
        assert!(agent.ledger.release_job(1));

        assert_eq!(
            agent.ledger.allocate_for_job(3, 1, 0, 0, &[sid(0x12, 0)]),
            Err(spur_sched::cons_tres::AllocError::GpusUnavailable)
        );
        agent.tick(&cpx);
        agent.tick(&cpx);
        assert_eq!(agent.on_gpu(0x12), vec![sid(0x12, 1)]);
    }

    #[test]
    fn a_repartition_seen_once_gates_every_old_id_of_the_gpu() {
        use spur_sched::cons_tres::AllocError;
        let dpx = layout_node(2);
        let mut agent = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        agent.observe(&dpx);
        assert!(agent.ledger.release_job(1));
        agent.observe(&dpx);

        for id in [sid(0x11, 0), sid(0x11, 5)] {
            assert_eq!(
                agent.ledger.allocate_for_job(3, 1, 0, 0, &[id]),
                Err(AllocError::GpusUnavailable),
                "{id:#x}"
            );
        }
        assert!(agent
            .ledger
            .allocate_for_job(4, 1, 0, 0, &[sid(0x12, 0)])
            .is_ok());
    }

    #[test]
    fn positional_gpus_are_never_gated() {
        let positional = |n: u32| rset(8, 1, (0..n).map(|i| gpu(i, i as u64)).collect());
        let mut agent = Agent::new(positional(8), &[(1, 0)]);
        agent.observe(&positional(7));
        assert!(agent.ledger.allocate_for_job(2, 1, 0, 0, &[3]).is_ok());
    }

    #[test]
    fn a_repartition_nothing_holds_is_not_gated() {
        let dpx = layout_node(2);
        let mut agent = Agent::new(layout_node(8), &[]);
        agent.observe(&dpx);
        assert!(agent
            .ledger
            .allocate_for_job(3, 1, 0, 0, &[sid(0x11, 5)])
            .is_ok());
        assert!(agent.ledger.release_job(3));
        agent.tick(&dpx);
        assert_eq!(agent.reported.gpus, dpx.gpus);
    }

    #[test]
    fn the_tick_that_lifts_a_fence_offers_the_new_layout_at_once() {
        use spur_sched::cons_tres::AllocError;
        let dpx = layout_node(2);
        let mut lifted = Agent::new(layout_node(8), &[(1, sid(0x11, 0))]);
        lifted.tick(&dpx);
        assert!(lifted.ledger.release_job(1));
        lifted.tick(&dpx);
        assert_eq!(lifted.reported.gpus, dpx.gpus);
        // DPX rank 0 reuses the id the fence pinned.
        for id in [sid(0x11, 0), sid(0x11, 1)] {
            assert!(
                lifted.ledger.allocate_for_job(3, 1, 0, 0, &[id]).is_ok(),
                "{id:#x}"
            );
            assert!(lifted.ledger.release_job(3));
        }

        let mut held = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        held.tick(&dpx);
        assert!(held.ledger.release_job(1));
        held.tick(&dpx);
        assert!(held
            .ledger
            .allocate_for_job(3, 1, 0, 0, &[sid(0x11, 0)])
            .is_err());
        assert_eq!(
            held.ledger.allocate_for_job(3, 1, 0, 0, &[sid(0x11, 1)]),
            Err(AllocError::GpusUnavailable)
        );
    }

    #[test]
    fn a_deferred_refresh_keeps_a_released_id_fenced() {
        let mut agent = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        agent.tick(&layout_node(2));
        assert!(agent.ledger.release_job(1));

        // GPU 0x12 drops out while a launch takes it, so the apply defers.
        let mut without_0x12 = layout_node(2);
        without_0x12.gpus.retain(|g| g.stable_id >> 16 != 0x12);
        agent.tick_racing(&without_0x12, |ledger| {
            ledger
                .allocate_for_job(3, 1, 0, 0, &[sid(0x12, 0)])
                .unwrap();
        });

        assert_eq!(
            agent.ledger.allocate_for_job(4, 1, 0, 0, &[sid(0x11, 0)]),
            Err(spur_sched::cons_tres::AllocError::GpusUnavailable)
        );
    }

    #[test]
    fn a_released_fenced_id_cannot_be_dispatched_before_the_next_refresh() {
        let dpx = layout_node(2);
        let mut agent = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        agent.tick(&dpx);
        assert!(agent.ledger.release_job(1));

        assert_eq!(
            agent.ledger.allocate_for_job(3, 1, 0, 0, &[sid(0x11, 0)]),
            Err(spur_sched::cons_tres::AllocError::GpusUnavailable)
        );
        agent.tick(&dpx);
        assert_eq!(agent.on_gpu(0x11), vec![sid(0x11, 1)]);
    }

    #[test]
    fn no_repartition_offers_silicon_an_old_holder_still_uses_after_a_sibling_exits() {
        for a in [1u64, 2, 4, 8] {
            for b in [1u64, 2, 4, 8] {
                if a == b {
                    continue;
                }
                // Old ranks matching the new layout's hide the repartition.
                let patterns: [Vec<u64>; 2] = [(0..a.min(b)).collect(), vec![0, a - 1]];
                for ranks in patterns {
                    let ranks: std::collections::BTreeSet<u64> = ranks.into_iter().collect();
                    let holders: Vec<(u32, u64)> = ranks
                        .into_iter()
                        .enumerate()
                        .map(|(job, p)| (job as u32 + 1, sid(0x11, p)))
                        .collect();
                    let mut fresh = layout_node(b);
                    fresh.generation = 2;
                    let mut agent = Agent::new(layout_node(a), &holders);
                    for (exited, &(job, released)) in holders.iter().enumerate() {
                        for _ in 0..3 {
                            agent.tick(&fresh);
                            for &(_, id) in &holders[exited..] {
                                for offered in agent.offered(0x11) {
                                    assert!(
                                        !overlaps(a, id & 0xFF, b, offered & 0xFF),
                                        "{a}->{b}: offered {offered:#x} over held {id:#x}"
                                    );
                                }
                                assert!(agent.on_gpu(0x11).contains(&id), "{a}->{b}");
                            }
                        }
                        assert!(agent.ledger.release_job(job));
                        assert_eq!(
                            agent.ledger.allocate_for_job(99, 1, 0, 0, &[released]),
                            Err(spur_sched::cons_tres::AllocError::GpusUnavailable),
                            "{a}->{b}: released {released:#x} offered before the refresh"
                        );
                    }
                    agent.tick(&fresh);
                    assert_eq!(agent.reported.gpus, fresh.gpus, "{a}->{b}");
                }
            }
        }
    }

    #[test]
    fn fences_carry_a_repartition_across_a_restart() {
        let dpx = layout_node(2);
        let mut agent = Agent::new(layout_node(8), &[(1, sid(0x11, 0)), (2, sid(0x11, 1))]);
        agent.tick(&dpx);
        assert!(agent.ledger.release_job(1));

        // The saved baseline already lists only the pinned ids, which match
        // DPX's ranks: only the saved fences still know the GPU changed.
        let held = agent.held();
        let out = startup_inventory(&dpx, Some(&agent.reported), &held, &agent.fences);
        let on_gpu: Vec<u64> = out
            .reported
            .gpus
            .iter()
            .map(|g| g.stable_id)
            .filter(|id| id >> 16 == 0x11)
            .collect();
        assert_eq!(on_gpu, vec![sid(0x11, 1)]);
        assert_eq!(out.fences.pinned_ids(), held);
    }

    #[test]
    fn a_retry_after_a_failed_register_keeps_every_held_id() {
        use spur_sched::cons_tres::{CapacityChange, NodeAllocation};
        use std::collections::HashSet;
        // 0x11 is re-encoded in place; 0x12 is repartitioned under a job.
        let old_rank1 = (0x11 << 16) | (1 << 8);
        let old = rset(
            8,
            1,
            vec![
                gpu(0, sid(0x11, 0)),
                gpu(1, old_rank1),
                gpu(2, sid(0x12, 0)),
            ],
        );
        let fresh = rset(
            8,
            2,
            vec![
                gpu(0, sid(0x11, 0)),
                gpu(1, sid(0x11, 1)),
                gpu(2, sid(0x12, 0)),
                gpu(3, sid(0x12, 1)),
            ],
        );
        let mut ledger = NodeAllocation::new("n".into(), &old);
        for (job, id) in [(1, old_rank1), (2, sid(0x12, 0))] {
            ledger.allocate_for_job(job, 1, 1, 0, &[id]).unwrap();
            assert!(ledger.commit_job(job, 1));
        }
        let held = |l: &NodeAllocation| l.allocated_gpu_ids().into_iter().collect::<HashSet<_>>();

        // The first attempt renames the ledger; its register fails, so `old` stays.
        let before = held(&ledger);
        ledger.rename_gpus(&plan_refresh(&old, &fresh, &before).renamed);
        let reported = refresh_once(&old, &fresh, &before);
        assert_eq!(ledger.update_capacity(&reported), CapacityChange::Applied);

        let retried = refresh_once(&old, &fresh, &held(&ledger));
        let ids: HashSet<u64> = retried.gpus.iter().map(|g| g.stable_id).collect();
        assert!(ids.contains(&sid(0x11, 1)) && ids.contains(&sid(0x12, 0)));
        assert_eq!(ledger.update_capacity(&retried), CapacityChange::Applied);
    }

    #[test]
    fn after_release_next_classify_applies_current_baseline() {
        use spur_sched::cons_tres::{CapacityChange, NodeAllocation};
        use std::collections::HashSet;
        // Real release_job + classify/update_capacity path (no simulation).
        // A job holds the GPU on BDF 5; the node repartitions it into two CPX
        // slices sid(5,0),sid(5,1) while an unrelated free GPU sits on BDF 7.
        // While held the vanished-held classify fires; after release the held id
        // is gone, so the current hardware is a plain free-pool change.
        let held_id = sid(5, 9);
        let baseline = rset(8, 1, vec![gpu(0, held_id), gpu(1, sid(7, 0))]);
        let real_hw = rset(
            8,
            2,
            vec![gpu(0, sid(5, 0)), gpu(1, sid(5, 1)), gpu(2, sid(7, 0))],
        );

        let mut node = NodeAllocation::new("n".into(), &baseline);
        node.allocate_for_job(7, 0, 0, 0, &[held_id]).unwrap();

        // While held: held_id is allocated and absent from real_hw -> held loss.
        let held: HashSet<u64> = node.allocated_gpu_ids().into_iter().collect();
        assert_eq!(
            classify(&baseline, &real_hw, &held),
            InventoryDelta::AllocatedDevicesLost
        );

        // Release the job; the held id leaves the allocated set.
        assert!(node.release_job(7));
        let held_after: HashSet<u64> = node.allocated_gpu_ids().into_iter().collect();
        assert!(held_after.is_empty());

        // Now the real hardware is just a free-pool change and applies cleanly.
        assert_eq!(
            classify(&baseline, &real_hw, &held_after),
            InventoryDelta::FreeCapacityChanged
        );
        assert!(matches!(
            node.update_capacity(&real_hw),
            CapacityChange::Applied
        ));
        assert_eq!(node.gpus.len(), 3);
        assert_eq!(node.free_gpus(None), 3);
    }

    #[test]
    fn fabric_capable_initial_inventory_has_a_nonzero_boot_generation() {
        let reporter = test_reporter(ResourceSet {
            fabric_binding_version: 1,
            block_file_revision: None,
            ..Default::default()
        });
        let generation = reporter.snapshot_resources().generation;
        assert!(generation > 0 && generation < (1u64 << 63));
        let explicit = test_reporter(ResourceSet {
            fabric_binding_version: 1,
            block_file_revision: None,
            generation: 7,
            ..Default::default()
        });
        assert_eq!(explicit.snapshot_resources().generation, 7);
        assert_eq!(
            test_reporter(ResourceSet::default())
                .snapshot_resources()
                .generation,
            0
        );
    }

    #[tokio::test]
    async fn inventory_publication_waits_for_allocation_reservation() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let resources = ResourceSet {
            generation: 7,
            ..Default::default()
        };
        let reporter = test_reporter(resources.clone());
        let allocation = Mutex::new(NodeAllocation::new("test-node".into(), &resources));
        let guard = allocation.lock().await;
        let fresh = ResourceSet {
            generation: 8,
            ..resources
        };
        let fences = Fences::default();
        let mut publish = Box::pin(reporter.publish_resources(&allocation, fresh, &fences));
        assert!(matches!(
            publish
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(reporter.resource_generation(), 7);
        drop(guard);
        publish.await;
        assert_eq!(reporter.resource_generation(), 8);
    }

    #[test]
    fn update_resources_reports_change_ignoring_generation() {
        let base = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 4,
            memory_mb: 512,
            gpus: vec![],
            generic: Default::default(),
            generation: 1,
            topology: None,
        };
        let reporter = test_reporter(base.clone());
        // same content, different generation -> not a change
        let mut same = base.clone();
        same.generation = 2;
        assert!(!reporter.update_resources(same));
        // different cpu count -> change
        let mut diff = base.clone();
        diff.cpus = 8;
        assert!(reporter.update_resources(diff));
        assert_eq!(reporter.snapshot_resources().cpus, 8);
    }

    fn fp(cpus: u32, gpu_ids: &[u64]) -> InventoryFingerprint {
        (
            cpus,
            1024,
            gpu_ids.iter().map(|&id| gpu(id as u32, id)).collect(),
            Vec::new(),
            0,
            None,
            None,
        )
    }

    #[test]
    fn unchanged_tick_never_acts_and_resets_last_delta() {
        let (action, last) = next_refresh_action(
            &InventoryDelta::Unchanged,
            &fp(8, &[0, 1]),
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[0, 1]),
        );
        assert_eq!(action, RefreshAction::Wait);
        assert_eq!(last, InventoryDelta::Unchanged);
    }

    #[test]
    fn a_change_seen_once_only_arms_then_acts_on_the_second() {
        // First sighting: arm, do not act.
        let (action, last) = next_refresh_action(
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[0, 1]),
            &InventoryDelta::Unchanged,
            &fp(0, &[]),
        );
        assert_eq!(action, RefreshAction::Wait);
        assert_eq!(last, InventoryDelta::FreeCapacityChanged);
        // Same delta AND same inventory again: act, and reset the carry.
        let (action, last) = next_refresh_action(
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[0, 1]),
            &last,
            &fp(8, &[0, 1]),
        );
        assert_eq!(action, RefreshAction::ApplyCapacity);
        assert_eq!(last, InventoryDelta::Unchanged);
    }

    #[test]
    fn allocated_loss_seen_twice_selects_report_lost() {
        let (action, _) = next_refresh_action(
            &InventoryDelta::AllocatedDevicesLost,
            &fp(8, &[0]),
            &InventoryDelta::AllocatedDevicesLost,
            &fp(8, &[0]),
        );
        assert_eq!(action, RefreshAction::ReportLost);
    }

    #[test]
    fn a_different_change_on_the_second_tick_re_arms_instead_of_acting() {
        // Free-capacity armed last tick, but this tick reads a held-device loss:
        // the two disagree, so re-arm on the new delta rather than act on either.
        let (action, last) = next_refresh_action(
            &InventoryDelta::AllocatedDevicesLost,
            &fp(8, &[0]),
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[0]),
        );
        assert_eq!(action, RefreshAction::Wait);
        assert_eq!(last, InventoryDelta::AllocatedDevicesLost);
    }

    #[test]
    fn same_delta_but_different_inventory_does_not_act() {
        // Two ticks both classify FreeCapacityChanged, but the observed GPU sets
        // differ (one topology was never seen twice) -> arm, do not act.
        let (action, last) = next_refresh_action(
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[128, 129]),
            &InventoryDelta::FreeCapacityChanged,
            &fp(8, &[130, 131]),
        );
        assert_eq!(action, RefreshAction::Wait);
        assert_eq!(last, InventoryDelta::FreeCapacityChanged);
    }

    #[test]
    fn a_failed_register_keeps_the_baseline_and_re_arms_for_retry() {
        // The invariant that fixes convergence-loss: on register failure the
        // baseline must NOT advance, and the acted delta is re-armed so the very
        // next tick retries instead of re-serving the seen-twice debounce.
        let (commit, next) = post_converge(&InventoryDelta::AllocatedDevicesLost, false);
        assert_eq!(commit, BaselineCommit::Keep);
        assert_eq!(next, InventoryDelta::AllocatedDevicesLost);
    }

    #[test]
    fn a_successful_register_commits_the_baseline_and_resets() {
        let (commit, next) = post_converge(&InventoryDelta::FreeCapacityChanged, true);
        assert_eq!(commit, BaselineCommit::Commit);
        assert_eq!(next, InventoryDelta::Unchanged);
    }

    #[test]
    fn refresh_secs_falls_back_on_unset_zero_and_garbage() {
        assert_eq!(parse_refresh_secs(None), DEFAULT_INVENTORY_REFRESH_SECS);
        assert_eq!(
            parse_refresh_secs(Some("0".into())),
            DEFAULT_INVENTORY_REFRESH_SECS
        );
        assert_eq!(
            parse_refresh_secs(Some("nope".into())),
            DEFAULT_INVENTORY_REFRESH_SECS
        );
        assert_eq!(parse_refresh_secs(Some("5".into())), 5);
    }

    #[test]
    fn startup_without_a_baseline_translates_legacy_cpx_ids() {
        use std::collections::HashSet;
        let fresh = cpx_node(1);
        let held: HashSet<u64> = [(0x11 << 16) | (3 << 8)].into();
        let out = startup_inventory(&fresh, None, &held, &Fences::default());
        assert_eq!(out.renamed[&((0x11 << 16) | (3 << 8))], sid(0x11, 3));
        assert_eq!(out.reported.gpus, fresh.gpus);
    }

    #[test]
    fn startup_with_a_baseline_fences_a_gpu_repartitioned_while_stopped() {
        use std::collections::HashSet;
        for (a, b) in [(1u64, 8u64), (8, 1), (2, 4), (4, 8), (8, 2)] {
            let baseline = layout_node(a);
            let fresh = layout_node(b);
            let held: HashSet<u64> = [sid(0x11, a - 1)].into();
            let out = startup_inventory(&fresh, Some(&baseline), &held, &Fences::default());
            let on_gpu: Vec<u64> = out
                .reported
                .gpus
                .iter()
                .map(|g| g.stable_id)
                .filter(|id| id >> 16 == 0x11)
                .collect();
            assert_eq!(on_gpu, vec![sid(0x11, a - 1)], "{a}->{b}");
        }
    }

    #[test]
    fn startup_without_a_baseline_pins_a_held_id_whose_gpu_is_still_there() {
        use std::collections::HashSet;
        let fresh = layout_node(1); // was CPX, now SPX
        let held: HashSet<u64> = [sid(0x11, 7)].into();
        let out = startup_inventory(&fresh, None, &held, &Fences::default());
        let on_gpu: Vec<u64> = out
            .reported
            .gpus
            .iter()
            .map(|g| g.stable_id)
            .filter(|id| id >> 16 == 0x11)
            .collect();
        assert_eq!(on_gpu, vec![sid(0x11, 7)]);
    }

    #[test]
    fn startup_without_a_baseline_keeps_the_siblings_of_an_intact_held_gpu() {
        use std::collections::HashSet;
        let mut fresh = layout_node(8);
        fresh.gpus.retain(|g| g.stable_id != sid(0x12, 0));
        fresh.gpus.push(gpu(9, sid(0x13, 0)));
        let held: HashSet<u64> = [sid(0x11, 2), sid(0x12, 0)].into();
        let out = startup_inventory(&fresh, None, &held, &Fences::default());
        let ids: Vec<u64> = out.reported.gpus.iter().map(|g| g.stable_id).collect();
        assert_eq!(
            ids,
            fresh.gpus.iter().map(|g| g.stable_id).collect::<Vec<_>>()
        );
        assert!(out.renamed.is_empty());
    }

    #[test]
    fn startup_without_a_baseline_keeps_a_persisted_fence() {
        use std::collections::HashSet;
        let fresh = layout_node(8);
        let held_id = sid(0x11, 0);
        let mut fences = Fences::default();
        fences.pin(bdf_anchor(held_id), &gpu(0, held_id));
        let held: HashSet<u64> = [held_id].into();
        let out = startup_inventory(&fresh, None, &held, &fences);
        let on_gpu: Vec<u64> = out
            .reported
            .gpus
            .iter()
            .map(|g| g.stable_id)
            .filter(|id| id >> 16 == 0x11)
            .collect();
        assert_eq!(on_gpu, vec![held_id]);
        assert_eq!(out.fences, fences);
    }

    #[test]
    fn startup_without_a_baseline_drops_a_fence_with_no_live_holder() {
        let fresh = layout_node(8);
        let mut fences = Fences::default();
        fences.pin(bdf_anchor(sid(0x11, 0)), &gpu(0, sid(0x11, 0)));
        let out = startup_inventory(&fresh, None, &Default::default(), &fences);
        assert_eq!(out.reported.gpus, fresh.gpus);
        assert_eq!(out.fences, Fences::default());
    }

    #[test]
    fn startup_with_nothing_held_reports_the_fresh_inventory() {
        let fresh = layout_node(8);
        let out = startup_inventory(
            &fresh,
            Some(&layout_node(1)),
            &Default::default(),
            &Fences::default(),
        );
        assert_eq!(out.reported.gpus, fresh.gpus);
        assert!(out.renamed.is_empty());
    }

    #[test]
    fn the_baseline_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_baseline(dir.path()).is_none());
        save_baseline(dir.path(), &cpx_node(3));
        assert_eq!(load_baseline(dir.path()), Some(cpx_node(3)));
    }

    #[tokio::test]
    async fn publishing_resources_saves_them_as_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let reporter = test_reporter(cpx_node(1)).with_baseline_dir(dir.path().to_path_buf());
        let allocation = Mutex::new(NodeAllocation::new("n".into(), &cpx_node(1)));
        let fences = Fences {
            anchors: [(sid(0x11, 0), vec![gpu(1, sid(0x11, 1))])].into(),
        };
        reporter
            .publish_resources(&allocation, layout_node(2), &fences)
            .await;
        assert_eq!(load_baseline(dir.path()), Some(layout_node(2)));
        assert_eq!(load_fences(dir.path()), fences);
        assert_eq!(reporter.fences(), fences);
    }

    #[tokio::test]
    async fn a_baseline_is_not_saved_without_its_fences() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FENCES_FILE)).unwrap();
        let reporter = test_reporter(cpx_node(1)).with_baseline_dir(dir.path().to_path_buf());
        let allocation = Mutex::new(NodeAllocation::new("n".into(), &cpx_node(1)));
        reporter
            .publish_resources(&allocation, layout_node(2), &Fences::default())
            .await;
        assert!(load_baseline(dir.path()).is_none());
    }

    #[test]
    fn a_fence_is_reported_once_when_it_outlives_the_limit() {
        let hour = Duration::from_secs(3600);
        let fenced = Fences {
            anchors: [(sid(0x11, 0), vec![gpu(1, sid(0x11, 1))])].into(),
        };
        let start = Instant::now();
        let mut ages = FenceAges::default();
        assert!(ages.overdue(&fenced, start, hour).is_empty());
        assert_eq!(
            ages.overdue(&fenced, start + 2 * hour, hour),
            vec![sid(0x11, 0)]
        );
        assert!(ages.overdue(&fenced, start + 3 * hour, hour).is_empty());

        assert!(ages
            .overdue(&Fences::default(), start + 4 * hour, hour)
            .is_empty());
        assert!(ages.overdue(&fenced, start + 4 * hour, hour).is_empty());
        assert!(ages
            .overdue(&fenced, start + 4 * hour + hour / 2, hour)
            .is_empty());
    }

    #[test]
    fn committing_fences_alone_saves_and_holds_them() {
        let dir = tempfile::tempdir().unwrap();
        let reporter = test_reporter(cpx_node(1)).with_baseline_dir(dir.path().to_path_buf());
        let fences = Fences {
            anchors: [(sid(0x11, 0), vec![gpu(0, sid(0x11, 0))])].into(),
        };
        assert!(reporter.commit_fences(&fences));
        assert_eq!(load_fences(dir.path()), fences);
        assert_eq!(reporter.fences(), fences);
        assert!(load_baseline(dir.path()).is_none());
    }

    #[test]
    fn fences_round_trip_through_their_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_fences(dir.path()), Fences::default());
        let fences = Fences {
            anchors: [(
                sid(0x11, 0),
                vec![gpu(0, sid(0x11, 0)), gpu(1, sid(0x11, 1))],
            )]
            .into(),
        };
        save_fences(dir.path(), &fences);
        assert_eq!(load_fences(dir.path()), fences);
        std::fs::write(dir.path().join(FENCES_FILE), b"{").unwrap();
        assert_eq!(load_fences(dir.path()), Fences::default());
    }
}
