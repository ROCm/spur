// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Context;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
use spur_devices::{resolve_link_type, DeviceRegistry, LinkType};
use spur_proto::proto::{
    RegisterAgentRequest, ResourceSet as ProtoResourceSet, RunningJobStatus, StepdRecoveryRequest,
    StepdRecoveryResponse,
};
use spur_sched::cons_tres::NodeAllocation;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// Source of the job ids this node currently holds. The controller decides from
/// its own authoritative state whether any reported id is stale.
pub trait HeldJobs: Send + Sync {
    fn held_job_ids(&self) -> Vec<u32>;
}

impl<T: Send> HeldJobs for Mutex<HashMap<u32, T>> {
    fn held_job_ids(&self) -> Vec<u32> {
        match self.try_lock() {
            Ok(jobs) => jobs.keys().copied().collect(),
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
    /// Job ids this node holds, reported each heartbeat so the controller can
    /// reclaim allocations it no longer tracks. Shares the agent's running map.
    held_jobs: Arc<dyn HeldJobs>,
    /// The live per-node allocation, read each heartbeat for the translated GPU
    /// stable_ids each held job occupies so the controller can converge its
    /// used-view. Authoritative post-translation source (an adopted legacy job's
    /// `owners` entry holds the current stable_ids, unlike its running-map
    /// descriptor). Wired once, after the AgentService is built.
    allocation: std::sync::OnceLock<Arc<Mutex<NodeAllocation>>>,
    /// k0s node status the heartbeat carries; wired once after the K0sAgent is built.
    k0s_status: std::sync::OnceLock<Arc<crate::cluster::K0sNodeState>>,
    /// The entitlement ledger, wired once the state root is known. Without one
    /// the agent registers with no ledger, which asserts nothing.
    admissions: std::sync::OnceLock<crate::admission::AdmissionStore>,
    /// Identifies this agent process. A cut whose session a later registration
    /// has replaced is discarded rather than applied as current.
    agent_session_id: String,
    /// Whether the last cut could see everything. Only the transition is worth
    /// saying: the condition needs an operator, and it does not clear itself.
    inventory_was_complete: AtomicBool,
    /// Whether the last heartbeat asked for a reconcile, so the one that finds
    /// nothing left to reconcile still asks — see `judge_cut`.
    asked_for_reconcile: AtomicBool,
}

impl NodeReporter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hostname: String,
        controller_addr: String,
        resources: ResourceSet,
        node_address: spur_net::NodeAddress,
        labels: HashMap<String, String>,
        join_token: String,
        wg_iface: String,
        wg_config_dir: std::path::PathBuf,
        held_jobs: Arc<dyn HeldJobs>,
    ) -> Self {
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
            held_jobs,
            allocation: std::sync::OnceLock::new(),
            k0s_status: std::sync::OnceLock::new(),
            admissions: std::sync::OnceLock::new(),
            agent_session_id: uuid::Uuid::new_v4().to_string(),
            inventory_was_complete: AtomicBool::new(true),
            asked_for_reconcile: AtomicBool::new(false),
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

    /// Wire the ledger once the state root is resolved. Registration before this
    /// sends no ledger, which the controller reads as no evidence.
    pub fn set_admissions(&self, admissions: crate::admission::AdmissionStore) {
        let _ = self.admissions.set(admissions);
    }

    fn ledger_cut(&self) -> Option<spur_proto::proto::NodeLedger> {
        let cut = self.admissions.get()?.ledger_cut(&self.agent_session_id);
        Some(ledger_to_proto(cut))
    }

    /// Whether to ask the controller to reconcile this node. Unlatched, so it
    /// clears when the reconcile lands; off the runtime, because it reads disk.
    pub(crate) async fn wants_reconcile(&self) -> bool {
        let Some(admissions) = self.admissions.get().cloned() else {
            return false;
        };
        let session = self.agent_session_id.clone();
        match tokio::task::spawn_blocking(move || admissions.ledger_cut(&session)).await {
            Ok(cut) => self.judge_cut(&cut),
            Err(error) => {
                warn!(%error, "could not read this node's ledger for the heartbeat");
                false
            }
        }
    }

    /// Only the transition is worth saying: an unreadable record needs an
    /// operator, and no reconcile the controller could run would clear it.
    fn judge_cut(&self, cut: &crate::admission::LedgerCut) -> bool {
        let complete = cut.inventory_complete;
        if self
            .inventory_was_complete
            .swap(complete, Ordering::Relaxed)
            != complete
        {
            if complete {
                info!("every admission record on this node is readable again");
            } else {
                error!(
                    "an admission record on this node cannot be read; it may hold a claim \
                     nothing can account for, and only an operator can clear it"
                );
            }
        }
        let wants = cut.wants_reconcile();
        // Keep asking after the holds clear, until a pull actually lands: the
        // controller retires the reason it wrote on this node only from a cut.
        if wants {
            self.asked_for_reconcile.store(true, Ordering::Relaxed);
        }
        wants || self.asked_for_reconcile.load(Ordering::Relaxed)
    }

    /// The controller has taken a cut, so whatever this node was asking it to
    /// look at has now been looked at.
    pub(crate) fn note_ledger_pulled(&self) {
        self.asked_for_reconcile.store(false, Ordering::Relaxed);
    }

    /// Identifies this agent process. A cut whose session a later registration
    /// has replaced is discarded rather than applied as current.
    pub fn agent_session_id(&self) -> &str {
        &self.agent_session_id
    }

    /// This node's current WireGuard mesh public key (empty if the interface has no key / no mesh).
    /// Read live from the interface so a key that appears or changes after startup is picked up.
    fn wg_pubkey(&self) -> String {
        spur_net::wireguard::interface_public_key(&self.wg_iface).unwrap_or_default()
    }

    /// Job ids this heartbeat would report, from the shared running map.
    pub fn held_job_ids(&self) -> Vec<u32> {
        self.held_jobs.held_job_ids()
    }

    pub fn snapshot_resources(&self) -> ResourceSet {
        self.resources.read().unwrap().clone()
    }

    /// Swap the reported inventory if its schedulable content changed. Ignores
    /// `generation` so a pure generation bump does not itself count as a change.
    pub fn update_resources(&self, fresh: ResourceSet) -> bool {
        let mut cur = self.resources.write().unwrap();
        let changed = cur.cpus != fresh.cpus
            || cur.memory_mb != fresh.memory_mb
            || cur.gpus != fresh.gpus
            || cur.generic != fresh.generic;
        *cur = fresh;
        changed
    }

    /// Register with the controller, reporting the reporter's current inventory.
    pub async fn register(&self) -> anyhow::Result<()> {
        self.register_with(&self.snapshot_resources()).await
    }

    /// Register with the controller reporting a specific inventory. The refresh
    /// task uses this to converge on a fresh set WITHOUT first committing it to
    /// the reporter baseline, so a failed register leaves the baseline unchanged
    /// and the next tick re-detects the same delta and retries.
    pub async fn register_with(&self, resources: &ResourceSet) -> anyhow::Result<()> {
        let mut client = crate::controller_auth::connect(&self.controller_addr)
            .await
            .context("failed to connect to spurctld for registration")?;

        let mut labels = self.labels.clone();
        labels.insert("spur.stepd".into(), "1".into());

        let resources = resource_to_proto(resources);
        let resp = client
            .register_agent(RegisterAgentRequest {
                hostname: self.hostname.clone(),
                resources: Some(resources),
                version: env!("CARGO_PKG_VERSION").into(),
                address: self.node_address.ip.clone(),
                port: self.node_address.port as u32,
                wg_pubkey: self.wg_pubkey(),
                labels,
                join_token: self.join_token.clone(),
                ledger: self.ledger_cut(),
                runs_job_epilog: false,
            })
            .await
            .context("registration failed")?;

        let inner = resp.into_inner();
        if inner.accepted {
            warn_without_node_identity(&inner.node_token);
            if !inner.node_token.is_empty() {
                *self.node_token.write().unwrap() = inner.node_token;
            }
            info!("registered with controller");
        } else {
            anyhow::bail!("controller rejected registration: {}", inner.message);
        }

        Ok(())
    }

    /// Notify the controller that this agent is shutting down.
    pub async fn deregister(&self, reason: &str) -> anyhow::Result<()> {
        let current_token = self.node_token.read().unwrap().clone();
        let mut client = crate::controller_auth::connect(&self.controller_addr)
            .await
            .context("failed to connect to spurctld for deregistration")?;

        client
            .deregister_agent(spur_proto::proto::DeregisterAgentRequest {
                hostname: self.hostname.clone(),
                node_token: current_token,
                reason: reason.to_string(),
            })
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
        let mut client = crate::controller_auth::connect(&self.controller_addr)
            .await
            .context("failed to connect to spurctld for runtime recovery")?;
        let node_token = self
            .node_token
            .read()
            .map_err(|_| anyhow::anyhow!("runtime recovery node token lock poisoned"))?
            .clone();
        let response = client
            .report_stepd_recovery(StepdRecoveryRequest {
                hostname: self.hostname.clone(),
                job_id,
                run_attempt,
                node_token,
                stale_descriptor,
                step_id,
            })
            .await
            .context("runtime recovery report failed")?;
        Ok(response.into_inner())
    }

    /// Periodic heartbeat loop.
    pub async fn heartbeat_loop(&self) {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));

