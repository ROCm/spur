// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Full-mesh WireGuard peering with pod-CIDR routing.
//!
//! The default `spur net join` sets up a hub-and-spoke topology: each worker
//! peers only the controller (with `AllowedIPs` covering the whole mesh CIDR).
//! That is enough for control-plane traffic, but a CNI running in
//! native-routing mode (e.g. Calico `bird`/BGP, no VXLAN/IPIP overlay) on top
//! of the mesh needs two more things:
//!
//! 1. **Full mesh** — every node must peer every other node directly, so raw
//!    pod packets can flow worker↔worker without hairpinning through the hub.
//! 2. **Pod CIDRs in `AllowedIPs`** — WireGuard is a cryptokey router; it only
//!    forwards a packet whose destination is in some peer's `AllowedIPs`. A
//!    native-routed pod packet has a pod-IP destination, so each peer's
//!    `AllowedIPs` must include that node's pod CIDR (in addition to its `/32`
//!    mesh address), and a matching kernel route must exist over the interface.
//!
//! This module computes the per-node peer set (pure, tested) and applies it to
//! a running interface via `wg set`. `AllowedIPs` is all WireGuard needs to
//! forward pod traffic; the kernel FIB routes are owned by the CNI in
//! native-routing mode, so spur only programs them on request (`program_routes`,
//! for the no-CNI case). Applying is **additive** — it does not prune peers for
//! nodes removed from the membership.

use serde::{Deserialize, Serialize};

use crate::wireguard::{self, WgPeer};

/// A member of the WireGuard mesh, as known to the controller/operator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshNode {
    /// Node hostname (informational).
    #[serde(default)]
    pub hostname: String,
    /// WireGuard public key.
    pub public_key: String,
    /// Mesh address on `spur0`, e.g. `10.44.0.2` (no prefix).
    pub mesh_ip: String,
    /// Underlay endpoint for the WireGuard tunnel, e.g. `203.0.113.9:51820`.
    pub endpoint: String,
    /// Optional pod CIDR routed to this node for native-routing CNIs,
    /// e.g. `10.42.1.0/24`.
    #[serde(default)]
    pub pod_cidr: Option<String>,
}

/// A mesh membership document (what `spur net mesh --config` parses).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MeshMembership {
    #[serde(default)]
    pub nodes: Vec<MeshNode>,
}

/// The `AllowedIPs` string for a peer: its `/32` mesh address, plus its pod
/// CIDR when set (comma-separated, as `wg` accepts).
pub fn peer_allowed_ips(node: &MeshNode) -> String {
    match &node.pod_cidr {
        Some(pod) if !pod.trim().is_empty() => format!("{}/32,{}", node.mesh_ip, pod.trim()),
        _ => format!("{}/32", node.mesh_ip),
    }
}

