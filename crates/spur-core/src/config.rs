// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

use crate::partition::{Partition, PartitionState, PreemptMode};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("invalid value for {field}: {value}")]
    InvalidValue { field: String, value: String },
}

/// Top-level configuration (slurm.conf equivalent, in TOML).
///
/// We support reading both our native TOML format and the traditional
/// Slurm key=value format for migration purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlurmConfig {
    pub cluster_name: String,

    #[serde(default)]
    pub controller: ControllerConfig,

    #[serde(default)]
    pub accounting: AccountingConfig,

    #[serde(default)]
    pub scheduler: SchedulerConfig,

    #[serde(default)]
    pub auth: AuthConfig,

    #[serde(default)]
    pub partitions: Vec<PartitionConfig>,

    #[serde(default)]
    pub nodes: Vec<NodeConfig>,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    #[serde(default)]
    pub kubernetes: KubernetesConfig,

    #[serde(default)]
    pub notifications: NotificationConfig,

    #[serde(default)]
    pub power: PowerConfig,

    #[serde(default)]
    pub federation: FederationConfig,

    /// Topology configuration (switch hierarchy for locality-aware scheduling).
    #[serde(default)]
    pub topology: Option<crate::topology::TopologyConfig>,

    /// Job isolation configuration.
    #[serde(default)]
    pub isolation: IsolationConfig,

    /// Cluster-wide license pool, e.g., {"fluent": 20, "comsol": 5}.
    #[serde(default)]
    pub licenses: HashMap<String, u64>,

    /// Cluster-wide burst-buffer capacity pool.
    #[serde(default)]
    pub burst_buffer: BurstBufferConfig,

    /// Auto-update configuration.
    #[serde(default)]
    pub update: UpdateConfig,

    /// OpenMetrics HTTP export (spurctld, default port 6822).
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// REST API (Slurm-compatible HTTP, default port 6820).
    #[serde(default)]
    pub rest_api: RestApiConfig,

    /// Prolog/epilog hook scripts.
    #[serde(default)]
    pub hooks: HooksConfig,

    /// Device discovery, CDI, and GRES configuration.
    #[serde(default)]
    pub devices: DevicesConfig,

    /// Node admission control.
    #[serde(default)]
    pub admission: AdmissionConfig,
}

/// Configuration for auto-update checking and self-update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Check for updates on daemon startup (default: true).
    #[serde(default = "default_true_fn")]
    pub check_on_startup: bool,

    /// Automatically download and install updates (default: false).
    /// Even when true, daemons will NOT auto-restart.
    #[serde(default)]
    pub auto_update: bool,

    /// Release channel: "stable" or "nightly" (default: "stable").
    #[serde(default = "default_stable")]
    pub channel: String,

    /// Directory for the update check cache file.
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
}

fn default_stable() -> String {
    "stable".into()
}
fn default_cache_dir() -> String {
    "/var/cache/spur".into()
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            auto_update: false,
            channel: "stable".into(),
            cache_dir: "/var/cache/spur".into(),
        }
    }
}

/// OpenMetrics export settings for spurctld (separate listener from gRPC 6817).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsConfig {
    /// When false, spurctld does not start the metrics HTTP server.
    #[serde(default = "default_true_fn")]
    pub enabled: bool,
    /// Metrics HTTP listen address (port used when `bind = "loopback"`).
    #[serde(default = "default_metrics_listen_addr")]
    pub listen_addr: String,
    /// `loopback` binds 127.0.0.1; `all` uses `listen_addr` as-is.
    #[serde(default)]
    pub bind: MetricsBind,
    /// Reserved for `/metrics/jobs-users-accts` (high cardinality; off by default).
    /// Route exists but returns 404 until a follow-up PR implements the exporter.
    #[serde(default)]
    pub high_cardinality: bool,
}

fn default_metrics_listen_addr() -> String {
    "[::]:6822".into()
}

