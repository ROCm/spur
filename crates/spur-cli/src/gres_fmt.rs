// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A node's GRES as Slurm prints it: one `name[:type]:count` entry per kind.

use spur_proto::proto::ResourceSet;

pub fn node_gres(resources: &ResourceSet) -> Vec<String> {
    let mut kinds: Vec<((&str, &str), u32)> = Vec::new();
    for device in &resources.devices {
        let kind = (device.gres_name.as_str(), device.device_type.as_str());
        match kinds.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, count)) => *count += 1,
            None => kinds.push((kind, 1)),
        }
    }
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
