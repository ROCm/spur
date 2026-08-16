// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Native k0s cluster controller (leader-gated).
//!
//! Distinct from `crate::cluster` (the raft-backed `ClusterManager` state machine): this drives
//! the SPUR-managed k0s cluster — role selection, IP/CIDR allocation, and the
//! per-node `SlurmAgent` StartClusterComponent/StopClusterComponent fan-out — all
//! gated on Raft leadership. Phase transitions go through `ClusterManager::set_k0s_phase`
//! (WAL-replicated) so a leadership change mid-provision is safe.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tracing::{info, warn};

use spur_core::k0s::{K0sClusterState, K0sPhase, K0sRole};
use spur_net::address::AddressPool;
use spur_net::mesh::{MeshMembership, MeshNode};
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::{
    ClusterNodeStatus, CreateK0sJoinTokenRequest, GetClusterComponentStatusRequest,
    GetKubeconfigRequest, StartClusterComponentRequest, StopClusterComponentRequest,
};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

/// Per-agent RPC dial timeout.
const AGENT_TIMEOUT: Duration = Duration::from_secs(5);

const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Network CIDRs the reconcile loop needs (from `[network]` + `[cluster]` config).
#[derive(Clone, Debug)]
pub struct ClusterNetworking {
    /// WireGuard mesh CIDR (network.wg_cidr) — node mesh IPs are allocated from here.
    pub mesh_cidr: String,
    /// Pod CIDR (cluster.pod_cidr) — per-node /24s are carved from here.
    pub pod_cidr: String,
    /// Service CIDR (cluster.service_cidr) — for the generated k0s config.
    pub service_cidr: String,
    /// CNI MTU (cluster.cni_mtu) — emitted into the generated Calico config.
    pub cni_mtu: u16,
    /// CNI mode (cluster.cni): "kuberouter" (default) or "calico" (mesh-native config + node-ip).
    pub cni: String,
    /// Operator-pinned control-plane node (cluster.control_plane_node), if any.
    pub control_plane_node: Option<String>,
    /// How long a k8s (k0s) node may stay non-`active` during provisioning before the loop
    /// marks the cluster `degraded` (cluster.k8s_provisioning_timeout_secs).
    pub provisioning_timeout: Duration,
}

/// Leader-gated k0s reconcile loop. Spawned from `main.rs` when `[cluster].enabled`; it still
/// re-checks leadership every tick because leadership can flip at any time.
pub async fn run(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>, net: ClusterNetworking) {
    info!(mesh = %net.mesh_cidr, pod = %net.pod_cidr, "k0s cluster reconcile loop started");
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    let mut last_mesh: Vec<MeshNode> = Vec::new();
    // Cache of the join token minted per node (worker or secondary control-plane), so we mint once
    // (not every tick) while it joins — re-minting churns k0s server tokens and races the join.
    let mut join_tokens: HashMap<String, String> = HashMap::new();
    // Leader-local provisioning clock: a leader-flip or restart resets it, so the timeout re-arms
    // on the new leader rather than tripping instantly off a persisted start time.
    let mut provisioning_since: Option<Instant> = None;
    loop {
        interval.tick().await;
        if !raft.is_leader() {
            last_mesh.clear(); // forget on leadership loss so a new term re-logs the membership
            provisioning_since = None;
            continue; // only the leader reconciles
        }
        let state = cluster.k0s_state();

        let timed_out = update_provisioning_clock(
            state.phase,
            &mut provisioning_since,
            Instant::now(),
            net.provisioning_timeout,
        );

        // Mesh: derive the authoritative full-mesh membership (pubkey + mesh IP + pod /24) from
        // live inventory and push it to every meshed node's agent (ApplyMesh) so a native-routing
        // CNI can ride the WireGuard mesh. Level-triggered: re-push EVERY tick (the agent's
        // reconcile_mesh is idempotent + prunes) so node-local drift (reboot, wg restart), a failed
        // push, and controller failover all self-heal. Only meaningful with ≥2 meshed nodes; the
        // membership diff gates only the log line, not the push.
        let mesh = build_mesh_membership(&cluster);
        if mesh.nodes.len() >= 2 {
            if mesh.nodes != last_mesh {
                info!(
                    members = mesh.nodes.len(),
                    "k0s full-mesh membership changed"
                );
            }
            for node in &mesh.nodes {
                spawn_apply_mesh(&cluster, &node.hostname, &mesh);
            }
        }
        last_mesh = mesh.nodes.clone();

        let started = Instant::now();
        let errored = reconcile_phase(&cluster, &net, &state, &mut join_tokens, timed_out).await;
        let cluster_name = cluster.config().cluster_name.clone();
        cluster
            .k8s_metrics()
            .observe_reconcile_duration(&cluster_name, started.elapsed().as_secs_f64());
        if errored {
            cluster.k8s_metrics().record_reconcile_error(&cluster_name);
        }
    }
}

/// Advance the leader-local provisioning clock and report whether the deadline passed. Arms on the
/// first Provisioning observation and resets on any other phase, so re-entry restarts the timer.
fn update_provisioning_clock(
    phase: K0sPhase,
    since: &mut Option<Instant>,
    now: Instant,
    timeout: Duration,
) -> bool {
    if phase != K0sPhase::Provisioning {
        *since = None;
        return false;
    }
    let started = *since.get_or_insert(now);
    now.saturating_duration_since(started) >= timeout
}

/// Run one reconcile tick for the current phase. Extracted from `run` so it is testable.
///
/// Ready and Provisioning both run the assignment + converge reconcile so the cluster self-heals: a
/// node that is removed then re-added (a spurd restart deregisters on SIGTERM, dropping the node +
/// its k0s assignment) or a node added while Ready gets (re)assigned a role/IP/CIDR, (re)joined, and
/// rejoins the mesh membership on the next ApplyMesh tick. Idempotent — assigned + active nodes are
/// skipped — so a converged cluster does no work beyond the per-node status probes. Without running
/// this in Ready, a re-added node stays un-roled (out of the mesh) until the next manual `spur k8s up`.
pub(crate) async fn reconcile_phase(
    cluster: &ClusterManager,
    net: &ClusterNetworking,
    state: &K0sClusterState,
    join_tokens: &mut HashMap<String, String>,
    timed_out: bool,
) -> bool {
    match state.phase {
        K0sPhase::Ready | K0sPhase::Provisioning => {
            if let Err(e) = provision_assignments(cluster, net, state) {
                warn!(error = %e, "k0s provisioning assignment failed; will retry next tick");
                return true;
            }
            let errors = converge_provisioning(cluster, net, join_tokens).await;
            // converge may have flipped us to Ready this tick; only degrade a still-provisioning
            // cluster that has blown its deadline.
            if timed_out && cluster.k0s_state().phase == K0sPhase::Provisioning {
                degrade_stuck_cluster(cluster, net, join_tokens).await;
            }
            errors > 0
        }
        K0sPhase::Down => {
            stop_all_components(cluster, state.reset_requested).await;
            false
        }
        K0sPhase::Degraded => {
            warn!("k0s cluster degraded");
            false
        }
    }
}