/// Metrics HTTP bind policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MetricsBind {
    #[default]
    Loopback,
    All,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_addr: default_metrics_listen_addr(),
            bind: MetricsBind::Loopback,
            high_cardinality: false,
        }
    }
}

impl MetricsConfig {
    /// Listen socket after applying [`MetricsBind`].
    ///
    /// Returns an error if `listen_addr` is not a valid `SocketAddr`.
    pub fn effective_listen_addr(&self) -> Result<std::net::SocketAddr, ConfigError> {
        let addr = self
            .listen_addr
            .parse::<std::net::SocketAddr>()
            .map_err(|e| ConfigError::InvalidValue {
                field: "metrics.listen_addr".into(),
                value: e.to_string(),
            })?;
        Ok(match self.bind {
            MetricsBind::All => addr,
            MetricsBind::Loopback => std::net::SocketAddr::from(([127, 0, 0, 1], addr.port())),
        })
    }
}

/// REST API settings for spurctld (Slurm-compatible HTTP on a separate port).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestApiConfig {
    /// When false, spurctld does not start the REST server.
    #[serde(default = "default_true_fn")]
    pub enabled: bool,
}

impl Default for RestApiConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Prolog and epilog hook script configuration.
///
/// All fields are optional — `None` means no hook is configured for that point.
/// Paths must be fully qualified; no search path is set for security reasons.
/// Hook lifecycle and failure semantics match Slurm.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// Script run on compute nodes before job launch (Slurm `Prolog`).
    pub prolog: Option<String>,
    /// Script run on compute nodes at job termination (Slurm `Epilog`).
    pub epilog: Option<String>,
    /// Script run on the controller at job allocation (Slurm `PrologSlurmctld`).
    pub prolog_slurmctld: Option<String>,
    /// Script run on the controller at job termination (Slurm `EpilogSlurmctld`).
    pub epilog_slurmctld: Option<String>,
    /// Script run on compute nodes before each job step (Slurm `TaskProlog`).
    pub task_prolog: Option<String>,
    /// Script run on compute nodes after each job step (Slurm `TaskEpilog`).
    pub task_epilog: Option<String>,
    /// Script run on the srun invocation node before step dispatch (Slurm `SrunProlog`).
    pub srun_prolog: Option<String>,
    /// Script run on the srun invocation node after step completion (Slurm `SrunEpilog`).
    pub srun_epilog: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// gRPC listen address for the controller.
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    /// REST API listen address.
    #[serde(default = "default_rest_addr")]
    pub rest_addr: String,
    /// Hostname(s) for HA. First is primary.
    #[serde(default = "default_hosts")]
    pub hosts: Vec<String>,
    /// State save location.
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    /// Max job ID before wrapping.
    #[serde(default = "default_max_job_id")]
    pub max_job_id: u32,
    /// First job ID.
    #[serde(default = "default_one")]
    pub first_job_id: u32,

    /// Raft peers for HA consensus. Each entry is "host:port" (Raft gRPC address).
    /// If empty, single-node mode (no Raft, no replication).
    /// Example: ["node1:6821", "node2:6821", "node3:6821"]
    #[serde(default)]
    pub peers: Vec<String>,

    /// This node's Raft ID. If not set, auto-derived from hostname ordinal
    /// (e.g. spurctld-2 → node_id 3) or defaults to 1.
    pub node_id: Option<u64>,

    /// Listen address for Raft internal gRPC traffic (separate from client API).
    #[serde(default = "default_raft_listen_addr")]
    pub raft_listen_addr: String,

    /// Seconds without a heartbeat before a node is marked Down.
    /// Defaults to 90 when absent.
    #[serde(default)]
    pub heartbeat_timeout_secs: Option<u64>,
}