        loop {
            interval.tick().await;

            let (load, free_mem) = read_system_metrics();
            self.cpu_load.store(load as u64, Ordering::Relaxed);
            self.free_memory_mb.store(free_mem, Ordering::Relaxed);
            let current_token = self.node_token.read().unwrap().clone();
            let running_jobs = build_running_jobs(self.held_job_ids(), &self.held_job_gpu_ids());
            let needs_reconcile = self.wants_reconcile().await;
            if needs_reconcile {
                warn!("holding evidence only the controller can resolve; asking it to reconcile");
            }

            match crate::controller_auth::connect(&self.controller_addr).await {
                Ok(mut client) => {
                    match client
                        .heartbeat(spur_proto::proto::HeartbeatRequest {
                            hostname: self.hostname.clone(),
                            cpu_load: load,
                            free_memory_mb: free_mem,
                            running_jobs,
                            node_token: current_token,
                            wg_pubkey: self.wg_pubkey(),
                            k0s_status: self.k0s_status.get().map(|s| {
                                let (unit_active, restart_count, install_secs) =
                                    s.take_for_heartbeat();
                                spur_proto::proto::K0sNodeStatus {
                                    unit_active,
                                    restart_count,
                                    install_duration_seconds: install_secs,
                                }
                            }),
                            needs_reconcile,
                        })
                        .await
                    {
                        Ok(_) => debug!(load, free_mem, "heartbeat sent"),
                        Err(e) if should_reregister(&e) => {
                            warn!(
                                error = %e,
                                "controller does not recognize this node; re-registering"
                            );
                            if let Err(e) = self.register().await {
                                warn!(error = %e, "re-registration after heartbeat rejection failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "heartbeat failed"),
                    }
                }
                Err(e) => warn!(error = %e, "heartbeat connection failed"),
            }
        }
    }
}

/// Build the heartbeat's per-job status list. Each held job carries its
/// translated GPU stable_ids (empty when it holds no GPUs), so the controller
/// can converge its per-node used-view to what the job actually occupies.
fn build_running_jobs(
    held_ids: Vec<u32>,
    gpu_ids: &HashMap<u32, Vec<u64>>,
) -> Vec<RunningJobStatus> {
    held_ids
        .into_iter()
        .map(|job_id| RunningJobStatus {
            job_id,
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
pub type InventoryFingerprint = (u32, u64, Vec<u64>, Vec<(String, u64)>);

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
    {
        return InventoryDelta::Unchanged;
    }
    let new_ids: std::collections::HashSet<u64> = new.gpus.iter().map(|g| g.stable_id).collect();
    let lost_held = allocated_stable_ids.iter().any(|id| !new_ids.contains(id));
    if lost_held {
        InventoryDelta::AllocatedDevicesLost
    } else {
        InventoryDelta::FreeCapacityChanged
    }
}

/// Physical-GPU identity (BDF anchor) of a stable_id: mask off the low
/// partition bits so two ids on the same silicon compare equal.
fn bdf_anchor(stable_id: u64) -> u64 {
    stable_id & !spur_devices::cdi::STABLE_ID_PARTITION_MASK
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
    let held_bdfs: std::collections::HashSet<u64> =
        held_ids.iter().map(|&id| bdf_anchor(id)).collect();

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
            reconciled.gpus.push(gpu.clone());
        }
    }
    reconciled
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
    }
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
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
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
                stable_id: g.stable_id,
            })
            .collect(),
        generic: r.generic.clone(),
        generation: r.generation,
    }
}