/// Assign role + mesh IP + pod /24 to any node that lacks one. Idempotent: a node's persisted
/// `k0s_role`/`k0s_mesh_ip`/`k0s_pod_cidr` IS the allocation record, so assigned nodes are skipped
/// and never re-allocated. The (in-memory) AddressPool is re-seeded from persisted inventory on
/// every call — skipping that would hand out IPs already in use after a controller restart.
pub(crate) fn provision_assignments(
    cluster: &ClusterManager,
    net: &ClusterNetworking,
    state: &K0sClusterState,
) -> anyhow::Result<()> {
    let mut nodes = cluster.get_nodes();
    nodes.retain(|n| state.is_member(&n.name));
    if nodes.is_empty() {
        return Ok(());
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name)); // deterministic

    // Bootstrap control-plane (etcd seed, holder of `.1`). Recorded bootstrap outranks a scanned
    // role (secondary CPs also carry `Controller`) so `.1` stays put across a 1->3 grow.
    let bootstrap = state
        .bootstrap()
        .or_else(|| {
            nodes
                .iter()
                .find(|n| matches!(n.k0s_role, Some(K0sRole::Single | K0sRole::Controller)))
                .map(|n| n.name.clone())
        })
        .or_else(|| net.control_plane_node.clone())
        .unwrap_or_else(|| nodes[0].name.clone());

    // Control-plane set: the persisted HA set (from `cluster_up`), or just the bootstrap for the
    // legacy single-CP path where no set was recorded.
    let mut cp_set: HashSet<String> = state.controllers().into_iter().collect();
    if cp_set.is_empty() {
        cp_set.insert(bootstrap.clone());
    }

    // Re-seed the mesh pool from persisted assignments + reserve .1 for the bootstrap controller.
    let mut pool = AddressPool::new(&net.mesh_cidr)?;
    let controller_ip = first_host(&net.mesh_cidr)?;
    let _ = pool.allocate_specific(controller_ip); // reserve .1 (ignore if already reserved)
    for n in &nodes {
        if let Some(ip) = &n.k0s_mesh_ip {
            let parsed: Ipv4Addr = ip
                .parse()
                .with_context(|| format!("persisted k0s_mesh_ip {ip} for {}", n.name))?;
            pool.mark_allocated(parsed);
        }
    }

    // Pod-/24 ordinals already in use (so a new node never collides).
    let pod_base = cidr_base(&net.pod_cidr)?;
    let mut used_ordinals: HashSet<u32> = nodes
        .iter()
        .filter_map(|n| n.k0s_pod_cidr.as_deref())
        .filter_map(|c| pod_ordinal(c, pod_base))
        .collect();

    let single = nodes.len() == 1;
    // Two passes so control planes take the lowest mesh IPs deterministically (bootstrap keeps `.1`,
    // secondary CPs `.2`/`.3`...) regardless of where they sort among workers.
    let ordered: Vec<_> = nodes
        .iter()
        .filter(|n| cp_set.contains(&n.name))
        .chain(nodes.iter().filter(|n| !cp_set.contains(&n.name)))
        .collect();
    for node in ordered {
        if node.k0s_role.is_some() {
            continue; // already assigned
        }
        let is_cp = cp_set.contains(&node.name);
        let role = if is_cp {
            if single {
                K0sRole::Single
            } else {
                K0sRole::Controller
            }
        } else {
            K0sRole::Worker
        };
        let mesh_ip = if node.name == bootstrap {
            controller_ip
        } else {
            pool.allocate()?
        };
        let ordinal = next_free_ordinal(&used_ordinals);
        used_ordinals.insert(ordinal);
        let pod_cidr = carve_pod_cidr(&net.pod_cidr, ordinal)?;
        cluster.assign_node_k0s(&node.name, role, &mesh_ip.to_string(), &pod_cidr)?;
    }

    // Persist the bootstrap choice if not already recorded (legacy single-CP path; `cluster_up`
    // records the full set up front for HA).
    if state.control_plane_node.as_deref() != Some(bootstrap.as_str()) {
        cluster.set_k0s_phase(
            K0sPhase::Provisioning,
            Some(bootstrap),
            Vec::new(),
            Vec::new(),
            false,
        )?;
    }
    Ok(())
}

/// Resolve the member scope for `spur k8s up` fail-closed: the UNION of a `nodes` hostlist, a
/// `partition`'s members, and a label `selector` (all pairs match). Empty (nothing given) = whole inventory.
pub(crate) fn resolve_member_nodes(
    all_nodes: &[spur_core::node::Node],
    nodes_hostlist: &str,
    partition: &str,
    selector: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    if nodes_hostlist.is_empty() && partition.is_empty() && selector.is_empty() {
        return Ok(Vec::new());
    }
    let registered: HashSet<&str> = all_nodes.iter().map(|n| n.name.as_str()).collect();
    let mut members: HashSet<String> = HashSet::new();

    if !nodes_hostlist.is_empty() {
        let expanded = spur_core::hostlist::expand(nodes_hostlist)
            .map_err(|e| format!("invalid --nodes hostlist {nodes_hostlist}: {e}"))?;
        for name in expanded {
            if !registered.contains(name.as_str()) {
                return Err(format!("node {name} is not a registered node"));
            }
            members.insert(name);
        }
    }
    if !partition.is_empty() {
        let mut any = false;
        for n in all_nodes {
            if n.partitions.iter().any(|p| p == partition) {
                members.insert(n.name.clone());
                any = true;
            }
        }
        if !any {
            return Err(format!("partition {partition} has no registered nodes"));
        }
    }
    if !selector.is_empty() {
        let mut any = false;
        for n in all_nodes {
            if selector.iter().all(|(k, v)| n.labels.get(k) == Some(v)) {
                members.insert(n.name.clone());
                any = true;
            }
        }
        if !any {
            return Err("--selector matched no registered nodes".to_string());
        }
    }
    if members.is_empty() {
        return Err("node selection matched no registered nodes".to_string());
    }
    let mut out: Vec<String> = members.into_iter().collect();
    out.sort();
    Ok(out)
}