fn default_listen_addr() -> String {
    "[::]:6817".into()
}
fn default_raft_listen_addr() -> String {
    "[::]:6821".into()
}
fn default_rest_addr() -> String {
    "[::]:6820".into()
}
fn default_hosts() -> Vec<String> {
    vec!["localhost".into()]
}
fn default_state_dir() -> String {
    "/var/spool/spur".into()
}
fn default_max_job_id() -> u32 {
    999_999_999
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "[::]:6817".into(),
            rest_addr: "[::]:6820".into(),
            hosts: vec!["localhost".into()],
            state_dir: "/var/spool/spur".into(),
            max_job_id: 999_999_999,
            first_job_id: 1,
            peers: Vec::new(),
            node_id: None,
            raft_listen_addr: "[::]:6821".into(),
            heartbeat_timeout_secs: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountingConfig {
    /// PostgreSQL connection string for accounting. When set, spurctld
    /// connects directly and serves the SlurmAccounting gRPC service on
    /// port 6817 alongside the controller API.
    #[serde(default = "default_database_url")]
    pub database_url: String,
    /// How often to refresh fairshare/QoS caches from the accounting database.
    #[serde(default = "default_fairshare_refresh_secs")]
    pub fairshare_refresh_secs: u32,
}

fn default_database_url() -> String {
    "postgresql://spur:spur@localhost/spur".into()
}
fn default_fairshare_refresh_secs() -> u32 {
    300
}

impl Default for AccountingConfig {
    fn default() -> Self {
        Self {
            database_url: "postgresql://spur:spur@localhost/spur".into(),
            fairshare_refresh_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Scheduler plugin name.
    #[serde(default = "default_scheduler_plugin")]
    pub plugin: String,
    /// How often to run the scheduler (seconds).
    #[serde(default = "default_one")]
    pub interval_secs: u32,
    /// Max jobs to evaluate per cycle.
    #[serde(default = "default_max_jobs")]
    pub max_jobs_per_cycle: u32,
    /// Fair-share decay half-life (days).
    #[serde(default = "default_halflife")]
    pub fairshare_halflife_days: u32,
    /// Default job time limit (minutes), if not set per-partition.
    #[serde(default = "default_time_limit")]
    pub default_time_limit_minutes: u32,
    /// Max seconds to wait in COMPLETING before force-finishing the job.
    #[serde(default = "default_complete_wait")]
    pub complete_wait_secs: u32,
}

fn default_scheduler_plugin() -> String {
    "backfill".into()
}
fn default_max_jobs() -> u32 {
    10000
}
fn default_halflife() -> u32 {
    14
}
fn default_time_limit() -> u32 {
    60
}
fn default_complete_wait() -> u32 {
    300
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            plugin: "backfill".into(),
            interval_secs: 1,
            max_jobs_per_cycle: 10000,
            fairshare_halflife_days: 14,
            default_time_limit_minutes: 60,
            complete_wait_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin: "jwt", "munge", "none".
    pub plugin: String,
    /// JWT secret key (file path or inline).
    pub jwt_key: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            plugin: "jwt".into(),
            jwt_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionConfig {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default = "default_partition_state")]
    pub state: String,
    #[serde(default)]
    pub nodes: String,
    #[serde(default)]
    pub selector: HashMap<String, String>,
    pub max_time: Option<String>,
    pub default_time: Option<String>,
    pub max_nodes: Option<u32>,
    #[serde(default = "default_one")]
    pub min_nodes: u32,
    #[serde(default)]
    pub allow_accounts: Vec<String>,
    #[serde(default)]
    pub allow_groups: Vec<String>,
    #[serde(default)]
    pub priority_tier: u32,
    #[serde(default)]
    pub preempt_mode: String,
}

fn default_partition_state() -> String {
    "UP".into()
}
fn default_one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Hostlist pattern for this node definition. Optional when selector is used.
    #[serde(default)]
    pub names: String,
    /// Label selector: apply this config to nodes matching ALL key-value pairs.
    #[serde(default)]
    pub selector: HashMap<String, String>,
    #[serde(default)]
    pub cpus: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub gres: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
    /// Override address (if different from hostname).
    pub address: Option<String>,
    /// Scheduling weight. Higher weight = preferred for scheduling.
    #[serde(default = "default_one")]
    pub weight: u32,
}

/// Network / WireGuard mesh configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Enable WireGuard mesh networking between nodes.
    #[serde(default)]
    pub wg_enabled: bool,
    /// CIDR block for WireGuard address allocation (default: 10.44.0.0/16).
    #[serde(default = "default_wg_cidr")]
    pub wg_cidr: String,
    /// WireGuard interface name (default: spur0).
    #[serde(default = "default_wg_interface")]
    pub wg_interface: String,
    /// WireGuard listen port (default: 51820).
    #[serde(default = "default_wg_port")]
    pub wg_port: u16,
    /// Agent gRPC listen port (default: 6818).
    #[serde(default = "default_agent_port")]
    pub agent_port: u16,
}

