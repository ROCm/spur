// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_devices::topology as model;
pub use spur_devices::topology::{
    DeviceTopology, FabricObservation, ObservationIssue, ObservationIssueKind, ObservationStatus,
};
use spur_proto::proto as wire;

// Both registration and inventory responses use this mapping so the agent and
// controller cannot accidentally disagree about optional observation fields.
pub fn to_proto(t: &model::DeviceTopology) -> wire::DeviceTopology {
    wire::DeviceTopology {
        pci_bdf: t.pci_bdf.clone(),
        numa_node: t.numa_node,
        pci_root: t.pci_root.clone(),
        vram_bytes: t.vram_bytes,
        fabric: t.fabric.as_ref().map(|f| wire::FabricObservation {
            source: f.source.clone(),
            physical_domain_id: f.physical_domain_id.clone(),
            fabric_domain_id: f.fabric_domain_id.clone(),
            endpoint_id: f.endpoint_id.clone(),
            active_endpoints: f.active_endpoints.clone(),
            local_endpoints: f.local_endpoints.clone(),
            endpoint_state: f.endpoint_state.clone(),
            transport: f.transport.clone(),
            physical_capacity: f.physical_capacity,
            fabric_capacity: f.fabric_capacity,
            status: match f.status {
                model::ObservationStatus::Unknown => {
                    wire::ObservationStatus::ObservationUnknown as i32
                }
                model::ObservationStatus::Observed => {
                    wire::ObservationStatus::ObservationObserved as i32
                }
                model::ObservationStatus::Incomplete => {
                    wire::ObservationStatus::ObservationIncomplete as i32
                }
                model::ObservationStatus::Inconsistent => {
                    wire::ObservationStatus::ObservationInconsistent as i32
                }
                model::ObservationStatus::Unavailable => {
                    wire::ObservationStatus::ObservationUnavailable as i32
                }
            },
            issues: f
                .issues
                .iter()
                .map(|i| wire::ObservationIssue {
                    field: i.field.clone(),
                    kind: match i.kind {
                        model::ObservationIssueKind::Unknown => {
                            wire::ObservationIssueKind::ObservationIssueUnknown as i32
                        }
                        model::ObservationIssueKind::Missing => {
                            wire::ObservationIssueKind::ObservationIssueMissing as i32
                        }
                        model::ObservationIssueKind::Unreadable => {
                            wire::ObservationIssueKind::ObservationIssueUnreadable as i32
                        }
                        model::ObservationIssueKind::Malformed => {
                            wire::ObservationIssueKind::ObservationIssueMalformed as i32
                        }
                        model::ObservationIssueKind::TooLarge => {
                            wire::ObservationIssueKind::ObservationIssueTooLarge as i32
                        }
                        model::ObservationIssueKind::Conflict => {
                            wire::ObservationIssueKind::ObservationIssueConflict as i32
                        }
                        model::ObservationIssueKind::Changed => {
                            wire::ObservationIssueKind::ObservationIssueChanged as i32
                        }
                        model::ObservationIssueKind::Unavailable => {
                            wire::ObservationIssueKind::ObservationIssueUnavailable as i32
                        }
                    },
                })
                .collect(),
        }),
    }
}

pub fn from_proto(t: wire::DeviceTopology) -> model::DeviceTopology {
    model::DeviceTopology {
        pci_bdf: t.pci_bdf,
        numa_node: t.numa_node,
        pci_root: t.pci_root,
        vram_bytes: t.vram_bytes,
        fabric: t.fabric.map(|f| model::FabricObservation {
            source: f.source,
            physical_domain_id: f.physical_domain_id,
            fabric_domain_id: f.fabric_domain_id,
            endpoint_id: f.endpoint_id,
            active_endpoints: f.active_endpoints,
            local_endpoints: f.local_endpoints,
            endpoint_state: f.endpoint_state,
            transport: f.transport,
            physical_capacity: f.physical_capacity,
            fabric_capacity: f.fabric_capacity,
            status: match wire::ObservationStatus::try_from(f.status) {
                Ok(wire::ObservationStatus::ObservationObserved) => {
                    model::ObservationStatus::Observed
                }
                Ok(wire::ObservationStatus::ObservationIncomplete) => {
                    model::ObservationStatus::Incomplete
                }
                Ok(wire::ObservationStatus::ObservationInconsistent) => {
                    model::ObservationStatus::Inconsistent
                }
                Ok(wire::ObservationStatus::ObservationUnavailable) => {
                    model::ObservationStatus::Unavailable
                }
                _ => model::ObservationStatus::Unknown,
            },
            issues: f
                .issues
                .into_iter()
                .map(|i| model::ObservationIssue {
                    field: i.field,
                    kind: match wire::ObservationIssueKind::try_from(i.kind) {
                        Ok(wire::ObservationIssueKind::ObservationIssueMissing) => {
                            model::ObservationIssueKind::Missing
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueUnreadable) => {
                            model::ObservationIssueKind::Unreadable
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueMalformed) => {
                            model::ObservationIssueKind::Malformed
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueTooLarge) => {
                            model::ObservationIssueKind::TooLarge
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueConflict) => {
                            model::ObservationIssueKind::Conflict
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueChanged) => {
                            model::ObservationIssueKind::Changed
                        }
                        Ok(wire::ObservationIssueKind::ObservationIssueUnavailable) => {
                            model::ObservationIssueKind::Unavailable
                        }
                        _ => model::ObservationIssueKind::Unknown,
                    },
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_facts_and_diagnostics_round_trip() {
        let topology = model::DeviceTopology {
            pci_bdf: Some("0001:04:00.0".into()),
            numa_node: None,
            pci_root: Some("pci0001:00".into()),
            vram_bytes: Some(309_237_645_312),
            fabric: Some(model::FabricObservation {
                source: "amd-ualink".into(),
                status: model::ObservationStatus::Incomplete,
                physical_domain_id: Some("00000000-0000-4000-8000-000000000001".into()),
                fabric_domain_id: Some("0".into()),
                endpoint_id: Some("63".into()),
                physical_capacity: Some(72),
                fabric_capacity: Some(4),
                active_endpoints: vec!["60".into(), "61".into(), "62".into(), "63".into()],
                local_endpoints: vec!["63".into()],
                endpoint_state: Some("active".into()),
                transport: Some("UALoE".into()),
                issues: vec![model::ObservationIssue {
                    field: "vpod_id".into(),
                    kind: model::ObservationIssueKind::Missing,
                }],
            }),
        };
        assert_eq!(from_proto(to_proto(&topology)), topology);
        assert_eq!(
            from_proto(wire::DeviceTopology::default()),
            model::DeviceTopology::default()
        );
    }

    #[test]
    fn unknown_wire_states_do_not_become_observed() {
        let model = from_proto(wire::DeviceTopology {
            fabric: Some(wire::FabricObservation {
                status: 99,
                issues: vec![wire::ObservationIssue {
                    field: "new_field".into(),
                    kind: 99,
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let observation = model.fabric.unwrap();
        assert_eq!(observation.status, model::ObservationStatus::Unknown);
        assert_eq!(
            observation.issues[0].kind,
            model::ObservationIssueKind::Unknown
        );
    }
}
