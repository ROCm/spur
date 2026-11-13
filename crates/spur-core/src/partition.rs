// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::job::{JobSpec, PendingReason};

/// Partition (queue) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    pub name: String,
    pub state: PartitionState,
    pub is_default: bool,

    /// Nodes belonging to this partition (hostlist pattern).
    pub nodes: String,

    /// Label selector: node joins this partition if ALL key-value pairs match.
    #[serde(default)]
    pub selector: HashMap<String, String>,

    /// Limits
    pub max_time_minutes: Option<u32>,
    pub default_time_minutes: Option<u32>,
    pub max_nodes: Option<u32>,
    pub min_nodes: u32,

    /// Access control
    pub allow_root: bool,
    pub exclusive_user: bool,
    pub allow_accounts: Vec<String>,
    pub allow_groups: Vec<String>,
    pub allow_qos: Vec<String>,
    pub deny_accounts: Vec<String>,
    pub deny_qos: Vec<String>,

    /// Scheduling. `None` means nothing was configured at this scope; it
    /// resolves the same as `Off`.
    #[serde(default)]
    pub preempt_mode: Option<PreemptMode>,
    pub priority_tier: u32,
    /// Partition-level override for the minimum seconds a job must have been
    /// running before it is eligible for preemption. `None` defers to the
    /// cluster-wide `preempt_exempt_time` in `SchedulerConfig`.
    #[serde(default)]
    pub preempt_exempt_time: Option<u32>,
    /// Partition-level preemption grace period in seconds. `None` means unset;
    /// `Some(0)` means no grace. Used when the victim's QOS leaves its grace unset.
    #[serde(default)]
    pub grace_time: Option<u32>,

    /// Combined per-node cap per TRES key on this partition's jobs; the config's
    /// `max_cpus_per_node` folds into `cpu`. Empty is unlimited; a `0` limit forbids it.
    #[serde(default)]
    pub max_tres_per_node: BTreeMap<String, u64>,
    /// Per-job memory policy (MiB), Slurm's `MaxMemPerNode`/`MaxMemPerCPU`/
    /// `DefMemPer*`. Never accumulated across jobs.
    #[serde(default)]
    pub max_mem_per_node: Option<u64>,
    #[serde(default)]
    pub max_mem_per_cpu: Option<u64>,
    #[serde(default)]
    pub def_mem_per_node: Option<u64>,
    #[serde(default)]
    pub def_mem_per_cpu: Option<u64>,
    #[serde(default)]
    pub def_mem_per_gpu: Option<u64>,
}

impl Partition {
    /// Rejects uncharged cap keys and conflicting per-job memory knobs; shared by config
    /// load and the partition API so both enforce the same rules.
    pub fn validate_resource_limits(&self) -> Result<(), String> {
        if let Some(key) = self
            .max_tres_per_node
            .keys()
            .find(|key| !is_valid_partition_tres_key(key))
        {
            return Err(format!(
                "max_tres_per_node: unknown key {key:?} (expected cpu, mem, gres/gpu, or gres/gpu:<type>)"
            ));
        }
        if let (Some(per_node), Some(per_cpu)) = (self.max_mem_per_node, self.max_mem_per_cpu) {
            return Err(format!(
                "max_mem_per_node = {per_node} conflicts with max_mem_per_cpu = {per_cpu} (set only one)"
            ));
        }
        let def_mem: Vec<String> = [
            ("def_mem_per_node", self.def_mem_per_node),
            ("def_mem_per_cpu", self.def_mem_per_cpu),
            ("def_mem_per_gpu", self.def_mem_per_gpu),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| format!("{name} = {v}")))
        .collect();
        if def_mem.len() > 1 {
            return Err(format!("{} (set at most one)", def_mem.join(", ")));
        }
        Ok(())
    }

    /// Whether a wall-time request fits `MaxTime`; no request or no limit fits.
    pub fn time_allows(&self, time_limit: Option<&chrono::Duration>) -> bool {
        match (self.max_time_minutes, time_limit) {
            (Some(max_mins), Some(tl)) => tl.num_seconds() <= i64::from(max_mins) * 60,
            _ => true,
        }
    }

    /// Why this partition can never run the job (static limits), or `None` if it can:
    /// node count and `max_mem_per_node` report `PartitionConfig`, wall time `PartitionTimeLimit`.
    pub fn limit_block(&self, spec: &JobSpec) -> Option<PendingReason> {
        // A job never runs on fewer than one node, whatever it asked for.
        let nodes = spec.num_nodes.max(1);
        if self.max_nodes.is_some_and(|max| nodes > max) {
            return Some(PendingReason::PartitionConfig);
        }
        if nodes < self.min_nodes {
            return Some(PendingReason::PartitionConfig);
        }
        if !self.time_allows(spec.time_limit.as_ref()) {
            return Some(PendingReason::PartitionTimeLimit);
        }
        if self.max_mem_per_node_exceeded(spec).is_some() {
            return Some(PendingReason::PartitionConfig);
        }
        None
    }

    /// Whether the job may start here now: Up and not ruled out by static limits. Shared
    /// by the controller's pending-reason gate and the scheduler's partition choice.
    pub fn admits(&self, spec: &JobSpec) -> bool {
        self.state == PartitionState::Up && self.limit_block(spec).is_none()
    }
}