fn default_wg_cidr() -> String {
    "10.44.0.0/16".into()
}
fn default_wg_interface() -> String {
    "spur0".into()
}
fn default_wg_port() -> u16 {
    51820
}
fn default_agent_port() -> u16 {
    6818
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            wg_enabled: false,
            wg_cidr: "10.44.0.0/16".into(),
            wg_interface: "spur0".into(),
            wg_port: 51820,
            agent_port: 6818,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub file: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "text".into(),
            file: None,
        }
    }
}

/// Kubernetes integration configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KubernetesConfig {
    /// Enable K8s integration.
    #[serde(default)]
    pub enabled: bool,
    /// Path to kubeconfig file. If empty, uses in-cluster config.
    #[serde(default)]
    pub kubeconfig: Option<String>,
    /// K8s namespace for SpurJob CRDs and Pods.
    #[serde(default = "default_k8s_namespace")]
    pub namespace: String,
    /// Label selector for K8s nodes to include in the Spur pool.
    #[serde(default = "default_k8s_node_selector")]
    pub node_label_selector: String,
}

fn default_k8s_namespace() -> String {
    "spur".into()
}

fn default_k8s_node_selector() -> String {
    "spur.amd.com/managed=true".into()
}

impl Default for KubernetesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kubeconfig: None,
            namespace: "spur".into(),
            node_label_selector: "spur.amd.com/managed=true".into(),
        }
    }
}

/// Power management configuration for suspending/resuming idle nodes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PowerConfig {
    /// Seconds a node must be idle before it is suspended.
    pub suspend_timeout_secs: Option<u64>,
    /// Command to suspend a node (e.g., "systemctl suspend"). {node} is replaced with the node name.
    pub suspend_command: Option<String>,
    /// Command to resume a node (e.g., "ipmitool chassis power on"). {node} is replaced with the node name.
    pub resume_command: Option<String>,
}

/// Notification configuration for job event webhooks and email.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotificationConfig {
    /// Webhook URL to POST job event notifications to.
    pub webhook_url: Option<String>,
    /// SMTP command for sending mail, e.g., "/usr/sbin/sendmail -t".
    pub smtp_command: Option<String>,
    /// From address for notification emails, e.g., "spur@cluster.local".
    pub from_address: Option<String>,
}

/// Cluster-wide burst-buffer capacity, modeled like a single consumable pool
/// (analogous to a license total). Jobs reserve GB via `--bb capacity=NNN`.
///
/// ```toml
/// [burst_buffer]
/// total_gb = 1024
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BurstBufferConfig {
    /// Total burst-buffer capacity in gibibytes. 0 (default) disables BB: any
    /// job requesting capacity stays pending with `BurstBufferResources`.
    #[serde(default)]
    pub total_gb: u64,
}

