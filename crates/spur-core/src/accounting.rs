// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Accounting data models: accounts, users, QOS, associations, TRES.

use std::collections::HashMap;
use std::convert::Infallible;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Slurm's `INFINITE` sentinel for numeric limits carried over the wire as
/// `uint32`. It marks "clear to no limit" (stored as SQL `NULL`), distinct
/// from a literal `0`, which means "block all". See `nullable_limit` and the
/// sacctmgr `-1` keyword.
pub const INFINITE: u32 = u32::MAX;

/// Convert a stored nullable limit (`Option<i32>` from the DB) into the
/// in-memory `Option<u32>`: SQL `NULL` (`None`) is "no limit", a literal `0`
/// survives as `Some(0)` ("block all"), and a stray negative (predates the
/// sentinel flip) is treated as unset.
pub fn limit_from_db(v: Option<i32>) -> Option<u32> {
    v.filter(|&x| x >= 0).map(|x| x as u32)
}

/// Convert a stored nullable limit into its proto `uint32`: NULL (and any
/// stray negative) becomes the `INFINITE` sentinel so the CLI can tell "no
/// limit" apart from a literal `0`; a stored value is emitted as-is.
pub fn limit_to_wire(v: Option<i32>) -> u32 {
    limit_from_db(v).unwrap_or(INFINITE)
}

/// Trackable RESource types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TresType {
    Cpu,
    Memory, // MB
    Energy, // Joules
    Node,
    Gpu,
    Billing, // Weighted composite
}

impl TresType {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "mem",
            Self::Energy => "energy",
            Self::Node => "node",
            Self::Gpu => "gres/gpu",
            Self::Billing => "billing",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "cpu" => Some(Self::Cpu),
            "mem" | "memory" => Some(Self::Memory),
            "energy" => Some(Self::Energy),
            "node" => Some(Self::Node),
            "gres/gpu" | "gpu" => Some(Self::Gpu),
            "billing" => Some(Self::Billing),
            _ => None,
        }
    }
}

/// TRES usage/allocation record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TresRecord {
    pub values: HashMap<TresType, u64>,
}

impl TresRecord {
    pub fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    pub fn set(&mut self, tres: TresType, value: u64) {
        self.values.insert(tres, value);
    }

    pub fn get(&self, tres: TresType) -> u64 {
        self.values.get(&tres).copied().unwrap_or(0)
    }

    pub fn add(&mut self, other: &TresRecord) {
        for (k, v) in &other.values {
            *self.values.entry(*k).or_insert(0) += v;
        }
    }

    /// Format as "cpu=N,mem=N,gres/gpu=N" string.
    pub fn format(&self) -> String {
        let mut parts: Vec<String> = self
            .values
            .iter()
            .filter(|(_, v)| **v > 0)
            .map(|(k, v)| format!("{}={}", k.name(), v))
            .collect();
        parts.sort();
        parts.join(",")
    }

    /// Parse from a "cpu=N,mem=N" string. Errors on any token that isn't a
    /// known TRES type with a plain integer value (e.g. Slurm's unit-suffixed
    /// `mem=1G` is rejected, not silently dropped) so bad admin input never
    /// turns into a silent no-op limit.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut rec = Self::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (key, val) = part
                .split_once('=')
                .ok_or_else(|| format!("invalid TRES token '{part}': expected key=value"))?;
            let tres = TresType::from_name(key.trim())
                .ok_or_else(|| format!("unknown TRES type '{}'", key.trim()))?;
            let value = val.trim().parse::<u64>().map_err(|_| {
                format!("invalid TRES value for '{}': '{}'", key.trim(), val.trim())
            })?;
            rec.set(tres, value);
        }
        Ok(rec)
    }
}

/// An account in the accounting hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub description: String,
    pub organization: String,
    pub parent: Option<String>,
    pub fairshare_weight: u32,
    /// Resource limits for all jobs under this account.
    pub limits: AccountLimits,
}

/// Per-account resource limits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    /// Max running jobs for a single user within this account.
    pub max_running_jobs: Option<u32>,
    /// Max submitted (pending + running) jobs for a single user within this account.
    pub max_submit_jobs: Option<u32>,
    /// Max submitted (pending + running) jobs across all users in this account.
    #[serde(default)]
    pub grp_submit_jobs: Option<u32>,
    /// Max TRES per job.
    pub max_tres_per_job: Option<TresRecord>,
    /// Max total TRES across all running jobs in this account, summed over every user.
    pub grp_tres: Option<TresRecord>,
    /// Max wall time per job (minutes).
    pub max_wall_minutes: Option<u32>,
}

/// Quality of Service definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Qos {
    pub name: String,
    pub description: String,
    pub priority: i32,
    pub preempt_mode: QosPreemptMode,
    pub limits: QosLimits,
    /// Usage factor — multiplier for fair-share usage accounting.
    /// 0.0 = don't charge, 1.0 = normal, 2.0 = double charge.
    pub usage_factor: f64,
    /// QOS names that jobs in this QOS are allowed to preempt. Only enforced
    /// when `scheduler.preempt_type = qos_priority`. Empty means this QOS may
    /// not preempt any other QOS under that mode. Mirrors Slurm's `Preempt=`
    /// field on a QOS.
    #[serde(default)]
    pub preempt: Vec<String>,
    /// When set, a stand-alone resource/wall limit breach is denied at
    /// submission instead of leaving the job pending. Mirrors Slurm's QOS
    /// `DenyOnLimit` flag; the submit-count family always denies regardless.
    #[serde(default)]
    pub deny_on_limit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QosPreemptMode {
    #[default]
    Off,
    Cancel,
    Requeue,
    Suspend,
}