/// Resolve the control-plane set for `spur k8s up`, fail-closed, bootstrap node first: an explicit
/// `nodes` list wins, else the lowest `replicas` candidates. Count must be 1/3/5 and fit the nodes.
pub(crate) fn resolve_control_plane_set(
    mut candidates: Vec<String>,
    explicit: &[String],
    pinned_bootstrap: Option<&str>,
    replicas: u32,
) -> Result<Vec<String>, String> {
    candidates.sort();
    candidates.dedup();
    if !explicit.is_empty() {
        spur_core::k0s::validate_control_plane_replicas(explicit.len() as u32)?;
        let mut seen = HashSet::new();
        for n in explicit {
            if !candidates.contains(n) {
                return Err(format!("control-plane node {n} is not a registered node"));
            }
            if !seen.insert(n.clone()) {
                return Err(format!("duplicate control-plane node {n}"));
            }
        }
        // Fail closed on a contradictory bootstrap: if a bootstrap is pinned (operator override or a
        // previously-recorded CP) but absent from the explicit list, `.1`/etcd-seed would silently
        // land on a different node than intended.
        if let Some(boot) = pinned_bootstrap {
            if !explicit.iter().any(|n| n == boot) {
                return Err(format!(
                    "bootstrap control-plane {boot} is not in the requested set [{}]",
                    explicit.join(", ")
                ));
            }
        }
        let mut set = explicit.to_vec();
        order_bootstrap_first(&mut set, pinned_bootstrap);
        return Ok(set);
    }
    spur_core::k0s::validate_control_plane_replicas(replicas)?;
    if replicas as usize > candidates.len() {
        return Err(format!(
            "requested {replicas} control planes but only {} node(s) are registered",
            candidates.len()
        ));
    }
    // Fail closed on a pinned bootstrap outside the candidate set (e.g. a --control-plane-node not in
    // the requested node scope) — else `.1`/etcd-seed silently lands on a different, in-scope node.
    if let Some(boot) = pinned_bootstrap {
        if !candidates.iter().any(|c| c == boot) {
            return Err(format!(
                "control-plane node {boot} is not among the selected cluster nodes"
            ));
        }
    }
    // Pin the bootstrap into the set first so `.1` lands on it, then fill from the lowest names.
    let mut set: Vec<String> = Vec::new();
    if let Some(boot) = pinned_bootstrap {
        set.push(boot.to_string());
    }
    for c in candidates {
        if set.len() >= replicas as usize {
            break;
        }
        if !set.contains(&c) {
            set.push(c);
        }
    }
    order_bootstrap_first(&mut set, pinned_bootstrap);
    Ok(set)
}

/// Move the pinned bootstrap node to the front of the CP set (it holds `.1` + seeds etcd).
fn order_bootstrap_first(set: &mut [String], pinned_bootstrap: Option<&str>) {
    if let Some(boot) = pinned_bootstrap {
        if let Some(pos) = set.iter().position(|n| n == boot) {
            set.swap(0, pos);
        }
    }
}

/// The `.1` host of a CIDR (mesh controller IP).
fn first_host(cidr: &str) -> anyhow::Result<Ipv4Addr> {
    Ok(Ipv4Addr::from(u32::from(cidr_base(cidr)?) + 1))
}

/// The base address of a CIDR string.
fn cidr_base(cidr: &str) -> anyhow::Result<Ipv4Addr> {
    let (base, _) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("{cidr} is not a CIDR"))?;
    base.parse().with_context(|| format!("CIDR base in {cidr}"))
}

/// Carve a per-node pod /24 out of `pod_cidr` by ordinal, e.g. ("10.42.0.0/16", 2) -> "10.42.2.0/24".
fn carve_pod_cidr(pod_cidr: &str, ordinal: u32) -> anyhow::Result<String> {
    let (base, prefix) = pod_cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("{pod_cidr} is not a CIDR"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("pod_cidr prefix in {pod_cidr}"))?;
    if prefix > 24 {
        anyhow::bail!("pod_cidr {pod_cidr} must be /24 or larger to carve per-node /24s");
    }
    let base: Ipv4Addr = base
        .parse()
        .with_context(|| format!("pod_cidr base in {pod_cidr}"))?;
    let num_24s = 1u32 << (24 - prefix);
    if ordinal >= num_24s {
        anyhow::bail!("pod ordinal {ordinal} exceeds {num_24s} /24s in {pod_cidr}");
    }
    let carved = u32::from(base) + (ordinal << 8);
    Ok(format!("{}/24", Ipv4Addr::from(carved)))
}

/// Inverse of `carve_pod_cidr`: the ordinal of a per-node /24 within `pod_base`.
fn pod_ordinal(node_cidr: &str, pod_base: Ipv4Addr) -> Option<u32> {
    let (b, _) = node_cidr.split_once('/')?;
    let nb: Ipv4Addr = b.parse().ok()?;
    Some(u32::from(nb).checked_sub(u32::from(pod_base))? >> 8)
}

/// Smallest non-negative ordinal not already in use.
fn next_free_ordinal(used: &HashSet<u32>) -> u32 {
    let mut o = 0;
    while used.contains(&o) {
        o += 1;
    }
    o
}

/// Proto status string for a cluster phase.
pub fn phase_str(p: K0sPhase) -> String {
    match p {
        K0sPhase::Down => "down",
        K0sPhase::Provisioning => "provisioning",
        K0sPhase::Ready => "ready",
        K0sPhase::Degraded => "degraded",
    }
    .to_string()
}

fn role_str(r: K0sRole) -> String {
    match r {
        K0sRole::Controller => "controller",
        K0sRole::Worker => "worker",
        K0sRole::Single => "single",
    }
    .to_string()
}

/// Per-node status list from persisted (Raft-replicated) k0s state only (no agent round-trip).
pub fn node_statuses(cluster: &ClusterManager) -> Vec<ClusterNodeStatus> {
    cluster
        .get_nodes()
        .into_iter()
        .filter_map(|n| {
            let role = n.k0s_role?;
            Some(ClusterNodeStatus {
                node: n.name,
                role: role_str(role),
                component_state: "unknown".to_string(),
                enabled: true,
                reason: n.k0s_last_error.unwrap_or_default(),
            })
        })
        .collect()
}

/// Build the authoritative full-mesh membership from live node inventory: every node that has both
/// joined the WireGuard mesh (non-empty `wg_pubkey`, reported at registration) and been assigned a
/// mesh IP. Each entry carries the node's pod /24, so a native-routing CNI (Calico `bird`) can ride
/// the mesh — the controller is the source of truth for `MeshNode.public_key`/`pod_cidr`, which an
/// operator feeds to `apply_mesh` on each node via `spur net mesh --config`. Nodes not yet on the
/// mesh (no pubkey) are skipped rather than fabricated, so an incomplete membership is never emitted.
pub fn build_mesh_membership(cluster: &ClusterManager) -> MeshMembership {
    mesh_from_nodes(cluster.get_nodes())
}

