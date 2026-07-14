// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared types for native k0s cluster integration (SPUR owns the k0s lifecycle).
//!
//! Named here so both the WAL (`spur_core::wal`) and node inventory (`spur_core::node`) can
//! reference them, and the spurctld raft state machine can persist them across failover.

use serde::{Deserialize, Serialize};

/// k0s release SPUR installs/runs by default — pinned to a known-good version and bumped with each
/// spur release. Override per node via `[cluster] k0s_version` (accepts a tag or "latest").
pub const K0S_PINNED_VERSION: &str = "v1.36.2+k0s.0";

/// Default filesystem path for the k0s binary (install target + what the spurd-owned unit runs).
pub const K0S_DEFAULT_BINARY: &str = "/usr/local/bin/k0s";

/// The GitHub repo k0s releases come from.
pub const K0S_REPO: &str = "k0sproject/k0s";

/// Generate a k0s controller config (YAML) for a mesh-native cluster: the API server is advertised
/// on `api_address` (the control-plane's WireGuard mesh IP) and Calico runs in `bird` mode (native
/// routing, no overlay) so pod traffic rides the mesh. `cni_mtu` sets Calico's MTU (typically below
/// the underlay to leave room for WireGuard's ~50-byte overhead, avoiding fragmentation). Returns
/// `None` for any `cni` other than `"calico"` (the k0s default, kube-router, needs no config file).
/// `sans` are extra API-server certificate SANs (e.g. the control-plane's mesh + underlay IPs).
pub fn k0s_controller_config_yaml(
    cni: &str,
    pod_cidr: &str,
    service_cidr: &str,
    cni_mtu: u16,
    api_address: &str,
    sans: &[String],
) -> Option<String> {
    if cni != "calico" {
        return None;
    }
    let mut y = String::new();
    y.push_str("apiVersion: k0s.k0sproject.io/v1beta1\n");
    y.push_str("kind: ClusterConfig\n");
    y.push_str("metadata:\n");
    y.push_str("  name: k0s\n");
    y.push_str("spec:\n");
    y.push_str("  api:\n");
    y.push_str(&format!("    address: {api_address}\n"));
    if !sans.is_empty() {
        y.push_str("    sans:\n");
        for san in sans {
            y.push_str(&format!("      - {san}\n"));
        }
    }
    y.push_str("  network:\n");
    y.push_str("    provider: calico\n");
    y.push_str(&format!("    podCIDR: {pod_cidr}\n"));
    y.push_str(&format!("    serviceCIDR: {service_cidr}\n"));
    y.push_str("    calico:\n");
    y.push_str("      mode: bird\n");
    y.push_str(&format!("      mtu: {cni_mtu}\n"));
    Some(y)
}

#[cfg(test)]
mod k0s_config_tests {
    use super::k0s_controller_config_yaml;

    #[test]
    fn calico_config_has_mesh_api_and_bird() {
        // Documentation-range addresses only (RFC 5737 TEST-NET); no real infrastructure IPs.
        let y = k0s_controller_config_yaml(
            "calico",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            "192.0.2.1",
            &["192.0.2.1".to_string(), "203.0.113.9".to_string()],
        )
        .unwrap();
        assert!(y.contains("address: 192.0.2.1"));
        assert!(y.contains("      - 203.0.113.9"));
        assert!(y.contains("provider: calico"));
        assert!(y.contains("mode: bird"));
        assert!(y.contains("mtu: 1450"));
        assert!(y.contains("podCIDR: 192.0.2.0/24"));
        assert!(y.contains("serviceCIDR: 198.51.100.0/24"));
    }

    #[test]
    fn kuberouter_default_generates_no_config() {
        assert!(k0s_controller_config_yaml(
            "kuberouter",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            "192.0.2.1",
            &[]
        )
        .is_none());
    }
}

/// Which k0s role a node's spurd-owned systemd unit runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum K0sRole {
    Controller,
    Worker,
    Single,
}

/// Lifecycle phase of the SPUR-managed k0s cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum K0sPhase {
    #[default]
    Down,
    Provisioning,
    Ready,
    Degraded,
}

/// Cluster-wide k0s state held in the replicated raft state machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct K0sClusterState {
    #[serde(default)]
    pub phase: K0sPhase,
    #[serde(default)]
    pub control_plane_node: Option<String>,
    #[serde(default)]
    pub reset_requested: bool,
}