/// Job isolation configuration for native-host and container jobs.
///
/// Each layer operates independently and degrades gracefully when the
/// kernel doesn't support it or spurd isn't running as root.
///
/// Example:
/// ```toml
/// [isolation]
/// setuid = true       # Run jobs as submitting user (requires root)
/// namespaces = true   # PID + mount namespace isolation
/// seccomp = true      # syscall whitelist (blocks ptrace, mount, bpf)
/// landlock = true     # filesystem access control (kernel 5.13+)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolationConfig {
    /// Run jobs as the submitting user's UID/GID (requires root spurd).
    #[serde(default = "default_true_fn")]
    pub setuid: bool,
    /// PID + mount namespace isolation (requires root).
    #[serde(default = "default_true_fn")]
    pub namespaces: bool,
    /// seccomp-BPF syscall filter (kernel 3.5+).
    #[serde(default = "default_true_fn")]
    pub seccomp: bool,
    /// Landlock filesystem access control (kernel 5.13+, native-host only).
    #[serde(default = "default_true_fn")]
    pub landlock: bool,
}

fn default_true_fn() -> bool {
    true
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            setuid: true,
            namespaces: true,
            seccomp: true,
            landlock: true,
        }
    }
}

/// Federation configuration for multi-cluster job routing.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FederationConfig {
    /// Peer clusters in the federation.
    pub clusters: Vec<ClusterPeer>,
}

/// A peer cluster in a federation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPeer {
    /// Name of the peer cluster.
    pub name: String,
    /// gRPC address of the peer controller (e.g., "http://peer-ctrl:6817").
    pub address: String,
}

/// Device discovery and CDI configuration.
///
/// `auto_detect` is AMD KFD sysfs only; other vendors need on-disk CDI specs or GRES
/// `file` entries.
///
/// ```toml
/// [devices]
/// auto_detect = true
/// cdi_spec_dirs = ["/opt/vendor/cdi"]
///
/// [[devices.gres]]
/// name = "gpu"
/// file = "/dev/dri/renderD[128-129]"
/// flags = ["amd_gpu_env"]
///
/// [[devices.gres]]
/// name = "bandwidth"
/// type = "lustre"
/// count = 4096
/// flags = ["count_only"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicesConfig {
    /// Discover GPUs from KFD when the CDI cache is empty. AMD only for now.
    #[serde(default = "default_true_fn")]
    pub auto_detect: bool,

    /// Additional directories to scan for CDI spec files, beyond the
    /// defaults (`/etc/cdi`, `/var/run/cdi`).
    #[serde(default)]
    pub cdi_spec_dirs: Vec<String>,

    /// File-based or countable GRES pools alongside CDI-discovered devices.
    #[serde(default)]
    pub gres: Vec<DevicesGresEntry>,
}

impl Default for DevicesConfig {
    fn default() -> Self {
        Self {
            auto_detect: true,
            cdi_spec_dirs: Vec::new(),
            gres: Vec::new(),
        }
    }
}

/// A `[[devices.gres]]` entry (Slurm-compatible syntax).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicesGresEntry {
    pub name: String,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub multiple_files: Option<String>,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub cores: Option<String>,
    #[serde(default)]
    pub links: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
}

/// Node admission control configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionConfig {
    #[serde(default)]
    pub mode: AdmissionMode,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            mode: AdmissionMode::Open,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionMode {
    #[default]
    Open,
    Token,
}

impl SlurmConfig {
    /// Load from a TOML file.
    pub fn load_from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Load from a TOML string.
    pub fn load_from_str(s: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.cluster_name.is_empty() {
            return Err(ConfigError::MissingField("cluster_name".into()));
        }
        Ok(())
    }

