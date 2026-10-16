// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Joins sharing identities with `ResourceSlice`s the AMD DRA driver publishes:
//! a sysfs tree is built from each slice the way the kernel lays it out, and
//! discovery must name every device and select it by the driver's BDF.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use spur_devices::cdi::sharing_identities_from;

const PCI_BUS_ID: &str = "resource.kubernetes.io/pciBusID";

struct SliceDevice {
    card: u32,
    render_minor: u32,
    pci_bus_id: String,
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("resourceslice-") && n.ends_with(".json"))
        })
        .collect();
    paths.sort();
    paths
}

/// A fixture is one `ResourceSlice` or a `kubectl get -o json` list of them.
fn slice_devices(fixture: &Value) -> Vec<SliceDevice> {
    let slices = match fixture.get("items") {
        Some(items) => items.as_array().unwrap().clone(),
        None => vec![fixture.clone()],
    };
    slices
        .iter()
        .filter(|s| s["spec"]["driver"] == "gpu.amd.com")
        .flat_map(|s| s["spec"]["devices"].as_array().unwrap().clone())
        .map(|d| {
            let name = d["name"].as_str().unwrap();
            let (card, render) = name
                .strip_prefix("gpu-")
                .and_then(|rest| rest.split_once('-'))
                .unwrap();
            SliceDevice {
                card: card.parse().unwrap(),
                render_minor: render.parse().unwrap(),
                pci_bus_id: d["attributes"][PCI_BUS_ID]["string"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            }
        })
        .collect()
}

fn location_id(pci_bus_id: &str) -> (u32, u64) {
    let (domain, rest) = pci_bus_id.split_once(':').unwrap();
    let (bus, rest) = rest.split_once(':').unwrap();
    let (dev, func) = rest.split_once('.').unwrap();
    let domain = u32::from_str_radix(domain, 16).unwrap();
    let bus = u64::from_str_radix(bus, 16).unwrap();
    let dev = u64::from_str_radix(dev, 16).unwrap();
    let func = u64::from_str_radix(func, 16).unwrap();
    (domain, (bus << 8) | (dev << 3) | func)
}

/// Lay out KFD nodes and DRM cards as the kernel does, including the
/// partition index it ORs into `location_id` for a partitioned GPU.
fn write_sysfs(devices: &[SliceDevice], kfd: &Path, drm: &Path) {
    let mut by_parent: BTreeMap<&str, Vec<&SliceDevice>> = BTreeMap::new();
    for d in devices {
        by_parent.entry(&d.pci_bus_id).or_default().push(d);
    }
    let mut node_id = 1;
    for (bdf, mut parts) in by_parent {
        parts.sort_by_key(|d| d.render_minor);
        let (domain, parent_location) = location_id(bdf);
        let partitioned = parts.len() > 1;
        for (index, d) in parts.iter().enumerate() {
            node_id += 1;
            let location = if partitioned {
                parent_location | index as u64
            } else {
                parent_location
            };
            let node = kfd.join(node_id.to_string());
            fs::create_dir_all(&node).unwrap();
            fs::write(
                node.join("properties"),
                format!(
                    "cpu_cores_count 0\nsimd_count 152\nvendor_id 4098\n\
                     drm_render_minor {}\nlocation_id {location}\ndomain {domain}\n",
                    d.render_minor
                ),
            )
            .unwrap();
            let card_drm = drm.join(format!("card{}/device/drm", d.card));
            fs::create_dir_all(card_drm.join(format!("renderD{}", d.render_minor))).unwrap();
        }
    }
}

#[test]
fn sharing_identities_match_dra_driver_resourceslices() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no ResourceSlice fixtures found");

    for path in fixtures {
        let fixture: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let devices = slice_devices(&fixture);
        assert!(
            !devices.is_empty(),
            "{} has no gpu.amd.com devices",
            path.display()
        );
        let kfd = tempfile::tempdir().unwrap();
        let drm = tempfile::tempdir().unwrap();
        write_sysfs(&devices, kfd.path(), drm.path());

        let discovered: BTreeSet<(String, String)> =
            sharing_identities_from(kfd.path(), drm.path())
                .into_iter()
                .map(|g| (g.dra_device_name().unwrap(), g.selector_bdf))
                .collect();

        let published: BTreeSet<(String, String)> = devices
            .iter()
            .map(|d| {
                (
                    format!("gpu-{}-{}", d.card, d.render_minor),
                    d.pci_bus_id.clone(),
                )
            })
            .collect();
        assert_eq!(discovered, published, "{}", path.display());
    }
}