impl FromStr for QosPreemptMode {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Infallible> {
        Ok(match s.to_lowercase().as_str() {
            "cancel" => Self::Cancel,
            "requeue" => Self::Requeue,
            "suspend" => Self::Suspend,
            _ => Self::Off,
        })
    }
}

/// Per-QOS resource limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QosLimits {
    pub max_jobs_per_user: Option<u32>,
    pub max_submit_jobs_per_user: Option<u32>,
    /// Max submitted (pending + running) jobs per account within this QOS.
    #[serde(default)]
    pub max_submit_jobs_per_account: Option<u32>,
    /// Max submitted (pending + running) jobs across all users in this QOS.
    #[serde(default)]
    pub grp_submit_jobs: Option<u32>,
    pub max_tres_per_job: Option<TresRecord>,
    pub max_tres_per_user: Option<TresRecord>,
    pub grp_tres: Option<TresRecord>,
    pub max_wall_minutes: Option<u32>,
    pub grp_wall_minutes: Option<u32>,
    /// Per-QOS override for the minimum seconds a job must have been running
    /// before it is eligible for preemption. Overrides the partition value,
    /// which in turn overrides the cluster-wide `preempt_exempt_time`. `None`
    /// defers to the next level.
    #[serde(default)]
    pub preempt_exempt_time: Option<u32>,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            organization: String::new(),
            parent: None,
            fairshare_weight: 1,
            limits: AccountLimits::default(),
        }
    }
}

impl Default for Qos {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            priority: 0,
            preempt_mode: QosPreemptMode::Off,
            limits: QosLimits::default(),
            usage_factor: 1.0,
            preempt: Vec::new(),
            deny_on_limit: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tres_format_parse() {
        let mut rec = TresRecord::new();
        rec.set(TresType::Cpu, 64);
        rec.set(TresType::Memory, 256000);
        rec.set(TresType::Gpu, 8);

        let formatted = rec.format();
        assert!(formatted.contains("cpu=64"));
        assert!(formatted.contains("gres/gpu=8"));

        let parsed = TresRecord::parse(&formatted).unwrap();
        assert_eq!(parsed.get(TresType::Cpu), 64);
        assert_eq!(parsed.get(TresType::Gpu), 8);
    }

    #[test]
    fn test_tres_parse_rejects_unknown_type() {
        assert!(TresRecord::parse("bogus=5").is_err());
    }

    #[test]
    fn limit_from_db_maps_sentinels() {
        assert_eq!(limit_from_db(None), None);
        // A literal 0 is a real value ("block all"), not a clear.
        assert_eq!(limit_from_db(Some(0)), Some(0));
        assert_eq!(limit_from_db(Some(5)), Some(5));
        // A stray negative predates the sentinel flip; treat as unset.
        assert_eq!(limit_from_db(Some(-1)), None);
    }

    #[test]
    fn limit_to_wire_maps_null_and_values() {
        assert_eq!(limit_to_wire(None), INFINITE);
        assert_eq!(limit_to_wire(Some(-1)), INFINITE);
        assert_eq!(limit_to_wire(Some(0)), 0);
        assert_eq!(limit_to_wire(Some(7)), 7);
    }

    #[test]
    fn test_tres_parse_rejects_unit_suffixed_value() {
        // K/M/G unit suffixes (Slurm's `mem=1G`) are out of scope for this
        // parser; they must fail loudly rather than being silently dropped.
        assert!(TresRecord::parse("mem=1G").is_err());
    }

    #[test]
    fn test_tres_parse_rejects_malformed_token() {
        assert!(TresRecord::parse("cpu").is_err());
    }

    #[test]
    fn test_tres_parse_empty_string_is_empty_record() {
        let rec = TresRecord::parse("").unwrap();
        assert_eq!(rec.values.len(), 0);
    }

    #[test]
    fn test_tres_add() {
        let mut a = TresRecord::new();
        a.set(TresType::Cpu, 10);
        let mut b = TresRecord::new();
        b.set(TresType::Cpu, 20);
        b.set(TresType::Gpu, 4);
        a.add(&b);
        assert_eq!(a.get(TresType::Cpu), 30);
        assert_eq!(a.get(TresType::Gpu), 4);
    }

    #[test]
    fn test_qos_preempt_mode() {
        assert_eq!(
            "cancel".parse::<QosPreemptMode>().unwrap(),
            QosPreemptMode::Cancel
        );
        assert_eq!(
            "off".parse::<QosPreemptMode>().unwrap(),
            QosPreemptMode::Off
        );
        assert_eq!(
            "unknown".parse::<QosPreemptMode>().unwrap(),
            QosPreemptMode::Off
        );
    }

    #[test]
    fn test_tres_record_set_get() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Cpu, 64);
        tres.set(TresType::Memory, 256_000);
        assert_eq!(tres.get(TresType::Cpu), 64);
        assert_eq!(tres.get(TresType::Memory), 256_000);
        assert_eq!(tres.get(TresType::Gpu), 0); // default
    }

    #[test]
    fn test_qos_limits_default() {
        let limits = QosLimits::default();
        assert!(limits.max_jobs_per_user.is_none());
        assert!(limits.max_wall_minutes.is_none());
    }

    #[test]
    fn test_qos_default() {
        let qos = Qos::default();
        assert_eq!(qos.priority, 0);
        assert_eq!(qos.usage_factor, 1.0);
    }

    #[test]
    fn test_account_limits_default() {
        let limits = AccountLimits::default();
        assert!(limits.max_running_jobs.is_none());
    }
}