/// Pure core of [`build_mesh_membership`] (testable without a `ClusterManager`).
fn mesh_from_nodes(nodes: Vec<spur_core::node::Node>) -> MeshMembership {
    let mut nodes: Vec<MeshNode> = nodes
        .into_iter()
        .filter_map(|n| {
            let mesh_ip = n.k0s_mesh_ip.clone()?;
            let public_key = n.wg_pubkey.clone().filter(|k| !k.is_empty())?;
            Some(MeshNode {
                hostname: n.name,
                public_key,
                mesh_ip,
                // No endpoint: Node.address is the agent's advertised address, which
                // `detect_node_address` makes the *mesh* IP when WireGuard is up — not a valid WG
                // underlay endpoint (using it would clobber the working tunnel). Empty makes
                // apply_mesh preserve the endpoint `spur net join` already established; membership
                // reconciliation only maintains peers + AllowedIPs, not the underlay tunnel.
                endpoint: String::new(),
                pod_cidr: n.k0s_pod_cidr.clone(),
            })
        })
        .collect();
    // Sort numerically by IPv4 — a string sort orders "10.44.0.10" before "10.44.0.2", producing
    // spurious membership diffs (and unnecessary ApplyMesh pushes) between ticks.
    nodes.sort_by(|a, b| {
        a.mesh_ip
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .cmp(&b.mesh_ip.parse::<std::net::Ipv4Addr>().ok())
            .then_with(|| a.mesh_ip.cmp(&b.mesh_ip))
    });
    MeshMembership { nodes }
}

/// Resolve a node's agent endpoint (`http://addr:port`), or None if it has no address.
fn agent_endpoint(cluster: &ClusterManager, node: &str) -> Option<String> {
    let n = cluster.get_node(node)?;
    let addr = n.address?;
    Some(format!("http://{}:{}", addr, n.port))
}

/// Dial a healthy control-plane agent, timing out per `AGENT_TIMEOUT`. Tries the bootstrap node
/// first (its k0s API answers admin/token RPCs) then any other control plane, so admin/kubeconfig/
/// token minting survive the loss of a single control plane while etcd quorum holds. Errors if no
/// control-plane node is assigned yet or none is reachable.
async fn connect_control_plane(
    cluster: &ClusterManager,
) -> anyhow::Result<SlurmAgentClient<crate::agent_client::AgentChannel>> {
    let state = cluster.k0s_state();
    let mut candidates = state.controllers();
    if candidates.is_empty() {
        anyhow::bail!("no control-plane node assigned yet");
    }
    // Bootstrap node first (stable admin endpoint), then the rest as failover.
    if let Some(boot) = &state.control_plane_node {
        if let Some(pos) = candidates.iter().position(|n| n == boot) {
            candidates.swap(0, pos);
        }
    }
    let mut last_err = String::new();
    for cp in &candidates {
        let Some(endpoint) = agent_endpoint(cluster, cp) else {
            last_err = format!("control-plane node {cp} has no agent address");
            continue;
        };
        match tokio::time::timeout(AGENT_TIMEOUT, crate::agent_client::connect(endpoint)).await {
            Ok(Ok(client)) => return Ok(client),
            Ok(Err(e)) => last_err = format!("connect to control-plane agent {cp} failed: {e}"),
            Err(_) => last_err = format!("connect to control-plane agent {cp} timed out"),
        }
    }
    anyhow::bail!("no reachable control-plane agent ({last_err})")
}

/// Mint a join token of `role` ("worker" | "controller") from a control-plane agent (`k0s token
/// create --role <role>`). Errors until a control-plane component is up (its k0s API must answer);
/// the caller retries. A controller token lets a secondary CP join the bootstrap's etcd quorum.
async fn mint_join_token(cluster: &ClusterManager, role: &str) -> anyhow::Result<String> {
    let mut client = connect_control_plane(cluster).await?;
    let resp = client
        .create_k0s_join_token(CreateK0sJoinTokenRequest {
            role: role.to_string(),
            expiry_seconds: 0, // k0s default lifetime
        })
        .await
        .map_err(|e| anyhow::anyhow!("create_k0s_join_token RPC failed: {e}"))?;
    Ok(resp.into_inner().join_token)
}

/// Fetch the admin kubeconfig from the control-plane node's agent (`k0s kubeconfig admin`), for the
/// ClusterKubeconfig RPC. Errors if there is no control-plane node yet or it is unreachable.
pub async fn fetch_admin_kubeconfig(cluster: &ClusterManager) -> anyhow::Result<String> {
    let mut client = connect_control_plane(cluster).await?;
    let resp = client
        .get_kubeconfig(GetKubeconfigRequest::default())
        .await
        .map_err(|e| anyhow::anyhow!("get_kubeconfig RPC failed: {e}"))?;
    Ok(resp.into_inner().kubeconfig)
}

/// Mint a namespace-scoped kubeconfig for a SPUR user: the control-plane agent ensures the
/// ServiceAccount exists in the account namespace and mints a bound token. `namespace` + `sa` are
/// derived by the caller from the user's account via `spur_core::quota_names`.
pub async fn fetch_user_kubeconfig(
    cluster: &ClusterManager,
    user: &str,
    namespace: &str,
    service_account: &str,
) -> anyhow::Result<String> {
    let mut client = connect_control_plane(cluster).await?;
    let resp = client
        .get_kubeconfig(GetKubeconfigRequest {
            user: user.to_string(),
            namespace: namespace.to_string(),
            service_account: service_account.to_string(),
        })
        .await
        .map_err(|e| anyhow::anyhow!("get_kubeconfig (scoped) RPC failed: {e}"))?;
    Ok(resp.into_inner().kubeconfig)
}

/// Query a node's live k0s component state via its agent, with a timeout. Returns None if the node
/// is unreachable or has no component yet.
async fn fetch_component_status(cluster: &ClusterManager, node: &str) -> Option<(String, bool)> {
    let endpoint = agent_endpoint(cluster, node)?;
    let fut = async {
        let mut client = crate::agent_client::connect(endpoint).await.ok()?;
        let resp = client
            .get_cluster_component_status(GetClusterComponentStatusRequest {})
            .await
            .ok()?;
        let r = resp.into_inner();
        Some((r.component_state, r.enabled))
    };
    tokio::time::timeout(AGENT_TIMEOUT, fut)
        .await
        .ok()
        .flatten()
}