/// Validate that `cidr` is an IPv4 CIDR like `10.42.1.0/24`, so bad user input
/// fails fast instead of reaching `wg`/`ip` as an opaque stderr.
pub fn validate_cidr(cidr: &str) -> anyhow::Result<()> {
    let (addr, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid CIDR (missing /prefix): {cidr}"))?;
    addr.parse::<std::net::Ipv4Addr>()
        .map_err(|_| anyhow::anyhow!("invalid CIDR address: {cidr}"))?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid CIDR prefix: {cidr}"))?;
    if prefix > 32 {
        anyhow::bail!("invalid CIDR prefix (>32): {cidr}");
    }
    Ok(())
}

/// Build the WireGuard peer set for `self_mesh_ip`'s view of a full mesh:
/// every member except itself, each with pod-CIDR-aware `AllowedIPs`.
pub fn mesh_peers_for(self_mesh_ip: &str, members: &[MeshNode]) -> Vec<WgPeer> {
    let self_mesh_ip = self_mesh_ip.trim();
    members
        .iter()
        .filter(|n| n.mesh_ip.trim() != self_mesh_ip)
        .map(|n| WgPeer {
            public_key: n.public_key.clone(),
            allowed_ips: peer_allowed_ips(n),
            endpoint: Some(n.endpoint.clone()),
            persistent_keepalive: Some(25),
        })
        .collect()
}

/// The pod-CIDR routes for `self_mesh_ip`'s node (one per remote member that
/// advertises a pod CIDR). Only relevant when spur programs routes directly
/// (no native-routing CNI — see `apply_mesh`'s `program_routes`).
pub fn mesh_pod_routes<'a>(self_mesh_ip: &str, members: &'a [MeshNode]) -> Vec<&'a str> {
    let self_mesh_ip = self_mesh_ip.trim();
    members
        .iter()
        .filter(|n| n.mesh_ip.trim() != self_mesh_ip)
        .filter_map(|n| n.pod_cidr.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

/// Apply the full mesh on the local node: add every remote peer (with
/// pod-CIDR-aware `AllowedIPs`). Returns the number of peers applied.
///
/// **Additive**, not a full reconcile: it never removes peers/routes for nodes
/// dropped from `members` (re-run with the complete membership to converge). It
/// is idempotent for a fixed membership (`wg set` is add-or-update).
///
/// `program_routes` also installs kernel routes (`<pod>/24 dev <iface>`). Leave
/// it **false** whenever a native-routing CNI (Calico `bird`) is present — the
/// CNI owns the FIB routes and spur programming them would fight its reconcile
/// loop. `AllowedIPs` (always set) is all WireGuard itself needs.
pub fn apply_mesh(
    interface: &str,
    self_mesh_ip: &str,
    members: &[MeshNode],
    program_routes: bool,
) -> anyhow::Result<usize> {
    let peers = mesh_peers_for(self_mesh_ip, members);
    for peer in &peers {
        wireguard::add_peer(interface, peer)?;
    }
    if program_routes {
        for cidr in mesh_pod_routes(self_mesh_ip, members) {
            wireguard::add_route(interface, cidr)?;
        }
    }
    Ok(peers.len())
}

/// Of the interface's `current` peer public keys, those NOT in the desired member set (self
/// excluded) — i.e. peers for nodes that have left the mesh and should be removed. Pure + tested.
pub fn peers_to_prune<'a>(
    current: &'a [String],
    self_mesh_ip: &str,
    members: &[MeshNode],
) -> Vec<&'a str> {
    let desired: std::collections::HashSet<String> = mesh_peers_for(self_mesh_ip, members)
        .into_iter()
        .map(|p| p.public_key)
        .collect();
    current
        .iter()
        .filter(|k| !desired.contains(k.as_str()))
        .map(String::as_str)
        .collect()
}

