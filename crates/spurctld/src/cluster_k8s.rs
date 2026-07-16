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
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};

use spur_core::k0s::{K0sClusterState, K0sPhase, K0sRole};
use spur_net::address::AddressPool;
use spur_net::mesh::{MeshMembership, MeshNode};
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::{
    ClusterNodeStatus, CreateK0sJoinTokenRequest, GetAdminKubeconfigRequest,
    GetClusterComponentStatusRequest, StartClusterComponentRequest, StopClusterComponentRequest,
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
}

/// Leader-gated k0s reconcile loop. Spawned from `main.rs` when `[cluster].enabled`; it still
/// re-checks leadership every tick because leadership can flip at any time.
pub async fn run(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>, net: ClusterNetworking) {
    info!(mesh = %net.mesh_cidr, pod = %net.pod_cidr, "k0s cluster reconcile loop started");
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    let mut last_mesh: Vec<MeshNode> = Vec::new();
    // Cache of the worker join token minted per node, so we mint once (not every tick) while a
    // worker joins — re-minting each tick churns k0s server tokens and races the join. Cleared when
    // the worker's component reports active.
    let mut worker_tokens: HashMap<String, String> = HashMap::new();
    loop {
        interval.tick().await;
        if !raft.is_leader() {
            last_mesh.clear(); // forget on leadership loss so a new term re-logs the membership
            continue; // only the leader reconciles
        }
        let state = cluster.k0s_state();

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

        reconcile_phase(&cluster, &net, &state, &mut worker_tokens).await;
    }
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
    worker_tokens: &mut HashMap<String, String>,
) {
    match state.phase {
        K0sPhase::Ready | K0sPhase::Provisioning => {
            if let Err(e) = provision_assignments(cluster, net, state) {
                warn!(error = %e, "k0s provisioning assignment failed; will retry next tick");
                return;
            }
            converge_provisioning(cluster, net, worker_tokens).await;
        }
        K0sPhase::Down => stop_all_components(cluster, state.reset_requested).await,
        K0sPhase::Degraded => warn!("k0s cluster degraded"),
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
    if nodes.is_empty() {
        return Ok(());
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name)); // deterministic

    // Control-plane node. Derive from an ALREADY-ASSIGNED Controller/Single node FIRST: the persisted
    // role assignment is the durable source of truth. The CP-choice persist below is a separate raft
    // write that lands *after* the assignment loop, so a crash between the first `assign(CP, .1)` and
    // that persist would otherwise let a restart (with `state.control_plane_node` still None) pick a
    // different, lexically-earlier node as CP and hand it the same `.1`/Controller — two controllers
    // silently sharing the mesh IP across the failover. Fall back to the recorded choice -> config
    // override -> lexically-first only when nothing is assigned yet.
    let cp_node = nodes
        .iter()
        .find(|n| matches!(n.k0s_role, Some(K0sRole::Controller) | Some(K0sRole::Single)))
        .map(|n| n.name.clone())
        .or_else(|| state.control_plane_node.clone())
        .or_else(|| net.control_plane_node.clone())
        .unwrap_or_else(|| nodes[0].name.clone());

    // Re-seed the mesh pool from persisted assignments + reserve .1 for the controller.
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
    for node in &nodes {
        if node.k0s_role.is_some() {
            continue; // already assigned
        }
        let is_cp = node.name == cp_node;
        let role = if is_cp {
            if single {
                K0sRole::Single
            } else {
                K0sRole::Controller
            }
        } else {
            K0sRole::Worker
        };
        let mesh_ip = if is_cp {
            controller_ip
        } else {
            pool.allocate()?
        };
        let ordinal = next_free_ordinal(&used_ordinals);
        used_ordinals.insert(ordinal);
        let pod_cidr = carve_pod_cidr(&net.pod_cidr, ordinal)?;
        cluster.assign_node_k0s(&node.name, role, &mesh_ip.to_string(), &pod_cidr)?;
    }

    // Persist the control-plane choice if not already recorded.
    if state.control_plane_node.as_deref() != Some(cp_node.as_str()) {
        cluster.set_k0s_phase(K0sPhase::Provisioning, Some(cp_node), false)?;
    }
    Ok(())
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

/// Dial the control-plane node's agent, timing out per `AGENT_TIMEOUT`. Errors if no control-plane
/// node is assigned yet or it has no reachable address.
async fn connect_control_plane(
    cluster: &ClusterManager,
) -> anyhow::Result<SlurmAgentClient<tonic::transport::Channel>> {
    let cp = cluster
        .k0s_state()
        .control_plane_node
        .ok_or_else(|| anyhow::anyhow!("no control-plane node assigned yet"))?;
    let endpoint = agent_endpoint(cluster, &cp)
        .ok_or_else(|| anyhow::anyhow!("control-plane node {cp} has no agent address"))?;
    tokio::time::timeout(AGENT_TIMEOUT, SlurmAgentClient::connect(endpoint))
        .await
        .map_err(|_| anyhow::anyhow!("connect to control-plane agent timed out"))?
        .map_err(|e| anyhow::anyhow!("connect to control-plane agent failed: {e}"))
}

/// Mint a worker join token from the control-plane node's agent (`k0s token create --role worker`).
/// Errors until the control-plane component is up (its k0s API must answer); the caller retries.
async fn mint_worker_token(cluster: &ClusterManager) -> anyhow::Result<String> {
    let mut client = connect_control_plane(cluster).await?;
    let resp = client
        .create_k0s_join_token(CreateK0sJoinTokenRequest {
            role: "worker".to_string(),
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
        .get_admin_kubeconfig(GetAdminKubeconfigRequest {})
        .await
        .map_err(|e| anyhow::anyhow!("get_admin_kubeconfig RPC failed: {e}"))?;
    Ok(resp.into_inner().kubeconfig)
}

/// Query a node's live k0s component state via its agent, with a timeout. Returns None if the node
/// is unreachable or has no component yet.
async fn fetch_component_status(cluster: &ClusterManager, node: &str) -> Option<(String, bool)> {
    let endpoint = agent_endpoint(cluster, node)?;
    let fut = async {
        let mut client = SlurmAgentClient::connect(endpoint).await.ok()?;
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
/// `worker_tokens` caches each worker's minted join token across ticks so we mint once per join.
async fn converge_provisioning(
    cluster: &ClusterManager,
    net: &ClusterNetworking,
    worker_tokens: &mut HashMap<String, String>,
) {
    let assigned: Vec<_> = cluster
        .get_nodes()
        .into_iter()
        .filter(|n| n.k0s_role.is_some())
        .collect();
    if assigned.is_empty() {
        worker_tokens.clear();
        return;
    }
    let mut all_active = true;
    // Control-plane (controller/single) first: a worker needs the control-plane's k0s API up to
    // mint its join token, so the control-plane must be started before we try any worker.
    for node in &assigned {
        let role = node.k0s_role.expect("assigned above");
        if role == K0sRole::Worker {
            continue;
        }
        if fetch_component_state(cluster, &node.name).await.as_deref() == Some("active") {
            continue;
        }
        all_active = false;
        // Mesh-native cluster: generate the k0s config (api on the mesh IP + Calico bird) when
        // cni=calico; None keeps the default kube-router. Control-plane needs no join token.
        let k0s_config = controller_k0s_config(net, node);
        spawn_start_component(cluster, &node.name, role, None, k0s_config, None);
    }
    // Workers: mint a fresh join token from the control-plane agent, then start with it.
    // Minting errors until the control-plane's k0s API is reachable — treated as "retry next tick".
    for node in &assigned {
        if node.k0s_role != Some(K0sRole::Worker) {
            continue;
        }
        if fetch_component_state(cluster, &node.name).await.as_deref() == Some("active") {
            worker_tokens.remove(&node.name); // joined — drop the cached token
            continue;
        }
        all_active = false;
        // For a native-routing CNI, pin the worker's kubelet node-ip to its mesh IP.
        let node_ip = if net.cni == "calico" {
            node.k0s_mesh_ip.clone()
        } else {
            None
        };
        // Mint the worker's join token once and cache it: re-minting every tick churns k0s server
        // tokens and races the join. Reuse the cached token on later ticks until the worker joins.
        let token = match worker_tokens.get(&node.name) {
            Some(cached) => cached.clone(),
            None => match mint_worker_token(cluster).await {
                Ok(token) => {
                    worker_tokens.insert(node.name.clone(), token.clone());
                    token
                }
                Err(e) => {
                    warn!(node = %node.name, error = %e, "could not mint worker join token yet; will retry");
                    continue;
                }
            },
        };
        spawn_start_component(
            cluster,
            &node.name,
            K0sRole::Worker,
            Some(token),
            None,
            node_ip,
        );
    }
    // Only transition on the edge — this reconcile also runs every tick while already Ready (to
    // heal re-added nodes), so an unconditional set would churn a WAL write + log line each tick.
    if all_active && cluster.k0s_state().phase != K0sPhase::Ready {
        match cluster.set_k0s_phase(K0sPhase::Ready, None, false) {
            Ok(()) => info!("k0s cluster converged: all components active -> Ready"),
            Err(e) => warn!(error = %e, "failed to mark k0s cluster Ready"),
        }
    }
}

/// Stop every assigned component that is still running (cluster teardown).
async fn stop_all_components(cluster: &ClusterManager, reset: bool) {
    for node in cluster.get_nodes() {
        if node.k0s_role.is_none() {
            continue;
        }
        if fetch_component_state(cluster, &node.name).await.as_deref() == Some("inactive") {
            continue;
        }
        spawn_stop_component(cluster, &node.name, reset);
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
        match SlurmAgentClient::connect(endpoint).await {
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
        match SlurmAgentClient::connect(endpoint).await {
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
            let mut client = SlurmAgentClient::connect(endpoint)
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
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn first_host_is_dot_one() {
        assert_eq!(
            first_host("10.44.0.0/16").unwrap(),
            "10.44.0.1".parse::<Ipv4Addr>().unwrap()
        );
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