pub fn ledger_to_proto(cut: crate::admission::LedgerCut) -> spur_proto::proto::NodeLedger {
    spur_proto::proto::NodeLedger {
        agent_session_id: cut.agent_session_id,
        inventory_complete: cut.inventory_complete,
        entries: cut
            .entries
            .into_iter()
            .map(|entry| spur_proto::proto::LedgerEntry {
                job_id: entry.job_id,
                run_attempt: entry.run_attempt,
                cpu_ids: entry.allocation.cpu_ids,
                memory_mb: entry.allocation.memory_mb,
                gpu_devices: entry.allocation.gpu_devices,
                disposition: entry.disposition.as_str().to_string(),
                conflict_hold: entry.conflict_hold,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_devices::cdi::annotations;
    use spur_devices::cdi::cache::CdiCache;
    use spur_devices::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, DeviceNode};
    use spur_devices::{DeviceRegistry, GresCache, GresEntry};

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
        let mut held = vec![7u32, 9u32];
        held.sort_unstable();
        let gpu_ids: HashMap<u32, Vec<u64>> = [(7u32, vec![0x63_0000u64, 0x83_0000u64])]
            .into_iter()
            .collect();

        let mut jobs = build_running_jobs(held, &gpu_ids);
        jobs.sort_by_key(|j| j.job_id);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].job_id, 7);
        assert_eq!(jobs[0].gpu_stable_ids, vec![0x63_0000, 0x83_0000]);
        assert_eq!(jobs[1].job_id, 9);
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

    #[test]
    fn held_job_ids_accepts_send_but_not_sync_values() {
        // Cell is Send but !Sync; this fails to compile if the bound re-tightens to Sync.
        let map: Mutex<HashMap<u32, std::cell::Cell<u8>>> =
            Mutex::new(HashMap::from([(7, std::cell::Cell::new(0))]));
        assert_eq!(map.held_job_ids(), vec![7]);
    }

    fn test_reporter(resources: ResourceSet) -> NodeReporter {
        NodeReporter::new(
            "test-node".into(),
            "http://localhost:6817".into(),
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
            Arc::new(Mutex::new(HashMap::<u32, ()>::new())),
        )
    }

    fn a_cut(conflict_hold: bool) -> crate::admission::LedgerCut {
        crate::admission::LedgerCut {
            agent_session_id: "session-a".into(),
            inventory_complete: true,
            entries: vec![crate::admission::LedgerCutEntry {
                job_id: 42,
                run_attempt: 1,
                allocation: Default::default(),
                disposition: spur_core::job::LedgerDisposition::Unresolved,
                conflict_hold,
            }],
        }
    }

    #[test]
    fn a_node_keeps_asking_after_its_holds_clear_until_a_pull_lands() {
        let reporter = test_reporter(ResourceSet::default());

        assert!(reporter.judge_cut(&a_cut(true)));
        // The controller paces pulls, so the heartbeat that first reports "nothing
        // left" can be refused; asking until a cut is taken cannot be.
        assert!(
            reporter.judge_cut(&a_cut(false)),
            "the reason the controller wrote on this node is retired only by a pull"
        );
        assert!(reporter.judge_cut(&a_cut(false)), "and the next one too");

        reporter.note_ledger_pulled();
        assert!(
            !reporter.judge_cut(&a_cut(false)),
            "the cut was taken, so there is nothing left to ask for"
        );
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
            stable_id: s,
        };
        let rs = |gpus: Vec<GpuResource>| ResourceSet {
            cpus: 8,
            memory_mb: 1024,
            gpus,
            generic: Default::default(),
            generation: 0,
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
            stable_id,
        }
    }

    fn rset(cpus: u32, generation: u64, gpus: Vec<GpuResource>) -> ResourceSet {
        ResourceSet {
            cpus,
            memory_mb: 1024,
            gpus,
            generic: Default::default(),
            generation,
        }
    }

    /// stable_id encoded like `encode_stable_id`: BDF anchor (`bus` in the high
    /// bits) plus a low-8-bit partition rank. Two ids with the same `bus` share
    /// physical silicon.
    fn sid(bus: u64, partition: u64) -> u64 {
        (bus << 16) | (partition & spur_devices::cdi::STABLE_ID_PARTITION_MASK)
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
    fn reconcile_of_a_held_repartition_equals_baseline_fingerprint() {
        use std::collections::HashSet;
        // Oscillation guard: while a held device (BDF 5) stays held, every fresh
        // scan that only repartitions its silicon reconciles to the SAME
        // schedulable set as the baseline. The refresh loop compares fingerprints
        // and skips the re-register/warn, so repeated ReportLost ticks register at
        // most once (here: zero, the set is unchanged).
        let baseline = rset(8, 1, vec![gpu(0, sid(5, 0)), gpu(1, sid(7, 0))]);
        // The held render node vanished, replaced by a CPX slice sid(5,1).
        let fresh = rset(8, 2, vec![gpu(0, sid(5, 1)), gpu(1, sid(7, 0))]);
        let held: HashSet<u64> = [sid(5, 0)].into_iter().collect();

        let reconciled = reconcile_free_pool(&fresh, &baseline, &held);
        assert_eq!(
            reconciled.schedulable_fingerprint(),
            baseline.schedulable_fingerprint()
        );
    }

    #[test]
    fn after_release_next_classify_applies_current_baseline() {
        use spur_core::job::RunKey;
        use spur_sched::cons_tres::{CapacityChange, NodeAllocation, ReleaseWarrant};
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
        assert!(node.release_job(ReleaseWarrant::never_spawned(RunKey::any_attempt(7))));
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
    fn update_resources_reports_change_ignoring_generation() {
        let base = ResourceSet {
            cpus: 4,
            memory_mb: 512,
            gpus: vec![],
            generic: Default::default(),
            generation: 1,
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
        (cpus, 1024, gpu_ids.to_vec(), Vec::new())
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
}
