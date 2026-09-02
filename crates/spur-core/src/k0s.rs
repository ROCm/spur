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

/// k0s's manifest-deployer directory (under the default data-dir). Any manifest written to a
/// `<stack>/` subdirectory here is applied + reconciled by the k0s controller automatically, so SPUR
/// ships cluster addons (e.g. local-path storage) by writing files here — no in-cluster kube client.
pub const K0S_MANIFESTS_DIR: &str = "/var/lib/k0s/manifests";

/// Vendored local-path-provisioner release (see `assets/local-path-provisioner.yaml`).
pub const LOCAL_PATH_VERSION: &str = "v0.0.31";

/// Default node directory the local-path provisioner stores PersistentVolumes in. Override via
/// `[cluster] local_path_dir` — point it at a large scratch disk if PVCs will hold much data (the
/// default lives under `/var/lib`, i.e. the root filesystem).
pub const DEFAULT_LOCAL_PATH_DIR: &str = "/var/lib/local-path-provisioner";

/// Render the local-path-provisioner manifest with `data_dir` as the on-node PV storage path. The
/// result is a full k8s manifest (Namespace, RBAC, Deployment, default StorageClass, ConfigMap) that
/// SPUR writes into k0s's manifest-deployer dir on the control-plane node; k0s applies it.
///
/// `data_dir` is interpolated verbatim into a JSON string in the ConfigMap, so it must be free of
/// quotes/backslashes/whitespace — [`ClusterConfig::validate`](crate::config) enforces that for
/// `[cluster] local_path_dir`, so callers pass a validated value.
pub fn k0s_local_path_manifest(data_dir: &str) -> String {
    include_str!("assets/local-path-provisioner.yaml").replace("__LOCAL_PATH_DIR__", data_dir)
}

#[cfg(test)]
mod local_path_tests {
    use super::k0s_local_path_manifest;

    #[test]
    fn renders_data_dir_and_default_storageclass() {
        let m = k0s_local_path_manifest("/mnt/scratch/local-path");
        // The placeholder is substituted and no literal placeholder survives.
        assert!(m.contains("\"paths\":[\"/mnt/scratch/local-path\"]"));
        assert!(!m.contains("__LOCAL_PATH_DIR__"));
        // Shipped as the cluster default so unclassed PVCs bind.
        assert!(m.contains("storageclass.kubernetes.io/is-default-class: \"true\""));
        assert!(m.contains("kind: StorageClass"));
        assert!(m.contains("provisioner: rancher.io/local-path"));
    }
}

/// Generate a k0s controller config (YAML) carrying `pod_cidr`/`service_cidr` for either supported
/// `cni` ("kuberouter" or "calico") — without this, k0s falls back to its own built-in CIDRs and
/// silently ignores what's configured. `calico` additionally advertises the API on `api_address`
/// (the control-plane's WireGuard mesh IP, when known) and runs Calico in `bird` mode (native
/// routing, no overlay) so pod traffic rides the mesh; `cni_mtu` sets Calico's MTU (typically below
/// the underlay to leave room for WireGuard's ~50-byte overhead, avoiding fragmentation). `sans` are
/// extra API-server certificate SANs (e.g. the control-plane's mesh + underlay IPs), used only when
/// `api_address` is set.
///
/// For a multi-CP Calico cluster (`cp_count > 1`) no VIP can float over WireGuard cryptokey routing,
/// so node-local load balancing (EnvoyProxy) is enabled to give konnectivity a cluster-wide balanced
/// endpoint instead of pinning every agent to one controller.
pub fn k0s_controller_config_yaml(
    cni: &str,
    pod_cidr: &str,
    service_cidr: &str,
    cni_mtu: u16,
    api_address: Option<&str>,
    sans: &[String],
    cp_count: usize,
) -> String {
    let mesh_native = cni == "calico";
    let mut y = String::new();
    y.push_str("apiVersion: k0s.k0sproject.io/v1beta1\n");
    y.push_str("kind: ClusterConfig\n");
    y.push_str("metadata:\n");
    y.push_str("  name: k0s\n");
    y.push_str("spec:\n");
    if mesh_native {
        if let Some(api_address) = api_address {
            y.push_str("  api:\n");
            y.push_str(&format!("    address: {api_address}\n"));
            if !sans.is_empty() {
                y.push_str("    sans:\n");
                for san in sans {
                    y.push_str(&format!("      - {san}\n"));
                }
            }
        }
    }
    y.push_str("  network:\n");
    y.push_str(&format!("    provider: {cni}\n"));
    y.push_str(&format!("    podCIDR: {pod_cidr}\n"));
    y.push_str(&format!("    serviceCIDR: {service_cidr}\n"));
    if mesh_native {
        y.push_str("    calico:\n");
        y.push_str("      mode: bird\n");
        y.push_str(&format!("      mtu: {cni_mtu}\n"));
        if cp_count > 1 {
            y.push_str("    nodeLocalLoadBalancing:\n");
            y.push_str("      enabled: true\n");
            y.push_str("      type: EnvoyProxy\n");
        }
    }
    y
}