    /// Convert partition configs to Partition structs.
    pub fn build_partitions(&self) -> Vec<Partition> {
        self.partitions
            .iter()
            .map(|pc| Partition {
                name: pc.name.clone(),
                state: match pc.state.to_uppercase().as_str() {
                    "UP" => PartitionState::Up,
                    "DOWN" => PartitionState::Down,
                    "DRAIN" => PartitionState::Drain,
                    _ => PartitionState::Inactive,
                },
                is_default: pc.default,
                nodes: pc.nodes.clone(),
                selector: pc.selector.clone(),
                max_time_minutes: pc.max_time.as_ref().and_then(|t| parse_time_minutes(t)),
                default_time_minutes: pc.default_time.as_ref().and_then(|t| parse_time_minutes(t)),
                max_nodes: pc.max_nodes,
                min_nodes: pc.min_nodes,
                priority_tier: pc.priority_tier,
                preempt_mode: match pc.preempt_mode.to_lowercase().as_str() {
                    "cancel" => PreemptMode::Cancel,
                    "requeue" => PreemptMode::Requeue,
                    "suspend" => PreemptMode::Suspend,
                    _ => PreemptMode::Off,
                },
                ..Default::default()
            })
            .collect()
    }
}

/// Parse a time string like "72:00:00", "4-00:00:00", "INFINITE", "60" (minutes).
pub fn parse_time_minutes(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("INFINITE") || s.eq_ignore_ascii_case("UNLIMITED") {
        return None; // No limit
    }

    // days-hours:minutes:seconds
    if let Some((days, rest)) = s.split_once('-') {
        let days: u32 = days.parse().ok()?;
        let hms = parse_hms(rest)?;
        return Some(days * 24 * 60 + hms);
    }

    // hours:minutes:seconds or hours:minutes or just minutes
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => parts[0].parse().ok(),
        2 => {
            let h: u32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            Some(h * 60 + m)
        }
        3 => Some(parse_hms(s)?),
        _ => None,
    }
}

/// Parse a time string to total seconds (not minutes).
///
/// Same Slurm-compatible format as `parse_time_minutes` but with second
/// granularity: "N" → N minutes, "H:MM" → hours+minutes, "H:MM:SS" → exact.
pub fn parse_time_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("INFINITE") || s.eq_ignore_ascii_case("UNLIMITED") {
        return None;
    }

    // days-hours:minutes:seconds
    if let Some((days, rest)) = s.split_once('-') {
        let days: u64 = days.parse().ok()?;
        return Some(days * 86400 + parse_hms_seconds(rest)?);
    }

    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            // Just minutes → convert to seconds
            let mins: u64 = parts[0].parse().ok()?;
            Some(mins * 60)
        }
        2 => {
            // HH:MM → hours and minutes (no seconds)
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            Some(h * 3600 + m * 60)
        }
        3 => {
            // HH:MM:SS
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            let sec: u64 = parts[2].parse().ok()?;
            Some(h * 3600 + m * 60 + sec)
        }
        _ => None,
    }
}

fn parse_hms_seconds(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            Some(h * 3600 + m * 60)
        }
        3 => {
            let h: u64 = parts[0].parse().ok()?;
            let m: u64 = parts[1].parse().ok()?;
            let sec: u64 = parts[2].parse().ok()?;
            Some(h * 3600 + m * 60 + sec)
        }
        _ => None,
    }
}

