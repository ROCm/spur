// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A node's allocatable devices of every kind (GPUs, NICs, ...), in one shape.

use serde::{Deserialize, Serialize};
pub use spur_devices::topology::DeviceTopology;
pub use spur_devices::DeviceSelection;

use crate::resource::GpuLinkType;

pub const GPU_GRES: &str = "gpu";

/// Whether a gres is handed out as specific devices rather than counted from a
/// pool. Controller and agent must agree, so both call this. GPUs always are: a
/// node without any offers none, not a countable pool.
pub fn allocates_by_id(gres_name: &str, has_devices_of_name: bool) -> bool {
    gres_name == GPU_GRES || has_devices_of_name
}

/// The gres a countable pool key (`name` or `name:type`) belongs to.
pub fn pool_gres_name(key: &str) -> &str {
    key.split_once(':').map_or(key, |(name, _)| name)
}

/// The aliases and defaults let a deprecated GPU-only entry (`GpuResource`) from
/// the Raft log, a snapshot or an agent's state files read as a GPU device.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Device {
    /// Absent only from those GPU-only entries, so it defaults to `gpu`.
    #[serde(default = "gpu_gres")]
    pub gres_name: String,
    /// Empty when the device is untyped.
    #[serde(default, alias = "gpu_type")]
    pub device_type: String,
    /// Allocation identity, unique per `gres_name` on a node; survives a
    /// mid-set removal, unlike `index`. Ids of file-backed GRES devices are
    /// positional, so editing their `file` list renumbers them.
    #[serde(default)]
    pub stable_id: u64,
    /// Positional index the vendor runtime numbers the device by.
    #[serde(default, alias = "device_id")]
    pub index: u32,
    #[serde(default)]
    pub topology: Option<DeviceTopology>,
    #[serde(default)]
    pub numa_node: Option<u32>,
    /// OS CPU IDs near the device, from `[[devices.gres]] cores=`; empty means
    /// the CPUs of `numa_node`.
    #[serde(default)]
    pub affinity_cpus: Vec<u32>,
}

fn gpu_gres() -> String {
    GPU_GRES.to_string()
}

impl Device {
    pub fn new(gres_name: &str, device_type: &str, stable_id: u64, index: u32) -> Self {
        Self {
            gres_name: gres_name.to_string(),
            device_type: device_type.to_string(),
            stable_id,
            index,
            topology: None,
            numa_node: None,
            affinity_cpus: Vec::new(),
        }
    }

    pub fn gpu(stable_id: u64, device_type: &str, index: u32) -> Self {
        Self::new(GPU_GRES, device_type, stable_id, index)
    }

    pub fn is_gpu(&self) -> bool {
        self.gres_name == GPU_GRES
    }
}

/// The deprecated GPU-only inventory entry, still written next to `devices`
/// for one release so a rolled-back controller can replay the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LegacyGpu {
    device_id: u32,
    gpu_type: String,
    memory_mb: u64,
    peer_gpus: Vec<u32>,
    link_type: GpuLinkType,
    stable_id: u64,
    topology: Option<DeviceTopology>,
    #[serde(default)]
    numa_node: Option<u32>,
    #[serde(default)]
    affinity_cpus: Vec<u32>,
}

impl From<&Device> for LegacyGpu {
    fn from(device: &Device) -> Self {
        let vram_bytes = device.topology.as_ref().and_then(|t| t.vram_bytes);
        Self {
            device_id: device.index,
            gpu_type: device.device_type.clone(),
            memory_mb: vram_bytes.unwrap_or(0) / (1024 * 1024),
            peer_gpus: Vec::new(),
            link_type: GpuLinkType::PCIe,
            stable_id: device.stable_id,
            topology: device.topology.clone(),
            numa_node: device.numa_node,
            affinity_cpus: device.affinity_cpus.clone(),
        }
    }
}