/// Reconcile the full mesh: prune peers no longer in `members`, then add/update the desired peers.
/// Unlike [`apply_mesh`] (additive only), this converges — a node dropped from `members` has its
/// WireGuard peer removed. `current_peers` is the interface's live peer keys (from
/// [`wireguard::list_peers`]). Returns `(added, pruned)`. `program_routes` as in [`apply_mesh`].
pub fn reconcile_mesh(
    interface: &str,
    self_mesh_ip: &str,
    members: &[MeshNode],
    current_peers: &[String],
    program_routes: bool,
) -> anyhow::Result<(usize, usize)> {
    let mut pruned = 0;
    for key in peers_to_prune(current_peers, self_mesh_ip, members) {
        wireguard::remove_peer(interface, key)?;
        pruned += 1;
    }
    let added = apply_mesh(interface, self_mesh_ip, members, program_routes)?;
    Ok((added, pruned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ip: &str, pod: Option<&str>) -> MeshNode {
        MeshNode {
            hostname: format!("n{ip}"),
            public_key: format!("pk-{ip}"),
            mesh_ip: ip.to_string(),
            endpoint: format!("{ip}:51820"),
            pod_cidr: pod.map(|s| s.to_string()),
        }
    }

    #[test]
    fn allowed_ips_with_and_without_pod_cidr() {
        assert_eq!(
            peer_allowed_ips(&node("10.44.0.2", Some("10.42.1.0/24"))),
            "10.44.0.2/32,10.42.1.0/24"
        );
        assert_eq!(peer_allowed_ips(&node("10.44.0.2", None)), "10.44.0.2/32");
        // whitespace-only pod cidr is treated as absent
        assert_eq!(
            peer_allowed_ips(&node("10.44.0.2", Some("  "))),
            "10.44.0.2/32"
        );
    }

    #[test]
    fn mesh_peers_exclude_self_and_carry_pod_cidr() {
        let members = vec![
            node("10.44.0.1", None),                 // controller, no pods
            node("10.44.0.2", Some("10.42.1.0/24")), // self
            node("10.44.0.3", Some("10.42.2.0/24")),
            node("10.44.0.4", Some("10.42.3.0/24")),
        ];
        let peers = mesh_peers_for("10.44.0.2", &members);
        assert_eq!(peers.len(), 3, "self excluded");
        assert!(peers.iter().all(|p| p.public_key != "pk-10.44.0.2"));
        let p3 = peers
            .iter()
            .find(|p| p.public_key == "pk-10.44.0.3")
            .unwrap();
        assert_eq!(p3.allowed_ips, "10.44.0.3/32,10.42.2.0/24");
        assert_eq!(p3.endpoint.as_deref(), Some("10.44.0.3:51820"));
        assert_eq!(p3.persistent_keepalive, Some(25));
        let p1 = peers
            .iter()
            .find(|p| p.public_key == "pk-10.44.0.1")
            .unwrap();
        assert_eq!(p1.allowed_ips, "10.44.0.1/32", "hub with no pod cidr");
    }

    #[test]
    fn pod_routes_skip_self_and_empty() {
        let members = vec![
            node("10.44.0.1", None),
            node("10.44.0.2", Some("10.42.1.0/24")), // self, must be skipped
            node("10.44.0.3", Some("10.42.2.0/24")),
            node("10.44.0.4", Some("")), // empty pod cidr skipped
        ];
        let routes = mesh_pod_routes("10.44.0.2", &members);
        assert_eq!(routes, vec!["10.42.2.0/24"]);
    }

    #[test]
    fn validate_cidr_accepts_and_rejects() {
        assert!(validate_cidr("10.42.1.0/24").is_ok());
        assert!(validate_cidr("10.44.0.2/32").is_ok());
        assert!(validate_cidr("10.42.1.0").is_err()); // missing /prefix
        assert!(validate_cidr("10.42.1.0/33").is_err()); // prefix > 32
        assert!(validate_cidr("not-an-ip/24").is_err());
        assert!(validate_cidr("10.42.1.0/xx").is_err());
    }

    #[test]
    fn mesh_peers_trims_self_ip() {
        let members = vec![
            node("10.44.0.2", Some("10.42.1.0/24")),
            node("10.44.0.3", None),
        ];
        // trailing space on self must still exclude the matching member
        let peers = mesh_peers_for("10.44.0.2 ", &members);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].public_key, "pk-10.44.0.3");
    }

    #[test]
    fn membership_parses_from_json() {
        let json = r#"{"nodes":[
            {"public_key":"a","mesh_ip":"10.44.0.2","endpoint":"1.2.3.4:51820","pod_cidr":"10.42.1.0/24"},
            {"public_key":"b","mesh_ip":"10.44.0.3","endpoint":"1.2.3.5:51820"}
        ]}"#;
        let m: MeshMembership = serde_json::from_str(json).unwrap();
        assert_eq!(m.nodes.len(), 2);
        assert_eq!(m.nodes[0].pod_cidr.as_deref(), Some("10.42.1.0/24"));
        assert_eq!(m.nodes[1].pod_cidr, None);
    }

    #[test]
    fn peers_to_prune_removes_departed_and_self_keeps_members() {
        let members = vec![
            node("10.44.0.1", None),                 // controller
            node("10.44.0.2", Some("10.42.1.0/24")), // self
            node("10.44.0.3", Some("10.42.2.0/24")),
        ];
        // Live peers on the interface: two current members, a departed node, and self's own key.
        let current = vec![
            "pk-10.44.0.1".to_string(),
            "pk-10.44.0.3".to_string(),
            "pk-10.44.0.4".to_string(), // departed -> prune
            "pk-10.44.0.2".to_string(), // self (never a desired peer) -> prune
        ];
        let prune = peers_to_prune(&current, "10.44.0.2", &members);
        assert_eq!(prune.len(), 2);
        assert!(prune.contains(&"pk-10.44.0.4"));
        assert!(prune.contains(&"pk-10.44.0.2"));
        assert!(!prune.contains(&"pk-10.44.0.1"));
        assert!(!prune.contains(&"pk-10.44.0.3"));
    }
}
