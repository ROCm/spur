// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A node's GRES as Slurm prints it: one `name[:type]:count` entry per kind.

use spur_proto::proto::{Device, ResourceSet};

/// The node's devices, read from the deprecated GPU list when talking to a
/// controller that predates `devices`.
pub fn node_devices(resources: &ResourceSet) -> Vec<Device> {
    if !resources.devices.is_empty() {
        return resources.devices.clone();
    }
    #[allow(deprecated)]
    let legacy = resources.gpus.clone();
    let devices = spur_core::device::devices_from_proto(Vec::new(), legacy);
    spur_core::device::devices_to_proto(&devices).0
}

pub fn node_gres(resources: &ResourceSet) -> Vec<String> {
    let devices = node_devices(resources);
    let mut kinds: Vec<((&str, &str), u32)> = Vec::new();
    for device in &devices {
        let kind = (device.gres_name.as_str(), device.device_type.as_str());
        match kinds.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, count)) => *count += 1,
            None => kinds.push((kind, 1)),
        }
    }
    kinds.sort_by_key(|((name, _), _)| (*name != "gpu", *name));
    let devices = kinds.into_iter().map(|((name, ty), count)| {
        if ty.is_empty() {
            format!("{name}:{count}")
        } else {
            format!("{name}:{ty}:{count}")
        }
    });

    let mut pools: Vec<(&String, &u64)> = resources
        .generic
        .iter()
        .filter(|(_, count)| **count > 0)
        .collect();
    pools.sort();
    devices
        .chain(
            pools
                .into_iter()
                .map(|(pool, count)| format!("{pool}:{count}")),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::Device;

    fn device(gres_name: &str, device_type: &str, stable_id: u64) -> Device {
        Device {
            gres_name: gres_name.into(),
            device_type: device_type.into(),
            stable_id,
            ..Default::default()
        }
    }

    #[test]
    fn devices_aggregate_per_kind_and_type_in_inventory_order() {
        let set = ResourceSet {
            devices: vec![
                device("gpu", "mi300x", 1),
                device("gpu", "mi250x", 2),
                device("gpu", "mi300x", 3),
                device("nic", "pollara400", 4),
                device("nic", "pollara400", 5),
            ],
            ..Default::default()
        };
        assert_eq!(
            node_gres(&set),
            vec!["gpu:mi300x:2", "gpu:mi250x:1", "nic:pollara400:2"]
        );
    }

    #[test]
    fn gpus_print_first_then_kinds_by_name() {
        let set = ResourceSet {
            devices: vec![
                device("vnic", "pollara400", 1),
                device("nic", "pollara400", 2),
                device("gpu", "mi300x", 3),
            ],
            ..Default::default()
        };
        assert_eq!(
            node_gres(&set),
            vec!["gpu:mi300x:1", "nic:pollara400:1", "vnic:pollara400:1"]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn an_older_controller_s_gpus_still_show() {
        let set = ResourceSet {
            gpus: vec![
                spur_proto::proto::GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    stable_id: 128,
                    ..Default::default()
                },
                spur_proto::proto::GpuResource {
                    device_id: 1,
                    gpu_type: "mi300x".into(),
                    stable_id: 129,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(node_gres(&set), vec!["gpu:mi300x:2"]);
        assert_eq!(node_devices(&set).len(), 2);
    }

    #[test]
    fn an_untyped_device_omits_its_type() {
        let set = ResourceSet {
            devices: vec![device("nic", "", 1)],
            ..Default::default()
        };
        assert_eq!(node_gres(&set), vec!["nic:1"]);
    }

    #[test]
    fn countable_pools_follow_devices_in_name_order() {
        let set = ResourceSet {
            devices: vec![device("gpu", "mi300x", 1)],
            generic: [
                ("license".to_string(), 0),
                ("bandwidth:lustre".to_string(), 4096),
                ("akey".to_string(), 2),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(
            node_gres(&set),
            vec!["gpu:mi300x:1", "akey:2", "bandwidth:lustre:4096"]
        );
    }
}
