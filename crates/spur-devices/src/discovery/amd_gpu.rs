// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::Path;

use tracing::{debug, warn};

use crate::cdi::annotations::{self, DeviceMetadata};
use crate::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, Mount, GENERATED_CDI_VERSION};
use crate::pci::{bdf_from_identity, encode_stable_id, PciIdentity};
use crate::sysfs::{
    char_device_node, parse_kv_file, read_numa_node, read_sysfs_string, under_root,
};
use crate::topology::DeviceTopology;
use crate::types::LinkType;

const AMD_CDI_KIND: &str = "amd.com/gpu";
const KFD_AMD_VENDOR_ID: u32 = 4098; // 0x1002
const VRAM_HEAP_TYPE: u64 = 1;
const IO_LINK_XGMI: u32 = 2;
const GPU_SUPPLEMENTARY_GROUPS: [&str; 2] = ["video", "render"];

pub fn discover_gpu_cdi() -> Vec<CdiSpec> {
    discover_gpu_cdi_from_root(Path::new("/"))
}

/// Reads `sys/`, `dev/`, `opt/rocm/` and `etc/group` under `root`; the spec still
/// names host paths, since that is where a job will find them.
pub(super) fn discover_gpu_cdi_from_root(root: &Path) -> Vec<CdiSpec> {
    let gpus = discover_amd_gpus_from_root(&root.join("sys"));
    if gpus.is_empty() {
        return Vec::new();
    }

    let devices: Vec<CdiDevice> = gpus.iter().map(|g| g.to_cdi_device(root)).collect();

    let shared_edits = build_shared_edits(root);

    let mut spec_annotations = HashMap::new();
    spec_annotations.insert(annotations::AUTO_DETECTED.into(), "true".into());

    vec![CdiSpec {
        cdi_version: GENERATED_CDI_VERSION.into(),
        kind: AMD_CDI_KIND.into(),
        annotations: spec_annotations,
        devices,
        container_edits: if shared_edits.is_empty() {
            None
        } else {
            Some(shared_edits)
        },
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KfdGpuNode {
    node_id: u32,
    render_minor: u32,
    device_id: u32,
    location_id: u64,
    domain: u32,
    hive_id: u64,
    unique_id: u64,
    vram_bytes: Option<u64>,
    io_links: Vec<KfdIoLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KfdIoLink {
    node_to: u32,
    link_type: u32,
    weight: u32,
}

fn discover_kfd_gpus_from_root(root: &Path) -> Vec<KfdGpuNode> {
    if !root.exists() {
        warn!(
            path = %root.display(),
            "KFD topology not found; no AMD GPUs discovered"
        );
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        warn!(path = %root.display(), "cannot read KFD topology directory");
        return Vec::new();
    };

    let mut gpus = Vec::new();
    for entry in entries.flatten() {
        let node_path = entry.path();
        if !node_path.is_dir() {
            continue;
        }

        let node_id = match node_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => match name.parse::<u32>() {
                Ok(id) => id,
                Err(_) => continue,
            },
            None => continue,
        };

        let props_path = node_path.join("properties");
        let props = match parse_kv_file(&props_path) {
            Some(p) => p,
            None => continue,
        };

        if !is_amd_gpu_node(&props) {
            continue;
        }

        let render_minor = props
            .get("drm_render_minor")
            .and_then(|s| parse_u32(s))
            .unwrap_or(0);
        if render_minor == 0 {
            continue;
        }

        let vram_bytes = read_vram_bytes(&node_path);
        let io_links = read_io_links(&node_path);

        gpus.push(KfdGpuNode {
            node_id,
            render_minor,
            device_id: props
                .get("device_id")
                .and_then(|s| parse_u32(s))
                .unwrap_or(0),
            location_id: props
                .get("location_id")
                .and_then(|s| parse_u64(s))
                .unwrap_or(0),
            domain: props.get("domain").and_then(|s| parse_u32(s)).unwrap_or(0),
            hive_id: props.get("hive_id").and_then(|s| parse_u64(s)).unwrap_or(0),
            unique_id: props
                .get("unique_id")
                .and_then(|s| parse_u64(s))
                .unwrap_or(0),
            vram_bytes,
            io_links,
        });
    }

    gpus.sort_by_key(|g| g.render_minor);
    gpus
}

fn is_amd_gpu_node(props: &HashMap<String, String>) -> bool {
    let cpu_cores = props
        .get("cpu_cores_count")
        .and_then(|s| parse_u32(s))
        .unwrap_or(0);
    let simd_count = props
        .get("simd_count")
        .and_then(|s| parse_u32(s))
        .unwrap_or(0);
    let vendor_id = props
        .get("vendor_id")
        .and_then(|s| parse_u32(s))
        .unwrap_or(0);

    cpu_cores == 0 && simd_count > 0 && vendor_id == KFD_AMD_VENDOR_ID
}

fn read_vram_bytes(node_path: &Path) -> Option<u64> {
    let mem_banks_dir = node_path.join("mem_banks");
    let entries = std::fs::read_dir(&mem_banks_dir).ok()?;

    for entry in entries.flatten() {
        let props_path = entry.path().join("properties");
        let Some(props) = parse_kv_file(&props_path) else {
            continue;
        };
        let heap_type = props
            .get("heap_type")
            .and_then(|s| parse_u64(s))
            .unwrap_or(0);
        if heap_type == VRAM_HEAP_TYPE {
            return props.get("size_in_bytes").and_then(|s| parse_u64(s));
        }
    }

    None
}

fn read_io_links(node_path: &Path) -> Vec<KfdIoLink> {
    let io_links_dir = node_path.join("io_links");
    let Ok(entries) = std::fs::read_dir(&io_links_dir) else {
        return Vec::new();
    };

    let mut links = Vec::new();
    for entry in entries.flatten() {
        let props_path = entry.path().join("properties");
        let Some(props) = parse_kv_file(&props_path) else {
            continue;
        };

        let node_to = props.get("node_to").and_then(|s| parse_u32(s));
        let Some(node_to) = node_to else {
            continue;
        };

        links.push(KfdIoLink {
            node_to,
            link_type: props.get("type").and_then(|s| parse_u32(s)).unwrap_or(0),
            weight: props.get("weight").and_then(|s| parse_u32(s)).unwrap_or(0),
        });
    }

    links
}

/// Maps render minor to PCI identity. The low three bits of `location_id` are
/// the PCI function for a lone KFD node, and the kernel's partition number when
/// one device has several KFD nodes. So a device with several real functions,
/// each with a KFD node, decodes as partitions, and at most 8 ranks exist.
fn resolve_pci_identities(nodes: &[KfdGpuNode]) -> HashMap<u32, PciIdentity> {
    let slot = |n: &KfdGpuNode| {
        let bus = (n.location_id >> 8) as u8;
        let device = ((n.location_id >> 3) & 0x1f) as u8;
        (n.domain, bus, device)
    };
    let mut nodes_per_slot: HashMap<(u32, u8, u8), usize> = HashMap::new();
    for node in nodes {
        *nodes_per_slot.entry(slot(node)).or_default() += 1;
    }

    nodes
        .iter()
        .map(|node| {
            let (domain, bus, device) = slot(node);
            let low_bits = (node.location_id & 0x07) as u8;
            let (function, partition_rank) = if nodes_per_slot[&(domain, bus, device)] == 1 {
                (low_bits, 0)
            } else {
                (0, low_bits)
            };
            let identity = PciIdentity {
                domain,
                bus,
                device,
                function,
                partition_rank,
            };
            (node.render_minor, identity)
        })
        .collect()
}

fn format_unique_id(unique_id: u64) -> String {
    format!("{:016x}", unique_id)
}

fn has_xgmi_link(links: &[KfdIoLink]) -> bool {
    links.iter().any(|l| l.link_type == IO_LINK_XGMI)
}

fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

struct DiscoveredGpu {
    device_index: u32,
    render_minor: u32,
    stable_id: u64,
    topology: Option<DeviceTopology>,
    card_id: Option<u32>,
    gpu_type: String,
    memory_mb: u64,
    numa_node: Option<u32>,
    pci_bdf: Option<String>,
    link_type: LinkType,
    links: Vec<i32>,
    compute_partition: Option<String>,
    memory_partition: Option<String>,
    unique_id: Option<String>,
}

impl DiscoveredGpu {
    fn to_cdi_device(&self, root: &Path) -> CdiDevice {
        let render_path = format!("/dev/dri/renderD{}", self.render_minor);

        let meta = DeviceMetadata {
            gpu_type: Some(self.gpu_type.clone()),
            memory_mb: self.memory_mb,
            numa_node: self.numa_node,
            cores: None,
            links: Some(self.links.clone()),
            link_type: Some(self.link_type),
            pci_bdf: self.pci_bdf.clone(),
            auto_detected: true,
            compute_partition: self.compute_partition.clone(),
            memory_partition: self.memory_partition.clone(),
            unique_id: self.unique_id.clone(),
            render_minor: Some(self.render_minor),
            topology: self.topology.clone(),
            stable_id: Some(self.stable_id),
        };

        let mut device_nodes = vec![char_device_node(root, render_path)];
        // Only a card linked to this render node in sysfs: a guessed index can
        // name another GPU's card, which the job would then be granted.
        if let Some(card_id) = self.card_id {
            device_nodes.push(char_device_node(root, format!("/dev/dri/card{card_id}")));
        }

        CdiDevice {
            name: self.device_index.to_string(),
            annotations: meta.to_annotations(),
            container_edits: Some(ContainerEdits {
                device_nodes,
                ..Default::default()
            }),
        }
    }
}

fn build_shared_edits(root: &Path) -> ContainerEdits {
    let mut edits = ContainerEdits::default();

    if under_root(root, "/dev/kfd").exists() {
        edits
            .device_nodes
            .push(char_device_node(root, "/dev/kfd".into()));
    }

    for lib_path in &["/opt/rocm/lib", "/opt/rocm/lib64"] {
        if under_root(root, lib_path).is_dir() {
            edits.mounts.push(Mount {
                host_path: lib_path.to_string(),
                container_path: lib_path.to_string(),
                r#type: None,
                options: Some(vec![
                    "ro".into(),
                    "nosuid".into(),
                    "nodev".into(),
                    "bind".into(),
                ]),
            });
        }
    }

    edits.additional_gids = gpu_supplementary_gids_from_group_file(&under_root(root, "/etc/group"));
    edits
}

fn lookup_group_gid_in(group_file: &str, name: &str) -> Option<u32> {
    for line in group_file.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == name {
            return parts[2].trim().parse().ok();
        }
    }
    None
}

fn gpu_supplementary_gids_from_group_file(path: &Path) -> Vec<u32> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut gids: Vec<u32> = GPU_SUPPLEMENTARY_GROUPS
        .iter()
        .filter_map(|name| lookup_group_gid_in(&content, name))
        .collect();
    gids.sort_unstable();
    gids.dedup();
    gids
}

fn discover_amd_gpus_from_root(sysfs: &Path) -> Vec<DiscoveredGpu> {
    let kfd_nodes = discover_kfd_gpus_from_root(&sysfs.join("class/kfd/kfd/topology/nodes"));
    if kfd_nodes.is_empty() {
        return Vec::new();
    }

    let node_id_to_index: HashMap<u32, usize> = kfd_nodes
        .iter()
        .enumerate()
        .map(|(i, g)| (g.node_id, i))
        .collect();

    let identities = resolve_pci_identities(&kfd_nodes);

    let mut gpus: Vec<_> = kfd_nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let device_index = node.render_minor.saturating_sub(128);
            let identity = &identities[&node.render_minor];
            let pci_bdf = bdf_from_identity(identity);
            // A partition's render node links to an amdgpu_xcp platform device,
            // which has none of these attributes, so read the PCI device.
            let device_path = sysfs.join("bus/pci/devices").join(&pci_bdf);
            if !device_path.exists() {
                warn!(
                    pci_bdf = %pci_bdf,
                    render_minor = node.render_minor,
                    "decoded PCI device does not exist in sysfs; attributes will be missing"
                );
            }

            let gpu_type = if node.device_id != 0 {
                detect_amd_gpu_type_from_id(node.device_id)
            } else if let Some(name) = read_sysfs_string(&device_path.join("product_name")) {
                normalize_gpu_name(&name)
            } else {
                "amdgpu".into()
            };
            let memory_mb = node.vram_bytes.unwrap_or(0) / (1024 * 1024);
            let numa_node = read_numa_node(&device_path);
            let pci_bdf = Some(pci_bdf);
            let link_type = if node.hive_id != 0 || has_xgmi_link(&node.io_links) {
                LinkType::Xgmi
            } else {
                LinkType::Pcie
            };
            let links = build_link_weights(&kfd_nodes, index, &node_id_to_index);
            let compute_partition =
                read_sysfs_string(&device_path.join("current_compute_partition"));
            let memory_partition = read_sysfs_string(&device_path.join("current_memory_partition"));
            let unique_id = if node.unique_id != 0 {
                Some(format_unique_id(node.unique_id))
            } else {
                read_sysfs_string(&device_path.join("unique_id"))
            };
            let card_id = find_card_for_render_minor(sysfs, node.render_minor);
            let stable_id = encode_stable_id(identity);

            debug!(
                device_index,
                render_minor = node.render_minor,
                stable_id,
                gpu_type = %gpu_type,
                memory_mb,
                numa_node = ?numa_node,
                pci_bdf = ?pci_bdf,
                card_id = ?card_id,
                compute_partition = ?compute_partition,
                "discovered AMD GPU via KFD topology"
            );

            DiscoveredGpu {
                device_index,
                render_minor: node.render_minor,
                stable_id,
                card_id,
                gpu_type,
                memory_mb,
                topology: Some(DeviceTopology {
                    pci_bdf: pci_bdf.clone(),
                    numa_node,
                    pci_root: crate::pci::pci_root(&device_path),
                    vram_bytes: node.vram_bytes,
                    fabric: crate::ualink::observe(&device_path),
                }),
                numa_node,
                pci_bdf,
                link_type,
                links,
                compute_partition,
                memory_partition,
                unique_id,
            }
        })
        .collect();
    crate::ualink::validate_local(
        &mut gpus
            .iter_mut()
            .filter_map(|gpu| gpu.topology.as_mut())
            .collect::<Vec<_>>(),
    );
    gpus
}