/// For a persisted map of GPUs an agent of the previous release also reads: it
/// is written in the legacy `GpuResource` shape, and `Device` reads either.
pub mod legacy_gpu_serde {
    use super::{Device, LegacyGpu};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<u64, Vec<Device>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let legacy: BTreeMap<u64, Vec<LegacyGpu>> = map
            .iter()
            .map(|(key, gpus)| (*key, gpus.iter().map(LegacyGpu::from).collect()))
            .collect();
        legacy.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<u64, Vec<Device>>, D::Error> {
        BTreeMap::<u64, Vec<Device>>::deserialize(deserializer)
    }
}

/// The wire `devices`, plus the deprecated `gpus` list still filled for one
/// release so readers built before `devices` keep seeing the node's GPUs.
pub fn devices_to_proto(
    devices: &[Device],
) -> (
    Vec<spur_proto::proto::Device>,
    Vec<spur_proto::proto::GpuResource>,
) {
    let wire = devices
        .iter()
        .map(|d| spur_proto::proto::Device {
            gres_name: d.gres_name.clone(),
            device_type: d.device_type.clone(),
            stable_id: d.stable_id,
            index: d.index,
            topology: d.topology.as_ref().map(crate::device_topology::to_proto),
            numa_node: d.numa_node,
            affinity_cpus: d.affinity_cpus.clone(),
        })
        .collect();
    let legacy = devices
        .iter()
        .filter(|d| d.is_gpu())
        .map(|d| {
            let legacy = LegacyGpu::from(d);
            spur_proto::proto::GpuResource {
                device_id: legacy.device_id,
                gpu_type: legacy.gpu_type,
                memory_mb: legacy.memory_mb,
                peer_gpus: legacy.peer_gpus,
                link_type: spur_proto::proto::GpuLinkType::GpuLinkPcie as i32,
                stable_id: legacy.stable_id,
                topology: d.topology.as_ref().map(crate::device_topology::to_proto),
                numa_node: legacy.numa_node,
                affinity_cpus: legacy.affinity_cpus,
            }
        })
        .collect();
    (wire, legacy)
}