async fn fetch_component_state(cluster: &ClusterManager, node: &str) -> Option<String> {
    fetch_component_status(cluster, node)
        .await
        .map(|(state, _)| state)
}

/// The mesh-native k0s controller config for `node` (api on its mesh IP + Calico bird), or None for
/// the default kube-router mode (`cni != "calico"`) / a node without a mesh IP.
fn controller_k0s_config(net: &ClusterNetworking, node: &spur_core::node::Node) -> Option<String> {
    let api = node.k0s_mesh_ip.as_deref()?;
    // SANs: the mesh IP (advertised) + the underlay address (so `kubectl` over either works).
    let mut sans = vec![api.to_string()];
    if let Some(addr) = &node.address {
        if addr != api {
            sans.push(addr.clone());
        }
    }
    spur_core::k0s::k0s_controller_config_yaml(
        &net.cni,
        &net.pod_cidr,
        &net.service_cidr,
        net.cni_mtu,
        api,
        &sans,
    )
}

/// Start any assigned component that is not yet active; when all are active, mark the cluster Ready.
/// `join_tokens` caches each worker's minted join token across ticks so we mint once per join.
/// Returns the number of errors encountered this iteration (currently join-token mint failures),
/// so the reconcile loop can surface them via `spur_k8s_reconcile_errors_total`.
async fn converge_provisioning(
    cluster: &ClusterManager,
    net: &ClusterNetworking,
    join_tokens: &mut HashMap<String, String>,
) -> u64 {
    let mut errors = 0u64;
    let assigned: Vec<_> = cluster
        .get_nodes()
        .into_iter()
        .filter(|n| n.k0s_role.is_some())
        .collect();
    if assigned.is_empty() {
        join_tokens.clear();
        return errors;
    }
    let bootstrap = cluster.k0s_state().bootstrap();
    let mut all_active = true;
    let mut bootstrap_active = false;
    // Bootstrap control-plane first: it seeds etcd (tokenless) and its k0s API must answer before any
    // secondary control-plane or worker can mint a join token. A Single node is always the bootstrap.
    for node in &assigned {
        let role = node.k0s_role.expect("assigned above");
        if role == K0sRole::Worker {
            continue;
        }
        let is_bootstrap = role == K0sRole::Single || bootstrap.as_deref() == Some(&node.name);
        if !is_bootstrap {
            continue;
        }
        if fetch_component_state(cluster, &node.name).await.as_deref() == Some("active") {
            bootstrap_active = true;
            clear_node_error(cluster, node);
            continue;
        }
        all_active = false;
        // Mesh-native cluster: generate the k0s config (api on the mesh IP + Calico bird) when
        // cni=calico; None keeps the default kube-router. The bootstrap seeds etcd — no join token.
        let k0s_config = controller_k0s_config(net, node);
        spawn_start_component(cluster, &node.name, role, None, k0s_config, None);
    }
    // Don't touch secondary CPs / workers until the bootstrap's etcd is seeded and its API answers:
    // a controller token minted before then would race the quorum. Retry on the next tick.
    if !bootstrap_active {
        return errors;
    }
    // Secondary CPs join the etcd quorum with a `controller` token, then workers with a `worker`
    // token; both mint from a healthy CP agent, and a minting error just retries next tick.
    for node in &assigned {
        let role = node.k0s_role.expect("assigned above");
        let is_bootstrap = role == K0sRole::Single || bootstrap.as_deref() == Some(&node.name);
        if is_bootstrap {
            continue; // handled above
        }
        if fetch_component_state(cluster, &node.name).await.as_deref() == Some("active") {
            join_tokens.remove(&node.name); // joined — drop the cached token
            clear_node_error(cluster, node);
            continue;
        }
        all_active = false;
        let token_role = if role == K0sRole::Controller {
            "controller"
        } else {
            "worker"
        };
        // For a native-routing CNI, pin the node's kubelet node-ip to its mesh IP.
        let node_ip = if net.cni == "calico" {
            node.k0s_mesh_ip.clone()
        } else {
            None
        };
        // A secondary control-plane also needs its own generated k0s config (API SANs on its mesh IP).
        let k0s_config = if role == K0sRole::Controller {
            controller_k0s_config(net, node)
        } else {
            None
        };
        // Mint the join token once and cache it: re-minting every tick churns k0s server tokens and
        // races the join. Reuse the cached token on later ticks until the node joins.
        let token = match join_tokens.get(&node.name) {
            Some(cached) => cached.clone(),
            None => match mint_join_token(cluster, token_role).await {
                Ok(token) => {
                    join_tokens.insert(node.name.clone(), token.clone());
                    token
                }
                Err(e) => {
                    warn!(node = %node.name, error = %e, "could not mint {token_role} join token yet; will retry");
                    errors += 1;
                    continue;
                }
            },
        };
        spawn_start_component(cluster, &node.name, role, Some(token), k0s_config, node_ip);
    }
    // Only transition on the edge — this reconcile also runs every tick while already Ready (to
    // heal re-added nodes), so an unconditional set would churn a WAL write + log line each tick.
    if all_active && cluster.k0s_state().phase != K0sPhase::Ready {
        match cluster.set_k0s_phase(K0sPhase::Ready, None, Vec::new(), Vec::new(), false) {
            Ok(()) => info!("k0s cluster converged: all components active -> Ready"),
            Err(e) => warn!(error = %e, "failed to mark k0s cluster Ready"),
        }
    }
    errors
}

/// Drop a node's stale degrade reason once it is healthy again, so status reports honestly on retry.
/// Guarded so a converged cluster does not churn a WAL write per tick.
fn clear_node_error(cluster: &ClusterManager, node: &spur_core::node::Node) {
    if node.k0s_last_error.is_none() {
        return;
    }
    if let Err(e) = cluster.set_node_k0s_error(&node.name, None) {
        warn!(node = %node.name, error = %e, "failed to clear k0s node error");
    }
}

/// Provisioning blew its deadline: record why each non-active node blocked convergence, stop its
/// half-started unit (non-reset, keeping the role so `spur k8s up` can retry), then mark `degraded`.
async fn degrade_stuck_cluster(
    cluster: &ClusterManager,
    net: &ClusterNetworking,
    join_tokens: &mut HashMap<String, String>,
) {
    let timeout_secs = net.provisioning_timeout.as_secs();
    for node in cluster.get_nodes() {
        if node.k0s_role.is_none() {
            continue;
        }
        let state = fetch_component_state(cluster, &node.name).await;
        if state.as_deref() == Some("active") {
            clear_node_error(cluster, &node);
            continue;
        }
        let observed = state.as_deref().unwrap_or("unreachable");
        let reason = format!("not active after {timeout_secs}s (component {observed})");
        if let Err(e) = cluster.set_node_k0s_error(&node.name, Some(reason)) {
            warn!(node = %node.name, error = %e, "failed to record k0s node error");
        }
        spawn_stop_component(cluster, &node.name, false);
    }
    join_tokens.clear();
    match cluster.set_k0s_phase(K0sPhase::Degraded, None, Vec::new(), Vec::new(), false) {
        Ok(()) => warn!(
            timeout_secs,
            "k0s provisioning timed out -> Degraded; see `spur k8s status` for per-node reasons"
        ),
        Err(e) => warn!(error = %e, "failed to mark k0s cluster Degraded"),
    }
}