fn build_link_weights(
    gpus: &[KfdGpuNode],
    index: usize,
    node_id_to_index: &HashMap<u32, usize>,
) -> Vec<i32> {
    let n = gpus.len();
    let mut links = vec![0i32; n];
    links[index] = -1;

    for link in &gpus[index].io_links {
        if let Some(&target_idx) = node_id_to_index.get(&link.node_to) {
            links[target_idx] = link.weight as i32;
        }
    }

    links
}

fn find_card_for_render_minor(sysfs: &Path, render_minor: u32) -> Option<u32> {
    let render_name = format!("renderD{render_minor}");
    let drm_dir = sysfs.join("class/drm");
    let entries = std::fs::read_dir(drm_dir).ok()?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("card") || name_str.contains('-') {
            continue;
        }

        let render_link = entry.path().join("device/drm").join(&render_name);
        if render_link.exists() {
            return name_str.trim_start_matches("card").parse().ok();
        }
    }

    None
}

fn detect_amd_gpu_type_from_id(device_id: u32) -> String {
    let dev_id = format!("0x{:04x}", device_id);
    match dev_id.as_str() {
        "0x74a1" | "0x74a9" | "0x74bd" => "mi300x".into(),
        "0x74a0" => "mi300a".into(),
        "0x74a5" => "mi325x".into(),
        "0x74a2" | "0x74a8" => "mi308x".into(),
        "0x75a0" => "mi350x".into(),
        "0x75a3" => "mi355x".into(),
        "0x740f" => "mi210".into(),
        "0x7408" | "0x740c" => "mi250x".into(),
        "0x738c" | "0x738e" => "mi100".into(),
        "0x7550" | "0x7551" => "rx9070".into(),
        _ => format!("amdgpu-{dev_id}"),
    }
}