pub const TRES_CPU: &str = "cpu";
pub const TRES_MEM: &str = "mem";
pub const TRES_GPU: &str = "gres/gpu";

/// Cap key for one GPU type, e.g. `gres/gpu:a100`.
pub fn typed_gpu_tres_key(gpu_type: &str) -> String {
    format!("{TRES_GPU}:{gpu_type}")
}

/// GPU type named by a `gres/gpu:<type>` key; `None` for any other key.
pub fn tres_key_gpu_type(key: &str) -> Option<&str> {
    key.strip_prefix(TRES_GPU)?.strip_prefix(':')
}

/// Whether `key` is a chargeable cap key (`cpu`, `mem`, `gres/gpu[:<type>]`); anything
/// else is never charged, so a typo must be rejected rather than leave the partition uncapped.
pub fn is_valid_partition_tres_key(key: &str) -> bool {
    if matches!(key, TRES_CPU | TRES_MEM | TRES_GPU) {
        return true;
    }
    tres_key_gpu_type(key).is_some_and(|ty| {
        !ty.is_empty()
            && !ty
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, ':' | ',' | '='))
    })
}

/// Order in which over-limit keys are reported: `cpu`, then `mem`, then GRES.
pub fn tres_key_report_rank(key: &str) -> u8 {
    match key {
        TRES_CPU => 0,
        TRES_MEM => 1,
        _ => 2,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionState {
    Up,
    Down,
    Drain,
    Inactive,
}

impl PartitionState {
    pub fn display(&self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Drain => "drain",
            Self::Inactive => "inactive",
        }
    }
}

impl std::fmt::Display for PartitionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// Selects the preemption engine. Mirrors Slurm's `PreemptType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PreemptType {
    /// Preemption is disabled. `off` is accepted as a synonym.
    #[default]
    #[serde(alias = "off")]
    None,
    /// A pending job may preempt a running job only when the pending job's QOS
    /// lists the running job's QOS in its `preempt` allow-list and outranks it.
    QosPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreemptMode {
    #[default]
    Off,
    Cancel,
    Requeue,
    Suspend,
}

impl PreemptMode {
    /// Ranking used to pick a single mode when a job spans multiple partitions
    /// (comma-separated OR list). Higher = more disruptive to the preempted
    /// job. A job occupying a node in a partition that permits a harder
    /// preemption can be preempted that hard, so the most aggressive matched
    /// mode wins — maximizing the scheduler's ability to free resources.
    pub fn aggressiveness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Suspend => 1,
            Self::Requeue => 2,
            Self::Cancel => 3,
        }
    }
}