/// Cluster teardown: keep stopping a node's component while k0s still runs, else
/// (stopped/failed/unreachable) clear its role so it is never stranded out of scheduling.
async fn stop_all_components(cluster: &ClusterManager, reset: bool) {
    for node in cluster.get_nodes() {
        if node.k0s_role.is_none() {
            continue;
        }
        let state = fetch_component_state(cluster, &node.name).await;
        let still_running = matches!(
            state.as_deref(),
            Some("active") | Some("activating") | Some("deactivating")
        );
        if still_running {
            spawn_stop_component(cluster, &node.name, reset);
            continue;
        }
        if let Err(e) = cluster.clear_node_k0s(&node.name) {
            warn!(node = %node.name, error = %e, "failed to clear k0s role after teardown");
        }
    }
}

/// Fire-and-forget StartClusterComponent to a node's agent (off the reconcile thread).
fn spawn_start_component(
    cluster: &ClusterManager,
    node: &str,
    role: K0sRole,
    join_token: Option<String>,
    k0s_config: Option<String>,
    node_ip: Option<String>,
) {
    let Some(endpoint) = agent_endpoint(cluster, node) else {
        warn!(node = %node, "no agent address; cannot start k0s component");
        return;
    };
    let node = node.to_string();
    let role = role_str(role);
    tokio::spawn(async move {
        match crate::agent_client::connect(endpoint).await {
            Ok(mut client) => {
                let req = StartClusterComponentRequest {
                    role,
                    join_token,
                    k0s_config,
                    node_ip,
                };
                if let Err(e) = client.start_cluster_component(req).await {
                    warn!(node = %node, error = %e, "start_cluster_component failed");
                }
            }
            Err(e) => warn!(node = %node, error = %e, "connect to agent failed"),
        }
    });
}

/// Fire-and-forget StopClusterComponent to a node's agent.
fn spawn_stop_component(cluster: &ClusterManager, node: &str, reset: bool) {
    let Some(endpoint) = agent_endpoint(cluster, node) else {
        return;
    };
    let node = node.to_string();
    tokio::spawn(async move {
        match crate::agent_client::connect(endpoint).await {
            Ok(mut client) => {
                match client
                    .stop_cluster_component(StopClusterComponentRequest { reset })
                    .await
                {
                    Ok(resp) => {
                        // The agent reports a failed stop/reset in-band (stopped=false): surface it
                        // so `down --reset` isn't a false success. The component stays active, so the
                        // reconcile loop retries and `spur k8s status` still shows the node.
                        let r = resp.into_inner();
                        if !r.stopped {
                            warn!(
                                node = %node,
                                detail = %r.message,
                                "k0s component stop/reset failed; teardown is partial — retrying"
                            );
                        }
                    }
                    Err(e) => warn!(node = %node, error = %e, "stop_cluster_component failed"),
                }
            }
            Err(e) => warn!(node = %node, error = %e, "connect to agent failed"),
        }
    });
}

/// Convert the spur-net mesh membership to its proto mirror for the wire.
fn to_proto_membership(mesh: &MeshMembership) -> spur_proto::proto::MeshMembership {
    spur_proto::proto::MeshMembership {
        nodes: mesh
            .nodes
            .iter()
            .map(|n| spur_proto::proto::MeshNode {
                hostname: n.hostname.clone(),
                public_key: n.public_key.clone(),
                mesh_ip: n.mesh_ip.clone(),
                endpoint: n.endpoint.clone(),
                pod_cidr: n.pod_cidr.clone(),
            })
            .collect(),
    }
}

/// Fire-and-forget ApplyMesh to a node's agent: the agent reconciles the full mesh locally
/// (prune departed peers + add/update the desired set via `wg set`). Idempotent, so the
/// level-triggered per-tick re-push is safe.
fn spawn_apply_mesh(cluster: &ClusterManager, node: &str, mesh: &MeshMembership) {
    let Some(endpoint) = agent_endpoint(cluster, node) else {
        warn!(node = %node, "no agent address; cannot push mesh");
        return;
    };
    let node = node.to_string();
    let proto = to_proto_membership(mesh);
    tokio::spawn(async move {
        // Bound connect + RPC so a hung/blackholed agent can't leak accumulating detached tasks
        // (this fires every reconcile tick).
        let fut = async {
            let mut client = crate::agent_client::connect(endpoint)
                .await
                .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
            client.apply_mesh(proto).await
        };
        match tokio::time::timeout(AGENT_TIMEOUT, fut).await {
            Ok(Ok(resp)) => {
                let r = resp.into_inner();
                if !r.applied {
                    warn!(node = %node, message = %r.message, "apply_mesh not applied");
                }
            }
            Ok(Err(e)) => warn!(node = %node, error = %e, "apply_mesh RPC failed"),
            Err(_) => warn!(node = %node, "apply_mesh timed out"),
        }
    });
}