pub(crate) fn normalize_gpu_name(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("mi355") {
        "mi355x".into()
    } else if lower.contains("mi350") {
        "mi350x".into()
    } else if lower.contains("mi325") {
        "mi325x".into()
    } else if lower.contains("mi308") {
        "mi308x".into()
    } else if lower.contains("mi300x") {
        "mi300x".into()
    } else if lower.contains("mi300a") {
        "mi300a".into()
    } else if lower.contains("mi250") {
        "mi250x".into()
    } else if lower.contains("mi210") {
        "mi210".into()
    } else if lower.contains("mi100") {
        "mi100".into()
    } else if lower.contains("9070 xt") || lower.contains("9070xt") {
        "rx9070xt".into()
    } else if lower.contains("9070") {
        "rx9070".into()
    } else if lower.contains("9060") {
        "rx9060".into()
    } else {
        name.trim().to_lowercase().replace(' ', "-")
    }
}

#[cfg(test)]
mod tests {
    use crate::pci::{stable_id_to_bdf, STABLE_ID_PARTITION_MASK};
    #[test]
    fn vram_observation_distinguishes_missing_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(super::read_vram_bytes(dir.path()), None);
        let bank = dir.path().join("mem_banks/0");
        std::fs::create_dir_all(&bank).unwrap();
        std::fs::write(bank.join("properties"), "heap_type 1\nsize_in_bytes 0\n").unwrap();
        assert_eq!(super::read_vram_bytes(dir.path()), Some(0));
        std::fs::write(
            bank.join("properties"),
            "heap_type 1\nsize_in_bytes invalid\n",
        )
        .unwrap();
        assert_eq!(super::read_vram_bytes(dir.path()), None);
    }

    use super::*;
    use std::fs;

    fn write_kv(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, lines.join("\n")).unwrap();
    }

    fn make_kfd_gpu(base: &Path, node_id: u32, render_minor: u32, location_id: u64) {
        let node = base.join(node_id.to_string());
        write_kv(
            &node.join("properties"),
            &[
                "cpu_cores_count 0",
                "simd_count 320",
                "vendor_id 4098",
                "device_id 29858",
                &format!("drm_render_minor {render_minor}"),
                &format!("location_id {location_id}"),
                "domain 0",
                "hive_id 17778158520442550414",
                "unique_id 2864457224837561596",
                "num_xcc 4",
                "gfx_target_version 90402",
            ],
        );
        write_kv(
            &node.join("mem_banks/0/properties"),
            &["heap_type 1", "size_in_bytes 206141652992"],
        );
        write_kv(
            &node.join("io_links/0/properties"),
            &[
                "type 2",
                "node_from 2",
                "node_to 3",
                "weight 20",
                "max_bandwidth 64000",
            ],
        );
    }

    #[test]
    fn test_parse_kv_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("properties");
        write_kv(&path, &["cpu_cores_count 0", "simd_count 320"]);

        let map = parse_kv_file(&path).unwrap();
        assert_eq!(map.get("cpu_cores_count").unwrap(), "0");
        assert_eq!(map.get("simd_count").unwrap(), "320");
    }

    #[test]
    fn test_bdf_from_identity() {
        assert_eq!(
            bdf_from_identity(&lone_node_identity(1280, 0)),
            "0000:05:00.0"
        );
        assert_eq!(
            bdf_from_identity(&lone_node_identity(10496, 0)),
            "0000:29:00.0"
        );
    }

    #[test]
    fn test_discover_kfd_gpus_from_fixture() {
        let dir = tempfile::tempdir().unwrap();
        make_kfd_gpu(dir.path(), 2, 128, 1280);
        make_kfd_gpu(dir.path(), 3, 136, 10496);

        write_kv(
            &dir.path().join("0/properties"),
            &["cpu_cores_count 96", "simd_count 0", "vendor_id 0"],
        );

        let gpus = discover_kfd_gpus_from_root(dir.path());
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].render_minor, 128);
        assert_eq!(gpus[1].render_minor, 136);
        assert_eq!(gpus[0].vram_bytes, Some(206_141_652_992));
        assert_eq!(gpus[0].io_links.len(), 1);
        assert_eq!(gpus[0].io_links[0].node_to, 3);
        assert!(has_xgmi_link(&gpus[0].io_links));
    }

    #[test]
    fn test_discover_kfd_gpus_missing_root() {
        let gpus = discover_kfd_gpus_from_root(Path::new("/nonexistent/kfd/topology"));
        assert!(gpus.is_empty());
    }

    #[test]
    fn test_format_unique_id() {
        assert_eq!(format_unique_id(0x0123_4567_89ab_cdef), "0123456789abcdef");
    }

    #[test]
    fn test_normalize_gpu_name() {
        assert_eq!(normalize_gpu_name("AMD Instinct MI300X"), "mi300x");
        assert_eq!(normalize_gpu_name("AMD Instinct MI300XHF OAM"), "mi300x");
        assert_eq!(normalize_gpu_name("AMD Instinct MI300A"), "mi300a");
        assert_eq!(normalize_gpu_name("AMD Instinct MI308X"), "mi308x");
        assert_eq!(normalize_gpu_name("MI325X OAM"), "mi325x");
        assert_eq!(normalize_gpu_name("AMD Instinct MI350X"), "mi350x");
        assert_eq!(normalize_gpu_name("AMD Instinct MI355X"), "mi355x");
        assert_eq!(normalize_gpu_name("mi250x"), "mi250x");
        assert_eq!(normalize_gpu_name("AMD Instinct MI210"), "mi210");
        assert_eq!(normalize_gpu_name("AMD Instinct MI100"), "mi100");
        assert_eq!(normalize_gpu_name("AMD Radeon RX 9070 XT"), "rx9070xt");
        assert_eq!(normalize_gpu_name("Navi 48 [Radeon RX 9070]"), "rx9070");
    }

    #[test]
    fn test_detect_amd_gpu_type_from_id() {
        assert_eq!(detect_amd_gpu_type_from_id(0x74a2), "mi308x");
        assert_eq!(detect_amd_gpu_type_from_id(0x74a1), "mi300x");
        assert_eq!(detect_amd_gpu_type_from_id(0xffff), "amdgpu-0xffff");
    }

    #[test]
    fn test_build_link_weights() {
        let gpus = vec![
            KfdGpuNode {
                node_id: 2,
                render_minor: 128,
                device_id: 0x74a2,
                location_id: 1280,
                domain: 0,
                hive_id: 1,
                unique_id: 1,
                vram_bytes: Some(0),
                io_links: vec![KfdIoLink {
                    node_to: 3,
                    link_type: 2,
                    weight: 20,
                }],
            },
            KfdGpuNode {
                node_id: 3,
                render_minor: 129,
                device_id: 0x74a2,
                location_id: 10496,
                domain: 0,
                hive_id: 1,
                unique_id: 2,
                vram_bytes: Some(0),
                io_links: vec![KfdIoLink {
                    node_to: 2,
                    link_type: 2,
                    weight: 20,
                }],
            },
        ];
        let node_map: HashMap<u32, usize> = gpus
            .iter()
            .enumerate()
            .map(|(i, g)| (g.node_id, i))
            .collect();

        let links = build_link_weights(&gpus, 0, &node_map);
        assert_eq!(links, vec![-1, 20]);

        let links = build_link_weights(&gpus, 1, &node_map);
        assert_eq!(links, vec![20, -1]);
    }

    #[test]
    fn test_lookup_group_gid_in() {
        let content = "root:x:0:\nvideo:x:44:\nrender:x:109:\n";
        assert_eq!(lookup_group_gid_in(content, "video"), Some(44));
        assert_eq!(lookup_group_gid_in(content, "render"), Some(109));
        assert_eq!(lookup_group_gid_in(content, "missing"), None);
    }

    #[test]
    fn test_gpu_supplementary_gids_from_group_file() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        fs::write(&group, "video:x:44:\nrender:x:109:\n").unwrap();
        assert_eq!(
            gpu_supplementary_gids_from_group_file(&group),
            vec![44, 109]
        );
    }

    #[test]
    fn discovery_from_a_fixture_root_builds_one_gpu_spec() {
        let root = mi300x_cpx_sysfs()
            .parent()
            .expect("fixture sys dir has a parent")
            .to_path_buf();

        let specs = discover_gpu_cdi_from_root(&root);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].kind, AMD_CDI_KIND);
        assert_eq!(specs[0].devices.len(), 15);
        assert!(
            specs[0].container_edits.is_none(),
            "the fixture has no /dev/kfd, ROCm libraries or group file"
        );
    }

    #[test]
    fn only_a_card_linked_to_a_gpus_render_node_reaches_its_cdi_device() {
        let root = tempfile::tempdir().unwrap();
        let sys = root.path().join("sys");
        let kfd = sys.join("class/kfd/kfd/topology/nodes");
        make_kfd_gpu(&kfd, 2, 128, 1280);
        make_kfd_gpu(&kfd, 3, 129, 10496);
        let drm = sys.join("class/drm");
        fs::create_dir_all(drm.join("card0/device/drm/card0")).unwrap();
        fs::create_dir_all(drm.join("card1/device/drm/renderD128")).unwrap();

        let specs = discover_gpu_cdi_from_root(root.path());
        let nodes: Vec<Vec<&str>> = specs[0]
            .devices
            .iter()
            .map(|device| {
                device
                    .container_edits
                    .iter()
                    .flat_map(|edits| &edits.device_nodes)
                    .map(|node| node.path.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(
            nodes,
            vec![
                vec!["/dev/dri/renderD128", "/dev/dri/card1"],
                vec!["/dev/dri/renderD129"],
            ],
            "card0 belongs to another PCI device and the second GPU has no linked card"
        );
    }

    #[test]
    fn shared_edits_are_read_from_the_given_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dev")).unwrap();
        std::fs::write(root.path().join("dev/kfd"), b"").unwrap();
        std::fs::create_dir_all(root.path().join("opt/rocm/lib")).unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/group"),
            "video:x:44:\nrender:x:109:\nusers:x:100:\n",
        )
        .unwrap();

        let edits = build_shared_edits(root.path());
        let nodes: Vec<&str> = edits.device_nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(nodes, vec!["/dev/kfd"]);
        let mounts: Vec<&str> = edits.mounts.iter().map(|m| m.host_path.as_str()).collect();
        assert_eq!(mounts, vec!["/opt/rocm/lib"]);
        assert_eq!(edits.additional_gids, vec![44, 109]);
    }

    #[test]
    fn test_discovered_gpu_to_cdi_device() {
        let gpu = DiscoveredGpu {
            device_index: 3,
            render_minor: 131,
            topology: None,
            stable_id: encode_stable_id(&lone_node_identity(0xc100, 0)),
            card_id: Some(4),
            gpu_type: "mi300x".into(),
            memory_mb: 196608,
            numa_node: Some(1),
            pci_bdf: Some("0000:c1:00.0".into()),
            link_type: LinkType::Xgmi,
            links: vec![-1, 4, 4, 0],
            compute_partition: Some("CPX".into()),
            memory_partition: Some("NPS4".into()),
            unique_id: Some("abcdef0123456789".into()),
        };

        let dev = gpu.to_cdi_device(Path::new("/nonexistent-root"));
        assert_eq!(dev.name, "3");

        let edits = dev.container_edits.as_ref().unwrap();
        assert_eq!(edits.device_nodes.len(), 2);
        assert_eq!(edits.device_nodes[0].path, "/dev/dri/renderD131");
        assert_eq!(edits.device_nodes[1].path, "/dev/dri/card4");

        assert_eq!(
            dev.annotations.get(annotations::GPU_TYPE).unwrap(),
            "mi300x"
        );
        assert_eq!(
            dev.annotations.get(annotations::MEMORY_MB).unwrap(),
            "196608"
        );
        assert_eq!(dev.annotations.get(annotations::NUMA_NODE).unwrap(), "1");
        assert_eq!(dev.annotations.get(annotations::LINK_TYPE).unwrap(), "xgmi");
        assert_eq!(dev.annotations.get(annotations::LINKS).unwrap(), "-1,4,4,0");
        assert_eq!(
            dev.annotations.get(annotations::COMPUTE_PARTITION).unwrap(),
            "CPX"
        );
        assert_eq!(
            dev.annotations.get(annotations::MEMORY_PARTITION).unwrap(),
            "NPS4"
        );
        assert_eq!(
            dev.annotations.get(annotations::UNIQUE_ID).unwrap(),
            "abcdef0123456789"
        );
    }

    #[test]
    fn an_unresolved_card_link_injects_no_card_node() {
        let gpu = DiscoveredGpu {
            device_index: 3,
            render_minor: 131,
            topology: None,
            stable_id: 0,
            card_id: None,
            gpu_type: "mi300x".into(),
            memory_mb: 0,
            numa_node: None,
            pci_bdf: None,
            link_type: LinkType::Pcie,
            links: vec![-1],
            compute_partition: None,
            memory_partition: None,
            unique_id: None,
        };

        let dev = gpu.to_cdi_device(Path::new("/nonexistent-root"));
        let paths: Vec<&str> = dev
            .container_edits
            .as_ref()
            .unwrap()
            .device_nodes
            .iter()
            .map(|n| n.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["/dev/dri/renderD131"],
            "a guessed card node may belong to another GPU"
        );
    }

    #[test]
    fn test_discovered_spec_structure() {
        let gpu = DiscoveredGpu {
            device_index: 0,
            render_minor: 128,
            topology: None,
            stable_id: 0,
            card_id: None,
            gpu_type: "mi300x".into(),
            memory_mb: 196608,
            numa_node: Some(0),
            pci_bdf: None,
            link_type: LinkType::Pcie,
            links: vec![-1],
            compute_partition: None,
            memory_partition: None,
            unique_id: None,
        };

        let devices = vec![gpu.to_cdi_device(Path::new("/nonexistent-root"))];
        let spec = CdiSpec {
            cdi_version: GENERATED_CDI_VERSION.into(),
            kind: AMD_CDI_KIND.into(),
            annotations: HashMap::new(),
            devices,
            container_edits: None,
        };

        assert!(spec.validate().is_ok());
        assert_eq!(spec.qualified_name("0"), "amd.com/gpu=0");

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let parsed: CdiSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    fn kfd_node(node_id: u32, render_minor: u32, location_id: u64) -> KfdGpuNode {
        KfdGpuNode {
            node_id,
            render_minor,
            device_id: 0x74a1,
            location_id,
            domain: 0,
            hive_id: 0,
            unique_id: 0,
            vram_bytes: Some(0),
            io_links: Vec::new(),
        }
    }

    fn lone_node_identity(location_id: u64, domain: u32) -> PciIdentity {
        let node = KfdGpuNode {
            domain,
            ..kfd_node(2, 128, location_id)
        };
        resolve_pci_identities(&[node])[&128]
    }

    #[test]
    fn encode_stable_id_is_collision_free_and_decodes_bdf() {
        let a0 = lone_node_identity(0x0500, 0);
        let a1 = PciIdentity {
            partition_rank: 1,
            ..a0
        };
        let b0 = lone_node_identity(0x2900, 0);

        assert_ne!(encode_stable_id(&a0), encode_stable_id(&a1));
        assert_ne!(encode_stable_id(&a0), encode_stable_id(&b0));
        assert_eq!(encode_stable_id(&a0), 0x05u64 << 16);

        // A lone node on a non-zero function keeps that function.
        let loc = (0x29 << 8) | (0x06 << 3) | 0x01;
        let sid = encode_stable_id(&lone_node_identity(loc, 0));
        assert_eq!((sid >> 16) & 0xff, 0x29);
        assert_eq!((sid >> 11) & 0x1f, 0x06);
        assert_eq!((sid >> 8) & 0x07, 0x01);
        assert_eq!(sid & STABLE_ID_PARTITION_MASK, 0);
    }

    /// Two GPUs with the same bus:dev.func on different PCI domains must not
    /// collide as allocation ids.
    #[test]
    fn encode_stable_id_disambiguates_by_pci_domain() {
        let dom0 = lone_node_identity(0x1900, 0);
        let dom1 = lone_node_identity(0x1900, 1);
        let dom0_spx = encode_stable_id(&dom0);
        let dom1_spx = encode_stable_id(&dom1);
        assert_ne!(dom0_spx, dom1_spx);
        assert_eq!(dom1_spx - dom0_spx, 1u64 << 24);

        let rank1 = |id: PciIdentity| PciIdentity {
            partition_rank: 1,
            ..id
        };
        let ids = [
            dom0_spx,
            dom1_spx,
            encode_stable_id(&rank1(dom0)),
            encode_stable_id(&rank1(dom1)),
        ];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn stable_id_decodes_to_dra_pci_bus_id_form() {
        let sid = encode_stable_id(&lone_node_identity(0x1900, 0));
        assert_eq!(stable_id_to_bdf(sid), "0000:19:00.0");

        let loc = (0x3a << 8) | (0x02 << 3) | 0x01;
        let sid = encode_stable_id(&lone_node_identity(loc, 1));
        assert_eq!(stable_id_to_bdf(sid), "0001:3a:02.1");
    }

    #[test]
    fn partition_index_ranks_within_bdf_not_globally() {
        // Two GPUs with 2 partitions each. The render minors interleave across
        // the GPUs, so a global rank would give 0..3.
        let nodes = vec![
            kfd_node(2, 128, 0x0500),
            kfd_node(3, 129, 0x2900),
            kfd_node(4, 130, 0x0501),
            kfd_node(5, 131, 0x2901),
        ];

        let ids = resolve_pci_identities(&nodes);
        assert_eq!(ids[&128].partition_rank, 0);
        assert_eq!(ids[&130].partition_rank, 1);
        assert_eq!(ids[&129].partition_rank, 0);
        assert_eq!(ids[&131].partition_rank, 1);

        let sids: std::collections::HashSet<u64> = ids.values().map(encode_stable_id).collect();
        assert_eq!(sids.len(), 4);
    }

    /// Values measured on an MI300X at 0000:11:00.0 in CPX mode (amdgpu
    /// 6.19.14): the kernel ORs the partition node_id into the low three bits
    /// of location_id, so the eight partitions read 0x1100..0x1107 while lspci
    /// lists function 0 only. A SPX neighbour at 0000:2f:00.0 is the control.
    #[test]
    fn cpx_partitions_share_parent_bdf_with_function_zero() {
        let mut nodes: Vec<KfdGpuNode> = (0u32..8)
            .map(|i| kfd_node(9 + i, 128 + i, 0x1100 | u64::from(i)))
            .collect();
        nodes.push(kfd_node(2, 136, 0x2f00));

        let ids = resolve_pci_identities(&nodes);
        let parent_anchor = encode_stable_id(&lone_node_identity(0x1100, 0));
        for i in 0u32..8 {
            let id = &ids[&(128 + i)];
            assert_eq!(u32::from(id.partition_rank), i, "renderD{}", 128 + i);
            let sid = encode_stable_id(id);
            assert_eq!(stable_id_to_bdf(sid), "0000:11:00.0");
            assert_eq!(bdf_from_identity(id), "0000:11:00.0");
            assert_eq!(sid & !STABLE_ID_PARTITION_MASK, parent_anchor);
        }
        assert_eq!(ids[&136].partition_rank, 0);
        assert_eq!(bdf_from_identity(&ids[&136]), "0000:2f:00.0");

        let sids: std::collections::HashSet<u64> = ids.values().map(encode_stable_id).collect();
        assert_eq!(sids.len(), 9, "stable_ids stay distinct across partitions");
    }

    #[test]
    fn stable_ids_unchanged_when_render_minor_pool_shifts() {
        let base = vec![
            kfd_node(2, 128, 0x0500),
            kfd_node(3, 129, 0x2900),
            kfd_node(4, 130, 0x0501),
            kfd_node(5, 131, 0x2901),
        ];
        // A driver reload that moves the DRM minor pool while the BDFs stay.
        const SHIFT: u32 = 64;
        let shifted: Vec<KfdGpuNode> = base
            .iter()
            .map(|n| kfd_node(n.node_id, n.render_minor + SHIFT, n.location_id))
            .collect();

        let ids_base = resolve_pci_identities(&base);
        let ids_shifted = resolve_pci_identities(&shifted);
        for node in &base {
            assert_eq!(
                encode_stable_id(&ids_base[&node.render_minor]),
                encode_stable_id(&ids_shifted[&(node.render_minor + SHIFT)])
            );
        }
    }

    /// sysfs of an MI300X node with GPU 0 at 0000:11:00.0 in CPX and the
    /// other seven in SPX, captured on amdgpu 6.19.14. Serials are synthetic.
    fn mi300x_cpx_sysfs() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mi300x-cpx/sys")
    }

    #[test]
    fn cpx_partitions_resolve_to_parent_pci_device() {
        let gpus = discover_amd_gpus_from_root(&mi300x_cpx_sysfs());
        assert_eq!(gpus.len(), 15);

        let (partitions, spx): (Vec<&DiscoveredGpu>, Vec<&DiscoveredGpu>) =
            gpus.iter().partition(|g| g.render_minor < 136);
        assert_eq!(partitions.len(), 8);
        for (rank, gpu) in partitions.iter().enumerate() {
            let at = format!("renderD{}", gpu.render_minor);
            assert_eq!(gpu.pci_bdf.as_deref(), Some("0000:11:00.0"), "{at}");
            assert_eq!(stable_id_to_bdf(gpu.stable_id), "0000:11:00.0", "{at}");
            assert_eq!(
                gpu.stable_id & STABLE_ID_PARTITION_MASK,
                rank as u64,
                "{at}"
            );
            assert_eq!(gpu.numa_node, Some(0), "{at}");
            assert_eq!(gpu.compute_partition.as_deref(), Some("CPX"), "{at}");
        }

        let expected_spx = [
            ("0000:2f:00.0", 0),
            ("0000:46:00.0", 0),
            ("0000:5d:00.0", 0),
            ("0000:8b:00.0", 1),
            ("0000:aa:00.0", 1),
            ("0000:c2:00.0", 1),
            ("0000:da:00.0", 1),
        ];
        assert_eq!(spx.len(), expected_spx.len());
        for (gpu, (bdf, numa)) in spx.iter().zip(expected_spx) {
            assert_eq!(gpu.pci_bdf.as_deref(), Some(bdf));
            assert_eq!(stable_id_to_bdf(gpu.stable_id), bdf);
            assert_eq!(gpu.stable_id & STABLE_ID_PARTITION_MASK, 0);
            assert_eq!(gpu.numa_node, Some(numa));
            assert_eq!(gpu.compute_partition.as_deref(), Some("SPX"));
        }

        let ids: std::collections::HashSet<u64> = gpus.iter().map(|g| g.stable_id).collect();
        assert_eq!(ids.len(), 15);
    }

    #[test]
    fn root_relative_discovery_preserves_all_gpu_metadata_and_unknown_values() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let kfd = root.join("class/kfd/kfd/topology/nodes");
        make_kfd_gpu(&kfd, 2, 128, 1280);
        make_kfd_gpu(&kfd, 3, 129, 10496);
        for (minor, bdf, card) in [(128, "0000:05:00.0", 0), (129, "0000:29:00.0", 1)] {
            let pci = root.join("devices/pci0000:00").join(bdf);
            fs::create_dir_all(pci.join(format!("drm/renderD{minor}"))).unwrap();
            for (name, value) in [
                ("numa_node", "0"),
                ("current_compute_partition", "SPX"),
                ("current_memory_partition", "NPS1"),
            ] {
                fs::write(pci.join(name), value).unwrap();
            }
            let bus = root.join("bus/pci/devices");
            fs::create_dir_all(&bus).unwrap();
            std::os::unix::fs::symlink(&pci, bus.join(bdf)).unwrap();
            for device in [format!("renderD{minor}"), format!("card{card}")] {
                let path = root.join("class/drm").join(device);
                fs::create_dir_all(&path).unwrap();
                std::os::unix::fs::symlink(&pci, path.join("device")).unwrap();
            }
        }
        let gpus = discover_amd_gpus_from_root(root);
        assert_eq!(gpus.len(), 2);
        let gpu = &gpus[0];
        assert_eq!(gpu.gpu_type, "mi308x");
        assert_eq!(
            gpu.stable_id,
            encode_stable_id(&lone_node_identity(1280, 0))
        );
        assert_eq!(gpu.card_id, Some(0));
        assert_eq!(gpu.render_minor, 128);
        assert_eq!(gpu.compute_partition.as_deref(), Some("SPX"));
        assert_eq!(gpu.memory_partition.as_deref(), Some("NPS1"));
        assert_eq!(gpu.unique_id, Some(format_unique_id(2864457224837561596)));
        assert_eq!(gpu.link_type, LinkType::Xgmi);
        assert_eq!(gpu.links, [-1, 20]);
        let t = gpu.topology.as_ref().unwrap();
        assert_eq!(t.pci_bdf.as_deref(), Some("0000:05:00.0"));
        assert_eq!(t.pci_root.as_deref(), Some("pci0000:00"));
        assert_eq!(t.numa_node, Some(0));
        assert_eq!(t.vram_bytes, Some(206141652992));
        assert!(t.fabric.is_none());
        fs::write(root.join("devices/pci0000:00/0000:05:00.0/numa_node"), "-1").unwrap();
        fs::write(
            kfd.join("2/mem_banks/0/properties"),
            "heap_type 1\nsize_in_bytes bad",
        )
        .unwrap();
        let after = discover_amd_gpus_from_root(root)
            .remove(0)
            .topology
            .unwrap();
        assert_eq!(after.numa_node, None);
        assert_eq!(after.vram_bytes, None);
    }

    #[test]
    fn test_discover_amd_gpus_from_kfd_fixture() {
        let kfd_root = tempfile::tempdir().unwrap();
        make_kfd_gpu(kfd_root.path(), 2, 128, 1280);
        make_kfd_gpu(kfd_root.path(), 3, 129, 10496);

        write_kv(
            &kfd_root.path().join("3/io_links/0/properties"),
            &[
                "type 2",
                "node_from 2",
                "node_to 3",
                "weight 20",
                "max_bandwidth 64000",
            ],
        );
        write_kv(
            &kfd_root.path().join("2/io_links/0/properties"),
            &[
                "type 2",
                "node_from 2",
                "node_to 3",
                "weight 20",
                "max_bandwidth 64000",
            ],
        );

        let kfd_nodes = discover_kfd_gpus_from_root(kfd_root.path());
        assert_eq!(kfd_nodes.len(), 2);

        let node_map: HashMap<u32, usize> = kfd_nodes
            .iter()
            .enumerate()
            .map(|(i, g)| (g.node_id, i))
            .collect();

        let links = build_link_weights(&kfd_nodes, 0, &node_map);
        assert_eq!(links, vec![-1, 20]);

        assert_eq!(
            detect_amd_gpu_type_from_id(kfd_nodes[0].device_id),
            "mi308x"
        );
        assert_eq!(kfd_nodes[0].vram_bytes.unwrap() / (1024 * 1024), 196592);
        assert_eq!(
            bdf_from_identity(&resolve_pci_identities(&kfd_nodes)[&128]),
            "0000:05:00.0"
        );
    }
}