fn parse_hms(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 && parts.len() != 2 {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let s: u32 = if parts.len() == 3 {
        parts[2].parse().ok()?
    } else {
        0
    };
    Some(h * 60 + m + if s > 0 { 1 } else { 0 }) // Round up seconds
}

/// Format minutes as D-HH:MM:SS or HH:MM:SS.
pub fn format_time(total_minutes: Option<u32>) -> String {
    match total_minutes {
        None => "UNLIMITED".into(),
        Some(mins) => {
            let days = mins / (24 * 60);
            let hours = (mins % (24 * 60)) / 60;
            let minutes = mins % 60;
            if days > 0 {
                format!("{}-{:02}:{:02}:00", days, hours, minutes)
            } else {
                format!("{:02}:{:02}:00", hours, minutes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time() {
        assert_eq!(parse_time_minutes("60"), Some(60));
        assert_eq!(parse_time_minutes("1:30"), Some(90));
        assert_eq!(parse_time_minutes("72:00:00"), Some(4320));
        assert_eq!(parse_time_minutes("1-00:00:00"), Some(1440));
        assert_eq!(parse_time_minutes("INFINITE"), None);
    }

    #[test]
    fn test_parse_time_seconds() {
        // "N" → N minutes in seconds
        assert_eq!(parse_time_seconds("1"), Some(60));
        assert_eq!(parse_time_seconds("60"), Some(3600));
        // "H:MM" → exact seconds
        assert_eq!(parse_time_seconds("1:30"), Some(5400)); // 1h30m
                                                            // "H:MM:SS" → exact seconds (the key case)
        assert_eq!(parse_time_seconds("0:00:10"), Some(10));
        assert_eq!(parse_time_seconds("0:01:30"), Some(90));
        assert_eq!(parse_time_seconds("1:00:00"), Some(3600));
        // days-HH:MM:SS
        assert_eq!(parse_time_seconds("1-00:00:00"), Some(86400));
        assert_eq!(parse_time_seconds("7-00:00:00"), Some(604800));
        // limits
        assert_eq!(parse_time_seconds("INFINITE"), None);
        assert_eq!(parse_time_seconds("UNLIMITED"), None);
    }

    #[test]
    fn test_format_time() {
        assert_eq!(format_time(Some(90)), "01:30:00");
        assert_eq!(format_time(Some(1500)), "1-01:00:00");
        assert_eq!(format_time(None), "UNLIMITED");
    }

    #[test]
    fn test_load_metrics_config() {
        let toml = r#"
cluster_name = "test"

[metrics]
enabled = false
listen_addr = "[::]:9999"
bind = "all"
high_cardinality = true
"#;
        let config = SlurmConfig::load_from_str(toml).unwrap();
        assert!(!config.metrics.enabled);
        assert_eq!(config.metrics.listen_addr, "[::]:9999");
        assert_eq!(config.metrics.bind, MetricsBind::All);
        assert!(config.metrics.high_cardinality);
        assert_eq!(
            config.metrics.effective_listen_addr().unwrap(),
            "[::]:9999".parse().unwrap()
        );
    }

    #[test]
    fn test_metrics_defaults() {
        let config = SlurmConfig::load_from_str(r#"cluster_name = "x""#).unwrap();
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen_addr, "[::]:6822");
        assert_eq!(config.metrics.bind, MetricsBind::Loopback);
        assert!(!config.metrics.high_cardinality);
        assert_eq!(
            config.metrics.effective_listen_addr().unwrap(),
            "127.0.0.1:6822".parse().unwrap()
        );
    }

    #[test]
    fn test_metrics_stale_exposition_format_ignored() {
        let toml = r#"
cluster_name = "x"

[metrics]
exposition_format = "slurm_0_0_4"
"#;
        let config = SlurmConfig::load_from_str(toml);
        assert!(
            config.is_ok(),
            "stale exposition_format key should be silently ignored"
        );
    }

    #[test]
    fn test_metrics_invalid_listen_addr() {
        let config = SlurmConfig::load_from_str(
            r#"
cluster_name = "x"

[metrics]
listen_addr = "not-a-socket"
"#,
        )
        .unwrap();
        assert!(config.metrics.effective_listen_addr().is_err());
    }

    #[test]
    fn test_load_hooks_config() {
        let toml = r#"
cluster_name = "test"

[hooks]
prolog = "/etc/spur/prolog.sh"
epilog = "/etc/spur/epilog.sh"
prolog_slurmctld = "/etc/spur/prolog_slurmctld.sh"
epilog_slurmctld = "/etc/spur/epilog_slurmctld.sh"
task_prolog = "/etc/spur/task_prolog.sh"
task_epilog = "/etc/spur/task_epilog.sh"
srun_prolog = "/etc/spur/srun_prolog.sh"
srun_epilog = "/etc/spur/srun_epilog.sh"
"#;
        let config = SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(config.hooks.prolog.as_deref(), Some("/etc/spur/prolog.sh"));
        assert_eq!(config.hooks.epilog.as_deref(), Some("/etc/spur/epilog.sh"));
        assert_eq!(
            config.hooks.prolog_slurmctld.as_deref(),
            Some("/etc/spur/prolog_slurmctld.sh")
        );
        assert_eq!(
            config.hooks.epilog_slurmctld.as_deref(),
            Some("/etc/spur/epilog_slurmctld.sh")
        );
        assert_eq!(
            config.hooks.task_prolog.as_deref(),
            Some("/etc/spur/task_prolog.sh")
        );
        assert_eq!(
            config.hooks.task_epilog.as_deref(),
            Some("/etc/spur/task_epilog.sh")
        );
        assert_eq!(
            config.hooks.srun_prolog.as_deref(),
            Some("/etc/spur/srun_prolog.sh")
        );
        assert_eq!(
            config.hooks.srun_epilog.as_deref(),
            Some("/etc/spur/srun_epilog.sh")
        );
        // metrics section omitted — should keep defaults
        assert!(config.metrics.enabled);
    }

    #[test]
    fn test_hooks_defaults() {
        let config = SlurmConfig::load_from_str(r#"cluster_name = "x""#).unwrap();
        assert!(config.hooks.prolog.is_none());
        assert!(config.hooks.epilog.is_none());
        assert!(config.hooks.prolog_slurmctld.is_none());
        assert!(config.hooks.epilog_slurmctld.is_none());
        assert!(config.hooks.task_prolog.is_none());
        assert!(config.hooks.task_epilog.is_none());
        assert!(config.hooks.srun_prolog.is_none());
        assert!(config.hooks.srun_epilog.is_none());
        // hooks section omitted — metrics should keep defaults
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen_addr, "[::]:6822");
    }

    #[test]
    fn test_load_config() {
        let toml = r#"
cluster_name = "test-cluster"

[controller]
listen_addr = "[::]:6817"
rest_addr = "[::]:6820"
hosts = ["ctrl1", "ctrl2"]
state_dir = "/var/spool/spur"
max_job_id = 999999999
first_job_id = 100

[scheduler]
plugin = "backfill"
interval_secs = 2

[[partitions]]
name = "gpu"
default = true
nodes = "gpu[001-008]"
max_time = "72:00:00"

[[partitions]]
name = "cpu"
nodes = "cpu[001-064]"
max_time = "168:00:00"

[[nodes]]
names = "gpu[001-008]"
cpus = 128
memory_mb = 512000
gres = ["gpu:mi300x:8"]

[[nodes]]
names = "cpu[001-064]"
cpus = 256
memory_mb = 1024000
"#;

        let config = SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(config.cluster_name, "test-cluster");
        assert_eq!(config.partitions.len(), 2);
        assert_eq!(config.nodes.len(), 2);
        assert!(config.partitions[0].default);

        let parts = config.build_partitions();
        assert_eq!(parts[0].name, "gpu");
        assert_eq!(parts[0].max_time_minutes, Some(4320));
    }

    #[test]
    fn controller_config_defaults_for_new_fields() {
        let config = ControllerConfig::default();
        assert_eq!(config.heartbeat_timeout_secs, None);
    }

    #[test]
    fn controller_config_parses_heartbeat_timeout() {
        let toml = r#"
cluster_name = "test"

[controller]
heartbeat_timeout_secs = 120
"#;
        let config = SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(config.controller.heartbeat_timeout_secs, Some(120));
    }

    #[test]
    fn controller_config_absent_fields_are_none() {
        let toml = r#"
cluster_name = "test"

[controller]
listen_addr = "[::]:6817"
"#;
        let config = SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(config.controller.heartbeat_timeout_secs, None);
    }
}