#[cfg(test)]
mod cluster_state_tests {
    use super::{K0sClusterState, K0sPhase};

    // Frozen pre-multi-CP snapshot shape (no control_plane_nodes); must still deserialize or
    // spurctld crashes restoring an old raft snapshot. Never regenerate.
    #[test]
    fn pre_multi_cp_state_deserializes_and_folds_into_controllers() {
        const PRE_MULTI_CP: &str =
            r#"{"phase":"ready","control_plane_node":"head-node","reset_requested":false}"#;
        let st: K0sClusterState = serde_json::from_str(PRE_MULTI_CP).expect(
            "pre-multi-CP K0sClusterState must deserialize; a new field needs serde(default)",
        );
        assert_eq!(st.phase, K0sPhase::Ready);
        assert!(st.control_plane_nodes.is_empty());
        assert_eq!(st.controllers(), vec!["head-node"]);
        assert_eq!(st.bootstrap().as_deref(), Some("head-node"));
    }

    #[test]
    fn controllers_prefers_the_set_when_present() {
        let st = K0sClusterState {
            phase: K0sPhase::Ready,
            control_plane_node: Some("cp-1".into()),
            control_plane_nodes: vec!["cp-1".into(), "cp-2".into(), "cp-3".into()],
            ..Default::default()
        };
        assert_eq!(st.controllers(), vec!["cp-1", "cp-2", "cp-3"]);
        assert_eq!(st.bootstrap().as_deref(), Some("cp-1"));
    }

    #[test]
    fn bootstrap_falls_back_to_first_of_set_when_singular_absent() {
        let st = K0sClusterState {
            phase: K0sPhase::Ready,
            control_plane_nodes: vec!["cp-1".into(), "cp-2".into(), "cp-3".into()],
            ..Default::default()
        };
        assert_eq!(st.bootstrap().as_deref(), Some("cp-1"));
    }

    #[test]
    fn controllers_empty_when_down() {
        assert!(K0sClusterState::default().controllers().is_empty());
    }

    #[test]
    fn is_member_empty_scope_matches_all() {
        let st = K0sClusterState::default();
        assert!(st.is_member("anything"), "empty scope = whole inventory");
    }

    #[test]
    fn is_member_respects_recorded_scope() {
        let st = K0sClusterState {
            member_nodes: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert!(st.is_member("a"));
        assert!(st.is_member("b"));
        assert!(!st.is_member("c"), "out-of-scope node excluded");
    }
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
            Some("192.0.2.1"),
            &["192.0.2.1".to_string(), "203.0.113.9".to_string()],
            1,
        );
        assert!(y.contains("address: 192.0.2.1"));
        assert!(y.contains("      - 203.0.113.9"));
        assert!(y.contains("provider: calico"));
        assert!(y.contains("mode: bird"));
        assert!(y.contains("mtu: 1450"));
        assert!(y.contains("podCIDR: 192.0.2.0/24"));
        assert!(y.contains("serviceCIDR: 198.51.100.0/24"));
    }

    /// The bug this module fixes: kuberouter used to get no config at all, so a configured
    /// pod/service CIDR was silently ignored and k0s fell back to its own built-in default.
    #[test]
    fn kuberouter_carries_configured_pod_and_service_cidr() {
        let y = k0s_controller_config_yaml(
            "kuberouter",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            Some("192.0.2.1"),
            &[],
            3,
        );
        assert!(y.contains("provider: kuberouter"));
        assert!(y.contains("podCIDR: 192.0.2.0/24"));
        assert!(y.contains("serviceCIDR: 198.51.100.0/24"));
        assert!(!y.contains("api:"));
        assert!(!y.contains("calico:"));
    }

    #[test]
    fn calico_without_mesh_ip_still_carries_cidr_but_omits_api_block() {
        let y = k0s_controller_config_yaml(
            "calico",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            None,
            &[],
            1,
        );
        assert!(y.contains("podCIDR: 192.0.2.0/24"));
        assert!(y.contains("serviceCIDR: 198.51.100.0/24"));
        assert!(!y.contains("api:"));
    }

    #[test]
    fn multi_cp_calico_enables_node_local_load_balancing() {
        // Documentation-range addresses only (RFC 5737 TEST-NET); no real infrastructure IPs.
        let y = k0s_controller_config_yaml(
            "calico",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            Some("192.0.2.1"),
            &["192.0.2.1".to_string()],
            3,
        );
        assert!(y.contains("nodeLocalLoadBalancing:"));
        assert!(y.contains("enabled: true"));
        assert!(y.contains("type: EnvoyProxy"));
    }

    #[test]
    fn single_cp_calico_omits_node_local_load_balancing() {
        // Documentation-range addresses only (RFC 5737 TEST-NET); no real infrastructure IPs.
        let y = k0s_controller_config_yaml(
            "calico",
            "192.0.2.0/24",
            "198.51.100.0/24",
            1450,
            Some("192.0.2.1"),
            &["192.0.2.1".to_string()],
            1,
        );
        assert!(!y.contains("nodeLocalLoadBalancing"));
    }
}