/// A sender's `devices`, or its deprecated `gpus` list when it filled only that.
pub fn devices_from_proto(
    wire: Vec<spur_proto::proto::Device>,
    legacy_gpus: Vec<spur_proto::proto::GpuResource>,
) -> Vec<Device> {
    if !wire.is_empty() {
        return wire
            .into_iter()
            .map(|d| Device {
                gres_name: d.gres_name,
                device_type: d.device_type,
                stable_id: d.stable_id,
                index: d.index,
                topology: d.topology.map(crate::device_topology::from_proto),
                numa_node: d.numa_node,
                affinity_cpus: d.affinity_cpus,
            })
            .collect();
    }
    legacy_gpus
        .into_iter()
        .map(|g| {
            let mut topology = g.topology.map(crate::device_topology::from_proto);
            // An older sender with no topology (the K8s operator) still reports VRAM here.
            if g.memory_mb > 0 && topology.as_ref().and_then(|t| t.vram_bytes).is_none() {
                topology.get_or_insert_with(Default::default).vram_bytes = Some(g.memory_mb << 20);
            }
            Device {
                topology,
                numa_node: g.numa_node,
                affinity_cpus: g.affinity_cpus,
                ..Device::gpu(g.stable_id, &g.gpu_type, g.device_id)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{devices_from_proto, devices_to_proto, Device, LegacyGpu};

    #[test]
    fn legacy_gpu_writes_back_numa_and_affinity() {
        let device = Device {
            numa_node: Some(0),
            affinity_cpus: vec![2, 3],
            ..Device::gpu(7, "mi355x", 0)
        };
        let json = serde_json::to_value(LegacyGpu::from(&device)).unwrap();
        assert_eq!(json["numa_node"], 0);
        assert_eq!(json["affinity_cpus"], serde_json::json!([2, 3]));
    }

    #[test]
    fn proto_devices_round_trip_numa_and_affinity() {
        let devices = vec![
            Device {
                numa_node: Some(1),
                affinity_cpus: vec![4],
                ..Device::gpu(9, "mi355x", 0)
            },
            Device::new("nic", "pollara400", 3, 0),
        ];
        let (wire, legacy) = devices_to_proto(&devices);
        assert_eq!(wire[0].numa_node, Some(1));
        assert_eq!(wire[0].affinity_cpus, vec![4]);
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].numa_node, Some(1));
        assert_eq!(legacy[0].affinity_cpus, vec![4]);
        assert_eq!(devices_from_proto(wire, Vec::new()), devices);
        assert_eq!(
            devices_from_proto(Vec::new(), legacy)[0].affinity_cpus,
            vec![4]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn a_legacy_gpu_without_topology_keeps_its_memory() {
        let legacy = spur_proto::proto::GpuResource {
            memory_mb: 65536,
            topology: None,
            ..Default::default()
        };
        let devices = devices_from_proto(Vec::new(), vec![legacy]);
        assert_eq!(
            devices[0].topology.as_ref().and_then(|t| t.vram_bytes),
            Some(65536 << 20)
        );
    }

    #[test]
    fn proto_devices_round_trip_the_gpu_topology() {
        let topology = super::DeviceTopology {
            pci_bdf: Some("0002:00:01.0".into()),
            numa_node: Some(0),
            pci_root: Some("pci0002:00".into()),
            vram_bytes: Some(309_220_868_096),
            fabric: None,
        };
        let devices = vec![super::Device {
            topology: Some(topology.clone()),
            ..super::Device::gpu(33_556_480, "mi355x", 0)
        }];
        let (wire, legacy) = super::devices_to_proto(&devices);
        assert_eq!(
            super::devices_from_proto(wire, Vec::new())[0].topology,
            Some(topology.clone())
        );
        assert_eq!(
            super::devices_from_proto(Vec::new(), legacy)[0].topology,
            Some(topology)
        );
    }

    #[test]
    fn gpus_and_kinds_with_devices_are_allocated_by_id() {
        assert!(super::allocates_by_id("gpu", false));
        assert!(super::allocates_by_id("nic", true));
        assert!(!super::allocates_by_id("nic", false));
    }

    #[test]
    fn a_pool_key_names_its_gres_before_the_type() {
        assert_eq!(super::pool_gres_name("nic:pollara400"), "nic");
        assert_eq!(super::pool_gres_name("bandwidth"), "bandwidth");
    }

    use super::*;

    /// A `GpuResource` as builds before the generic inventory serialized it.
    mod prev {
        use super::super::{DeviceTopology, GpuLinkType};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Serialize, Deserialize)]
        pub struct GpuResource {
            pub device_id: u32,
            pub gpu_type: String,
            pub memory_mb: u64,
            pub peer_gpus: Vec<u32>,
            pub link_type: GpuLinkType,
            #[serde(default)]
            pub stable_id: u64,
            #[serde(default)]
            pub topology: Option<DeviceTopology>,
            #[serde(default)]
            pub numa_node: Option<u32>,
            #[serde(default)]
            pub affinity_cpus: Vec<u32>,
        }
    }

    #[test]
    fn gpu_maps_written_for_older_agents_decode_with_their_struct() {
        #[derive(Serialize)]
        struct Now {
            #[serde(with = "super::legacy_gpu_serde")]
            anchors: std::collections::BTreeMap<u64, Vec<Device>>,
        }
        #[derive(Deserialize)]
        struct Before {
            anchors: std::collections::BTreeMap<u64, Vec<prev::GpuResource>>,
        }
        let gpu = Device {
            topology: Some(DeviceTopology {
                vram_bytes: Some(48 << 30),
                ..Default::default()
            }),
            ..Device::gpu(0x110001, "mi300x", 1)
        };
        let json = serde_json::to_string(&Now {
            anchors: [(0x110000, vec![gpu])].into(),
        })
        .unwrap();
        let before: Before = serde_json::from_str(&json).expect("an older agent reads it");
        let read = &before.anchors[&0x110000][0];
        assert_eq!(
            (
                read.stable_id,
                read.gpu_type.as_str(),
                read.device_id,
                read.memory_mb
            ),
            (0x110001, "mi300x", 1, 49152)
        );
    }

    const LEGACY_GPU_JSON: &str = r#"{"device_id": 2, "gpu_type": "mi300x", "memory_mb": 196608,
        "peer_gpus": [0, 1], "link_type": "XGMI", "stable_id": 1114112,
        "topology": {"pci_bdf": "0000:11:00.0", "numa_node": 0, "pci_root": null,
                     "vram_bytes": 206158430208, "fabric": null}}"#;

    #[test]
    fn a_legacy_gpu_entry_reads_as_a_gpu_device() {
        let device: Device = serde_json::from_str(LEGACY_GPU_JSON).expect("legacy GPU entry");
        assert!(device.is_gpu());
        assert_eq!(device.device_type, "mi300x");
        assert_eq!(device.stable_id, 0x110000);
        assert_eq!(device.index, 2);
        assert_eq!(
            device.topology.and_then(|t| t.vram_bytes),
            Some(206158430208)
        );
    }

    #[test]
    fn a_legacy_gpu_entry_without_stable_id_reads_as_zero() {
        let device: Device = serde_json::from_str(
            r#"{"device_id": 0, "gpu_type": "mi300x", "memory_mb": 0, "peer_gpus": [], "link_type": "PCIe"}"#,
        )
        .expect("pre-stable_id GPU entry");
        assert_eq!(device.stable_id, 0);
        assert!(device.is_gpu());
    }

    #[test]
    fn a_gpu_device_writes_the_shape_older_builds_read() {
        let mut device = Device::gpu(0x110000, "mi300x", 2);
        device.topology = Some(DeviceTopology {
            vram_bytes: Some(196608 * 1024 * 1024),
            ..Default::default()
        });

        let json = serde_json::to_string(&LegacyGpu::from(&device)).unwrap();
        let old: prev::GpuResource = serde_json::from_str(&json).expect("older build decodes it");
        assert_eq!(old.device_id, 2);
        assert_eq!(old.gpu_type, "mi300x");
        assert_eq!(old.stable_id, 0x110000);
        assert_eq!(old.memory_mb, 196608);
        assert_eq!(old.link_type, GpuLinkType::PCIe);
    }

    fn nic_with_topology() -> Device {
        let mut nic = Device::new("nic", "pollara400", 0x050000, 1);
        nic.topology = Some(DeviceTopology {
            pci_bdf: Some("0000:05:00.0".into()),
            numa_node: Some(0),
            ..Default::default()
        });
        nic
    }

    #[test]
    fn devices_round_trip_through_the_wire() {
        let devices = vec![Device::gpu(0x110000, "mi300x", 0), nic_with_topology()];
        let (wire, legacy) = devices_to_proto(&devices);
        assert_eq!(wire.len(), 2);
        assert_eq!(devices_from_proto(wire, legacy), devices);
    }

    #[test]
    fn the_deprecated_wire_list_carries_only_gpus() {
        let devices = vec![Device::gpu(0x110000, "mi300x", 3), nic_with_topology()];
        let (_, legacy) = devices_to_proto(&devices);
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].stable_id, 0x110000);
        assert_eq!(legacy[0].device_id, 3);
        assert_eq!(legacy[0].gpu_type, "mi300x");
    }

    #[test]
    fn an_older_senders_gpu_list_reads_as_gpu_devices() {
        let legacy = vec![spur_proto::proto::GpuResource {
            device_id: 1,
            gpu_type: "mi300x".into(),
            stable_id: 0x120000,
            ..Default::default()
        }];
        assert_eq!(
            devices_from_proto(Vec::new(), legacy),
            vec![Device::gpu(0x120000, "mi300x", 1)]
        );
    }

    #[test]
    fn a_non_gpu_device_round_trips() {
        let mut nic = Device::new("nic", "pollara400", 0x050000, 0);
        nic.topology = Some(DeviceTopology {
            numa_node: Some(1),
            ..Default::default()
        });
        let json = serde_json::to_string(&nic).unwrap();
        assert_eq!(serde_json::from_str::<Device>(&json).unwrap(), nic);
    }
}
