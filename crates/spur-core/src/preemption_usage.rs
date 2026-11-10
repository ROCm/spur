// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lifetime outcomes of owner-directed preemptions, measured in victim runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PreemptionRelease {
    pub job_id: crate::job::JobId,
    pub run_attempt: u32,
    pub released: bool,
    pub observation_id: Option<u64>,
    pub unconfirmed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreemptionClaim {
    pub observation_id: u64,
    pub allocations: BTreeMap<String, crate::resource::ResourceAllocations>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnusedReason {
    StartedElsewhere,
    LaunchFailed,
    Cancelled,
    EndedOther,
}

impl UnusedReason {
    pub const ALL: [Self; 4] = [
        Self::StartedElsewhere,
        Self::LaunchFailed,
        Self::Cancelled,
        Self::EndedOther,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartedElsewhere => "started_elsewhere",
            Self::LaunchFailed => "launch_failed",
            Self::Cancelled => "cancelled",
            Self::EndedOther => "ended_other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreemptionInitiator {
    User,
    Admin,
    System,
    Unknown,
}

impl PreemptionInitiator {
    pub const ALL: [Self; 4] = [Self::User, Self::Admin, Self::System, Self::Unknown];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_cancel(cancel: Option<&crate::end_reason::CancelProvenance>) -> Self {
        match cancel.map(|c| c.origin.as_str()) {
            Some("User") => Self::User,
            Some("Admin") => Self::Admin,
            Some("System") => Self::System,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreemptionUsage {
    pub used: u64,
    pub unused: BTreeMap<UnusedReason, BTreeMap<PreemptionInitiator, u64>>,
    pub open: u64,
}

impl PreemptionUsage {
    pub fn unused_total(&self) -> u64 {
        self.unused
            .values()
            .flat_map(BTreeMap::values)
            .fold(0u64, |a, b| a.saturating_add(*b))
    }

    pub fn unused_count(&self, reason: UnusedReason, initiator: PreemptionInitiator) -> u64 {
        self.unused
            .get(&reason)
            .and_then(|counts| counts.get(&initiator))
            .copied()
            .unwrap_or(0)
    }

    pub fn open_one(&mut self) {
        self.open = self.open.saturating_add(1);
    }

    pub fn settle(&mut self, outcome: Option<(UnusedReason, PreemptionInitiator)>) {
        self.open = self.open.saturating_sub(1);
        match outcome {
            None => self.used = self.used.saturating_add(1),
            Some((reason, initiator)) => {
                let count = self
                    .unused
                    .entry(reason)
                    .or_default()
                    .entry(initiator)
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_empty_usage_payload_defaults_to_zero() {
        let usage: PreemptionUsage = serde_json::from_str("{}").expect("legacy payload");
        assert_eq!(usage, PreemptionUsage::default());
    }

    #[test]
    fn cancel_initiator_comes_from_provenance() {
        let cancel = crate::end_reason::CancelProvenance {
            origin: "Admin".into(),
            user: "operator".into(),
            uid: None,
        };
        assert_eq!(
            PreemptionInitiator::from_cancel(Some(&cancel)),
            PreemptionInitiator::Admin
        );
        assert_eq!(
            PreemptionInitiator::from_cancel(None),
            PreemptionInitiator::Unknown
        );
    }
}