/// Valid HA control-plane counts. Odd only (etcd quorum); capped at 5 (diminishing returns, more
/// etcd write latency). A single control plane (1) is the non-HA default.
pub const VALID_CONTROL_PLANE_REPLICAS: [u32; 3] = [1, 3, 5];

/// Validate an HA control-plane replica count: must be 1, 3, or 5. Shared by config load and the
/// `spur k8s up` gRPC boundary so an even/too-large count fails closed with one consistent message.
pub fn validate_control_plane_replicas(replicas: u32) -> Result<(), String> {
    if VALID_CONTROL_PLANE_REPLICAS.contains(&replicas) {
        return Ok(());
    }
    Err(format!(
        "control-plane replicas must be 1, 3, or 5 for etcd quorum (got {replicas})"
    ))
}

/// The majority of `n` (`n/2 + 1`) — the number of control planes whose k0s unit must be active for
/// the reconcile loop to gate `Ready` (a proxy for etcd quorum, not a direct etcd health check).
/// `quorum(0)` is 0 (no control plane assigned yet — never a satisfiable quorum).
pub fn quorum(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    n / 2 + 1
}

#[cfg(test)]
mod quorum_tests {
    use super::quorum;

    #[test]
    fn quorum_is_majority_for_valid_cp_counts() {
        assert_eq!(quorum(1), 1);
        assert_eq!(quorum(3), 2);
        assert_eq!(quorum(5), 3);
    }

    #[test]
    fn quorum_of_zero_is_zero() {
        assert_eq!(quorum(0), 0);
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
    /// Bootstrap control-plane: seeds etcd (started tokenless), primary endpoint for admin/token RPCs.
    #[serde(default)]
    pub control_plane_node: Option<String>,
    /// All control-plane nodes (1/3/5). Empty on pre-multi-CP state — read via [`Self::controllers`].
    #[serde(default)]
    pub control_plane_nodes: Vec<String>,
    /// Nodes the cluster is scoped to. Empty = enroll the whole inventory (back-compat).
    #[serde(default)]
    pub member_nodes: Vec<String>,
    #[serde(default)]
    pub reset_requested: bool,
}

impl K0sClusterState {
    /// The control-plane node set, back-compat with pre-multi-CP state: falls back to the singular
    /// `control_plane_node` when `control_plane_nodes` is empty (old snapshots/WAL entries).
    pub fn controllers(&self) -> Vec<String> {
        if !self.control_plane_nodes.is_empty() {
            return self.control_plane_nodes.clone();
        }
        self.control_plane_node.iter().cloned().collect()
    }

    /// The bootstrap control-plane (etcd seed): the recorded singular node, else the first of the
    /// set for HA state where the singular field was never written.
    pub fn bootstrap(&self) -> Option<String> {
        self.control_plane_node
            .clone()
            .or_else(|| self.control_plane_nodes.first().cloned())
    }

    /// Whether `name` is in scope for enrollment. An empty `member_nodes` means the whole inventory.
    pub fn is_member(&self, name: &str) -> bool {
        self.member_nodes.is_empty() || self.member_nodes.iter().any(|n| n == name)
    }
}