/// Parse a configured partition preempt mode. An empty value means "not
/// configured", which resolves the same as `off`. Unrecognized values are an
/// error so a typo cannot silently disable preemption.
pub fn parse_preempt_mode(value: &str) -> Result<Option<PreemptMode>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "none" => Ok(None),
        "off" => Ok(Some(PreemptMode::Off)),
        "cancel" => Ok(Some(PreemptMode::Cancel)),
        "requeue" => Ok(Some(PreemptMode::Requeue)),
        "suspend" => Ok(Some(PreemptMode::Suspend)),
        other => Err(format!(
            "unknown preempt_mode '{other}'; expected none, off, cancel, requeue, or suspend"
        )),
    }
}

/// Wire form of a configured preempt mode; empty when unset.
pub fn preempt_mode_str(mode: Option<PreemptMode>) -> &'static str {
    match mode {
        None => "",
        Some(PreemptMode::Off) => "OFF",
        Some(PreemptMode::Cancel) => "CANCEL",
        Some(PreemptMode::Requeue) => "REQUEUE",
        Some(PreemptMode::Suspend) => "SUSPEND",
    }
}

/// Non-empty names from a comma-separated partition OR-list.
pub fn requested_partition_names(spec: Option<&str>) -> impl Iterator<Item = &str> {
    spec.into_iter()
        .flat_map(|spec| spec.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Partitions a job requests, resolved by name.
pub fn matched_partitions<'a>(
    spec: Option<&str>,
    partitions: &'a [Partition],
) -> Vec<&'a Partition> {
    requested_partition_names(spec)
        .filter_map(|req| partitions.iter().find(|p| p.name == req))
        .collect()
}

/// Highest `priority_tier` among a job's requested partitions (see
/// `matched_partitions`), or 1 if none match.
pub fn max_priority_tier(spec: Option<&str>, partitions: &[Partition]) -> u32 {
    matched_partitions(spec, partitions)
        .into_iter()
        .map(|p| p.priority_tier)
        .max()
        .unwrap_or(1)
}