/// Per-node status with LIVE component_state fetched from each agent (for the ClusterStatus RPC).
pub async fn live_node_statuses(cluster: &ClusterManager) -> Vec<ClusterNodeStatus> {
    let mut out = Vec::new();
    for n in cluster.get_nodes() {
        let Some(role) = n.k0s_role else { continue };
        // Report the agent's real (state, enabled) — not a hard-coded enabled=true.
        let (component_state, enabled) = fetch_component_status(cluster, &n.name)
            .await
            .unwrap_or_else(|| ("unknown".to_string(), false));
        out.push(ClusterNodeStatus {
            node: n.name,
            role: role_str(role),
            component_state,
            enabled,
            reason: n.k0s_last_error.unwrap_or_default(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_clock_arms_then_trips_at_deadline() {
        let start = Instant::now();
        let timeout = Duration::from_secs(600);
        let mut since = None;
        // First Provisioning observation arms the clock; not yet timed out.
        assert!(!update_provisioning_clock(
            K0sPhase::Provisioning,
            &mut since,
            start,
            timeout
        ));
        assert!(since.is_some());
        // Before the deadline: still false. At/after: true.
        assert!(!update_provisioning_clock(
            K0sPhase::Provisioning,
            &mut since,
            start + Duration::from_secs(599),
            timeout
        ));
        assert!(update_provisioning_clock(
            K0sPhase::Provisioning,
            &mut since,
            start + timeout,
            timeout
        ));
    }

    #[test]
    fn provisioning_clock_resets_off_provisioning() {
        let start = Instant::now();
        let timeout = Duration::from_secs(600);
        let mut since = Some(start);
        // A non-Provisioning phase disarms the clock and never reports timed out.
        assert!(!update_provisioning_clock(
            K0sPhase::Ready,
            &mut since,
            start + timeout,
            timeout
        ));
        assert!(since.is_none());
        // Re-entering Provisioning re-arms from the new now, not the old start.
        assert!(!update_provisioning_clock(
            K0sPhase::Provisioning,
            &mut since,
            start + timeout,
            timeout
        ));
        assert_eq!(since, Some(start + timeout));
    }

    #[test]
    fn carve_pod_cidr_from_16() {
        assert_eq!(carve_pod_cidr("10.42.0.0/16", 0).unwrap(), "10.42.0.0/24");
        assert_eq!(carve_pod_cidr("10.42.0.0/16", 2).unwrap(), "10.42.2.0/24");
        assert_eq!(
            carve_pod_cidr("10.42.0.0/16", 255).unwrap(),
            "10.42.255.0/24"
        );
        // /16 has exactly 256 /24s -> ordinal 256 overflows.
        assert!(carve_pod_cidr("10.42.0.0/16", 256).is_err());
        // /25 is too small to carve a /24.
        assert!(carve_pod_cidr("10.42.0.0/25", 0).is_err());
    }

    #[test]
    fn pod_ordinal_inverts_carve() {
        let base: Ipv4Addr = "10.42.0.0".parse().unwrap();
        assert_eq!(pod_ordinal("10.42.0.0/24", base), Some(0));
        assert_eq!(pod_ordinal("10.42.7.0/24", base), Some(7));
        // below the pod base -> None (not one of ours)
        assert_eq!(pod_ordinal("10.41.0.0/24", base), None);
    }

    #[test]
    fn next_free_ordinal_skips_used() {
        let used: HashSet<u32> = [0, 1, 3].into_iter().collect();
        assert_eq!(next_free_ordinal(&used), 2);
        assert_eq!(next_free_ordinal(&HashSet::new()), 0);
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_cp_set_replicas_picks_lowest_bootstrap_first() {
        let set =
            resolve_control_plane_set(names(&["d", "b", "a", "c", "e"]), &[], None, 3).unwrap();
        // lowest 3 names, sorted; no pin so bootstrap-first is a no-op.
        assert_eq!(set, names(&["a", "b", "c"]));
    }

    #[test]
    fn resolve_cp_set_pins_bootstrap_to_front_and_into_the_set() {
        let set =
            resolve_control_plane_set(names(&["a", "b", "c", "d"]), &[], Some("c"), 3).unwrap();
        assert_eq!(set[0], "c", "pinned bootstrap leads (holds .1)");
        assert_eq!(set.len(), 3);
        assert!(set.contains(&"a".to_string()) && set.contains(&"b".to_string()));
    }

    #[test]
    fn resolve_cp_set_explicit_list_wins_and_orders_bootstrap() {
        let set = resolve_control_plane_set(
            names(&["a", "b", "c", "d", "e"]),
            &names(&["e", "c", "a"]),
            Some("c"),
            5, // ignored when explicit list is present
        )
        .unwrap();
        assert_eq!(set[0], "c");
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn resolve_cp_set_rejects_even_count() {
        assert!(resolve_control_plane_set(names(&["a", "b"]), &[], None, 2).is_err());
        assert!(
            resolve_control_plane_set(names(&["a", "b", "c", "d"]), &names(&["a", "b"]), None, 1)
                .is_err(),
            "explicit even list rejected"
        );
    }

    #[test]
    fn resolve_cp_set_rejects_more_cps_than_nodes() {
        let err = resolve_control_plane_set(names(&["a", "b"]), &[], None, 3).unwrap_err();
        assert!(err.contains("only 2 node"), "got: {err}");
    }

    #[test]
    fn resolve_cp_set_rejects_unknown_or_duplicate_explicit_node() {
        assert!(
            resolve_control_plane_set(names(&["a", "b", "c"]), &names(&["a", "x", "c"]), None, 3)
                .is_err(),
            "unknown node rejected"
        );
        assert!(
            resolve_control_plane_set(names(&["a", "b"]), &names(&["a", "a", "b"]), None, 3)
                .is_err(),
            "duplicate node rejected"
        );
    }

    #[test]
    fn resolve_cp_set_rejects_pinned_bootstrap_outside_explicit_list() {
        // A recorded/overridden bootstrap not in the requested set would silently move `.1`.
        let err = resolve_control_plane_set(
            names(&["a", "b", "c", "d"]),
            &names(&["a", "b", "c"]),
            Some("d"),
            3,
        )
        .unwrap_err();
        assert!(err.contains("bootstrap control-plane d"), "got: {err}");
        // In-list pinned bootstrap is fine and leads the set.
        let set = resolve_control_plane_set(
            names(&["a", "b", "c"]),
            &names(&["a", "b", "c"]),
            Some("b"),
            3,
        )
        .unwrap();
        assert_eq!(set[0], "b");
    }

    #[test]
    fn resolve_cp_set_legacy_single_cp_reup_is_idempotent() {
        // A 1-CP cluster re-upped with replicas=1 resolves to the same single-node set (guard allows).
        let set = resolve_control_plane_set(names(&["a", "b"]), &[], Some("a"), 1).unwrap();
        assert_eq!(set, names(&["a"]));
    }

    #[test]
    fn resolve_cp_set_rejects_singular_pin_outside_candidates() {
        // With candidates narrowed to the member scope, a --control-plane-node outside it must error
        // rather than be silently dropped and a different in-scope node elected.
        let err = resolve_control_plane_set(names(&["a", "b"]), &[], Some("z"), 1).unwrap_err();
        assert!(
            err.contains("control-plane node z is not among the selected"),
            "got: {err}"
        );
    }

    #[test]
    fn first_host_is_dot_one() {
        assert_eq!(
            first_host("10.44.0.0/16").unwrap(),
            "10.44.0.1".parse::<Ipv4Addr>().unwrap()
        );
    }

    fn scope_node(name: &str, parts: &[&str], labels: &[(&str, &str)]) -> spur_core::node::Node {
        let mut n = spur_core::node::Node::new(name.to_string(), Default::default());
        n.partitions = parts.iter().map(|s| s.to_string()).collect();
        n.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        n
    }

    fn sel(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolve_members_empty_selection_is_whole_inventory() {
        let nodes = vec![scope_node("a", &[], &[]), scope_node("b", &[], &[])];
        assert_eq!(
            resolve_member_nodes(&nodes, "", "", &HashMap::new()).unwrap(),
            Vec::<String>::new(),
            "no selection = empty = whole inventory"
        );
    }

    #[test]
    fn resolve_members_hostlist_expands_and_sorts() {
        let nodes = vec![
            scope_node("gpu01", &[], &[]),
            scope_node("gpu02", &[], &[]),
            scope_node("gpu03", &[], &[]),
        ];
        let out = resolve_member_nodes(&nodes, "gpu[01-02]", "", &HashMap::new()).unwrap();
        assert_eq!(out, names(&["gpu01", "gpu02"]));
    }

    #[test]
    fn resolve_members_hostlist_rejects_unregistered() {
        let nodes = vec![scope_node("a", &[], &[])];
        let err = resolve_member_nodes(&nodes, "a,ghost", "", &HashMap::new()).unwrap_err();
        assert!(err.contains("ghost is not a registered node"), "got: {err}");
    }

    #[test]
    fn resolve_members_partition_selects_members() {
        let nodes = vec![
            scope_node("a", &["gpu"], &[]),
            scope_node("b", &["cpu"], &[]),
            scope_node("c", &["gpu"], &[]),
        ];
        let out = resolve_member_nodes(&nodes, "", "gpu", &HashMap::new()).unwrap();
        assert_eq!(out, names(&["a", "c"]));
    }

    #[test]
    fn resolve_members_empty_partition_rejected() {
        let nodes = vec![scope_node("a", &["gpu"], &[])];
        let err = resolve_member_nodes(&nodes, "", "nope", &HashMap::new()).unwrap_err();
        assert!(err.contains("partition nope has no"), "got: {err}");
    }

    #[test]
    fn resolve_members_selector_matches_all_pairs() {
        let nodes = vec![
            scope_node("a", &[], &[("zone", "z1"), ("gpu", "mi300")]),
            scope_node("b", &[], &[("zone", "z1"), ("gpu", "mi200")]),
            scope_node("c", &[], &[("zone", "z2"), ("gpu", "mi300")]),
        ];
        let out = resolve_member_nodes(&nodes, "", "", &sel(&[("zone", "z1"), ("gpu", "mi300")]))
            .unwrap();
        assert_eq!(out, names(&["a"]), "only the node matching BOTH pairs");
    }

    #[test]
    fn resolve_members_union_dedups_across_surfaces() {
        let nodes = vec![
            scope_node("a", &["gpu"], &[("fast", "1")]),
            scope_node("b", &["gpu"], &[]),
            scope_node("c", &[], &[("fast", "1")]),
            scope_node("d", &[], &[]),
        ];
        // hostlist {a} ∪ partition gpu {a,b} ∪ selector fast=1 {a,c} = {a,b,c}, a not duplicated.
        let out = resolve_member_nodes(&nodes, "a", "gpu", &sel(&[("fast", "1")])).unwrap();
        assert_eq!(out, names(&["a", "b", "c"]));
    }

    #[test]
    fn resolve_members_selector_no_match_rejected() {
        let nodes = vec![scope_node("a", &[], &[("zone", "z1")])];
        let err = resolve_member_nodes(&nodes, "", "", &sel(&[("zone", "z9")])).unwrap_err();
        assert!(err.contains("matched no registered nodes"), "got: {err}");
    }

    #[test]
    fn resolve_members_bogus_selector_rejected_even_when_other_surface_matches() {
        // A supplied selector that matches nothing must error even if --nodes/--partition matched,
        // so a typo'd selector isn't silently ignored.
        let nodes = vec![scope_node("a", &["gpu"], &[("zone", "z1")])];
        let err = resolve_member_nodes(&nodes, "a", "", &sel(&[("zone", "z9")])).unwrap_err();
        assert!(err.contains("--selector matched no"), "got: {err}");
    }

    fn mesh_node(
        name: &str,
        mesh_ip: Option<&str>,
        pubkey: Option<&str>,
        addr: Option<&str>,
        pod: Option<&str>,
    ) -> spur_core::node::Node {
        let mut n = spur_core::node::Node::new(name.to_string(), Default::default());
        n.k0s_mesh_ip = mesh_ip.map(String::from);
        n.wg_pubkey = pubkey.map(String::from);
        n.address = addr.map(String::from);
        n.k0s_pod_cidr = pod.map(String::from);
        n
    }

    #[test]
    fn mesh_membership_skips_unmeshed_and_carries_pod_cidr() {
        let nodes = vec![
            // controller: meshed, pod CIDR set
            mesh_node(
                "cp",
                Some("10.44.0.1"),
                Some("pk-cp"),
                Some("198.51.100.1"),
                Some("10.42.0.0/24"),
            ),
            // worker: meshed
            mesh_node(
                "w2",
                Some("10.44.0.2"),
                Some("pk-w2"),
                Some("198.51.100.2"),
                Some("10.42.1.0/24"),
            ),
            // assigned a mesh IP but hasn't reported a pubkey yet -> not on the mesh, skip
            mesh_node("w3", Some("10.44.0.3"), None, Some("198.51.100.3"), None),
            // empty pubkey is treated as absent -> skip
            mesh_node(
                "w4",
                Some("10.44.0.4"),
                Some(""),
                Some("198.51.100.4"),
                None,
            ),
            // no mesh IP (not assigned) -> skip
            mesh_node("w5", None, Some("pk-w5"), Some("198.51.100.5"), None),
        ];
        let m = mesh_from_nodes(nodes);
        assert_eq!(m.nodes.len(), 2, "only fully-meshed nodes included");
        // sorted by mesh_ip
        assert_eq!(m.nodes[0].mesh_ip, "10.44.0.1");
        assert_eq!(m.nodes[0].public_key, "pk-cp");
        // endpoint is left empty on purpose — apply_mesh preserves the tunnel `spur net join` set.
        assert_eq!(m.nodes[0].endpoint, "");
        assert_eq!(m.nodes[0].pod_cidr.as_deref(), Some("10.42.0.0/24"));
        assert_eq!(m.nodes[1].mesh_ip, "10.44.0.2");
        // the resulting membership feeds apply_mesh: pod CIDR folds into AllowedIPs
        assert_eq!(
            spur_net::mesh::peer_allowed_ips(&m.nodes[1]),
            "10.44.0.2/32,10.42.1.0/24"
        );
    }
}
