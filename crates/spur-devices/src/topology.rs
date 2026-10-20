// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// Device facts, independent of any scheduler policy or vendor's control API.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceTopology {
    pub pci_bdf: Option<String>,
    pub numa_node: Option<u32>,
    /// Node-local PCI host bridge name; not a globally unique fabric domain.
    pub pci_root: Option<String>,
    pub vram_bytes: Option<u64>,
    pub fabric: Option<FabricObservation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct FabricObservation {
    pub source: String,
    pub status: ObservationStatus,
    pub physical_domain_id: Option<String>,
    /// Scoped by the physical domain and the controller's administrative namespace.
    pub fabric_domain_id: Option<String>,
    /// Physical endpoint identity, shared by logical partitions of the same GPU.
    pub endpoint_id: Option<String>,
    pub physical_capacity: Option<u32>,
    pub fabric_capacity: Option<u32>,
    pub active_endpoints: Vec<String>,
    pub local_endpoints: Vec<String>,
    pub endpoint_state: Option<String>,
    pub transport: Option<String>,
    pub issues: Vec<ObservationIssue>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    #[default]
    Unknown,
    /// Attributes were read consistently; this does not assert fabric readiness.
    Observed,
    Incomplete,
    Inconsistent,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationIssue {
    pub field: String,
    pub kind: ObservationIssueKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationIssueKind {
    #[default]
    Unknown,
    Missing,
    Unreadable,
    Malformed,
    TooLarge,
    Conflict,
    Changed,
    Unavailable,
}

impl FabricObservation {
    pub fn add_issue(&mut self, field: &str, kind: ObservationIssueKind) {
        self.issues.push(ObservationIssue {
            field: field.into(),
            kind,
        });
        self.issues.sort();
        self.issues.dedup();
    }

    pub fn unavailable(source: &str, field: &str, kind: ObservationIssueKind) -> Self {
        let mut observation = Self {
            source: source.into(),
            status: ObservationStatus::Unavailable,
            ..Self::default()
        };
        observation.add_issue(field, kind);
        observation
    }
}