impl Default for Partition {
    fn default() -> Self {
        Self {
            name: String::new(),
            state: PartitionState::Up,
            is_default: false,
            nodes: String::new(),
            selector: HashMap::new(),
            max_time_minutes: None,
            default_time_minutes: None,
            max_nodes: None,
            min_nodes: 1,
            allow_root: true,
            exclusive_user: false,
            allow_accounts: Vec::new(),
            allow_groups: Vec::new(),
            allow_qos: Vec::new(),
            deny_accounts: Vec::new(),
            deny_qos: Vec::new(),
            preempt_mode: None,
            priority_tier: 1,
            preempt_exempt_time: None,
            grace_time: None,
            max_tres_per_node: BTreeMap::new(),
            max_mem_per_node: None,
            max_mem_per_cpu: None,
            def_mem_per_node: None,
            def_mem_per_cpu: None,
            def_mem_per_gpu: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_requires_up_and_every_static_limit() {
        let spec = JobSpec {
            num_nodes: 2,
            time_limit: Some(chrono::Duration::minutes(60)),
            memory_per_node_mb: Some(2048),
            ..Default::default()
        };
        assert!(Partition::default().admits(&spec));
        let ruled_out = [
            Partition {
                state: PartitionState::Down,
                ..Default::default()
            },
            Partition {
                state: PartitionState::Drain,
                ..Default::default()
            },
            Partition {
                max_time_minutes: Some(59),
                ..Default::default()
            },
            Partition {
                max_nodes: Some(1),
                ..Default::default()
            },
            Partition {
                min_nodes: 3,
                ..Default::default()
            },
            Partition {
                max_mem_per_node: Some(1024),
                ..Default::default()
            },
        ];
        for partition in ruled_out {
            assert!(!partition.admits(&spec), "{partition:?}");
        }
        assert_eq!(
            Partition {
                max_time_minutes: Some(59),
                ..Default::default()
            }
            .limit_block(&spec),
            Some(PendingReason::PartitionTimeLimit)
        );
    }

    #[test]
    fn preempt_mode_parses_unset_off_and_actions() {
        assert_eq!(parse_preempt_mode(""), Ok(None));
        assert_eq!(parse_preempt_mode("  "), Ok(None));
        assert_eq!(parse_preempt_mode("none"), Ok(None));
        assert_eq!(parse_preempt_mode("off"), Ok(Some(PreemptMode::Off)));
        assert_eq!(parse_preempt_mode("OFF"), Ok(Some(PreemptMode::Off)));
        assert_eq!(parse_preempt_mode("Cancel"), Ok(Some(PreemptMode::Cancel)));
        assert!(parse_preempt_mode("cancle").is_err());
    }

    #[test]
    fn preempt_mode_str_round_trips() {
        for mode in [
            None,
            Some(PreemptMode::Off),
            Some(PreemptMode::Cancel),
            Some(PreemptMode::Requeue),
            Some(PreemptMode::Suspend),
        ] {
            assert_eq!(parse_preempt_mode(preempt_mode_str(mode)), Ok(mode));
        }
    }

    #[test]
    fn preempt_mode_aggressiveness_orders_cancel_over_requeue_over_suspend_over_off() {
        assert!(PreemptMode::Cancel.aggressiveness() > PreemptMode::Requeue.aggressiveness());
        assert!(PreemptMode::Requeue.aggressiveness() > PreemptMode::Suspend.aggressiveness());
        assert!(PreemptMode::Suspend.aggressiveness() > PreemptMode::Off.aggressiveness());
    }

    // Frozen pre-grace_time partition as stored in a Raft snapshot; it must still
    // load or a new controller cannot restore an old snapshot. Never regenerate.
    #[test]
    fn partition_snapshot_without_grace_time_loads() {
        const PARTITION_PRE_GRACE_TIME: &str = r#"{"name":"gpu","state":"Up","is_default":true,"nodes":"ALL","selector":{},"max_time_minutes":1440,"default_time_minutes":10,"max_nodes":null,"min_nodes":1,"allow_root":true,"exclusive_user":false,"allow_accounts":[],"allow_groups":[],"allow_qos":[],"deny_accounts":[],"deny_qos":[],"preempt_mode":"Cancel","priority_tier":1,"preempt_exempt_time":300}"#;
        let part: Partition = serde_json::from_str(PARTITION_PRE_GRACE_TIME)
            .expect("a pre-grace_time partition must load; a new field needs #[serde(default)]");
        assert_eq!(part.name, "gpu");
        assert_eq!(part.preempt_exempt_time, Some(300));
        assert_eq!(part.grace_time, None);
    }

    fn partition_with_tier(name: &str, priority_tier: u32) -> Partition {
        Partition {
            name: name.into(),
            priority_tier,
            ..Default::default()
        }
    }

    #[test]
    fn max_priority_tier_picks_highest_among_matched_partitions() {
        let parts = vec![
            partition_with_tier("low", 1),
            partition_with_tier("high", 9),
        ];
        assert_eq!(max_priority_tier(Some("low,high"), &parts), 9);
        assert_eq!(max_priority_tier(Some("high, low"), &parts), 9);
    }

    #[test]
    fn requested_partition_names_trims_and_ignores_empty_entries() {
        assert_eq!(
            requested_partition_names(Some(" gpu, ,cpu,, ")).collect::<Vec<_>>(),
            vec!["gpu", "cpu"]
        );
        assert!(requested_partition_names(None).next().is_none());
    }

    #[test]
    fn max_priority_tier_defaults_to_one_when_unset_or_unmatched() {
        let parts = vec![partition_with_tier("gpu", 5)];
        assert_eq!(max_priority_tier(None, &parts), 1);
        assert_eq!(max_priority_tier(Some("nope"), &parts), 1);
    }

    #[test]
    fn typed_gpu_key_round_trips_through_its_type() {
        let key = typed_gpu_tres_key("mi300x");
        assert_eq!(key, "gres/gpu:mi300x");
        assert_eq!(tres_key_gpu_type(&key), Some("mi300x"));
        assert_eq!(tres_key_gpu_type(TRES_GPU), None);
        assert_eq!(tres_key_gpu_type(TRES_CPU), None);
        assert_eq!(tres_key_gpu_type("gres/gpux:a100"), None);
    }

    #[test]
    fn partition_tres_keys_accept_only_chargeable_shapes() {
        for key in ["cpu", "mem", "gres/gpu", "gres/gpu:a100", "gres/gpu:MI300X"] {
            assert!(is_valid_partition_tres_key(key), "{key} must be valid");
        }
        for key in [
            "",
            "cpus",
            "CPU",
            "memory",
            "gres",
            "gres/",
            "gres/gpu:",
            "gres/gpu::a100",
            "gres/gpu:a 100",
            "gres/gpu:a=1",
            "gres/gpu:a,b",
            "gres/nic",
            "disk",
            "node",
        ] {
            assert!(!is_valid_partition_tres_key(key), "{key:?} must be invalid");
        }
    }

    #[test]
    fn tres_report_rank_orders_cpu_then_mem_then_gres() {
        assert!(tres_key_report_rank(TRES_CPU) < tres_key_report_rank(TRES_MEM));
        assert!(tres_key_report_rank(TRES_MEM) < tres_key_report_rank(TRES_GPU));
        assert_eq!(
            tres_key_report_rank(TRES_GPU),
            tres_key_report_rank("gres/gpu:a100")
        );
    }

    #[test]
    fn partition_resource_limits_validate_independently_of_config() {
        assert_eq!(Partition::default().validate_resource_limits(), Ok(()));

        let bad_key = Partition {
            max_tres_per_node: BTreeMap::from([("cpus".to_string(), 4)]),
            ..Default::default()
        };
        assert!(bad_key
            .validate_resource_limits()
            .unwrap_err()
            .contains("\"cpus\""));

        let both_max = Partition {
            max_mem_per_node: Some(1),
            max_mem_per_cpu: Some(1),
            ..Default::default()
        };
        assert!(both_max.validate_resource_limits().is_err());

        let two_defaults = Partition {
            def_mem_per_cpu: Some(1),
            def_mem_per_gpu: Some(1),
            ..Default::default()
        };
        assert!(two_defaults.validate_resource_limits().is_err());

        let valid = Partition {
            max_tres_per_node: BTreeMap::from([
                ("cpu".to_string(), 0),
                ("gres/gpu:a100".to_string(), 2),
            ]),
            max_mem_per_cpu: Some(2048),
            def_mem_per_node: Some(4096),
            ..Default::default()
        };
        assert_eq!(valid.validate_resource_limits(), Ok(()));
    }

    #[test]
    fn a_partition_snapshot_without_resource_caps_still_deserializes() {
        let mut value = serde_json::to_value(Partition::default()).expect("serialize partition");
        let fields = value
            .as_object_mut()
            .expect("partition serializes as an object");
        for field in [
            "max_tres_per_node",
            "max_mem_per_node",
            "max_mem_per_cpu",
            "def_mem_per_node",
            "def_mem_per_cpu",
            "def_mem_per_gpu",
        ] {
            assert!(fields.remove(field).is_some(), "{field} must serialize");
        }
        let back: Partition = serde_json::from_value(value).expect("deserialize partition");
        assert!(back.max_tres_per_node.is_empty());
        assert_eq!(back.max_mem_per_cpu, None);
        assert_eq!(back.def_mem_per_gpu, None);
    }

    #[test]
    fn partition_resource_caps_round_trip_through_serde() {
        let p = Partition {
            max_tres_per_node: BTreeMap::from([("mem".to_string(), 131072)]),
            max_mem_per_node: Some(65536),
            def_mem_per_cpu: Some(1024),
            ..Default::default()
        };
        let back: Partition = serde_json::from_str(&serde_json::to_string(&p).expect("serialize"))
            .expect("deserialize");
        assert_eq!(back.max_tres_per_node, p.max_tres_per_node);
        assert_eq!(back.max_mem_per_node, Some(65536));
        assert_eq!(back.def_mem_per_cpu, Some(1024));
    }
}
