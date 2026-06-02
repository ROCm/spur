// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tracing::{debug, warn};

use crate::cdi::annotations::{self, DeviceMetadata};
use crate::cdi::spec::{
    CdiDevice, CdiSpec, ContainerEdits, DeviceNode, Mount, GENERATED_CDI_VERSION,
};
use crate::types::LinkType;

const AMD_CDI_KIND: &str = "amd.com/gpu";
const KFD_TOPOLOGY_ROOT: &str = "/sys/class/kfd/kfd/topology/nodes";
const KFD_AMD_VENDOR_ID: u32 = 4098; // 0x1002
const VRAM_HEAP_TYPE: u64 = 1;
const IO_LINK_XGMI: u32 = 2;
const GPU_SUPPLEMENTARY_GROUPS: [&str; 2] = ["video", "render"];

pub fn discover_to_cdi() -> Vec<CdiSpec> {
    let gpus = discover_amd_gpus();
    if gpus.is_empty() {
        return Vec::new();
    }

    let devices: Vec<CdiDevice> = gpus.iter().map(|g| g.to_cdi_device()).collect();

    let shared_edits = build_shared_edits();

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
    num_xcc: u32,
    vram_bytes: u64,
    gfx_target_version: u32,
    io_links: Vec<KfdIoLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KfdIoLink {
    node_to: u32,
    link_type: u32,
    weight: u32,
    max_bandwidth: u64,
}

fn discover_kfd_gpus() -> Vec<KfdGpuNode> {
    discover_kfd_gpus_from_root(Path::new(KFD_TOPOLOGY_ROOT))
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
            num_xcc: props.get("num_xcc").and_then(|s| parse_u32(s)).unwrap_or(0),
            vram_bytes,
            gfx_target_version: props
                .get("gfx_target_version")
                .and_then(|s| parse_u32(s))
                .unwrap_or(0),
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

fn read_vram_bytes(node_path: &Path) -> u64 {
    let mem_banks_dir = node_path.join("mem_banks");
    let Ok(entries) = std::fs::read_dir(&mem_banks_dir) else {
        return 0;
    };

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
            return props
                .get("size_in_bytes")
                .and_then(|s| parse_u64(s))
                .unwrap_or(0);
        }
    }

    0
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
            max_bandwidth: props
                .get("max_bandwidth")
                .and_then(|s| parse_u64(s))
                .unwrap_or(0),
        });
    }

    links
}

fn parse_kv_file(path: &Path) -> Option<HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(' ')?;
        map.insert(key.to_string(), value.to_string());
    }
    Some(map)
}

fn bdf_from_location_id(location_id: u64, domain: u32) -> String {
    let func = location_id & 0x07;
    let dev = (location_id >> 3) & 0x1f;
    let bus = (location_id >> 8) & 0xff;
    format!("{:04x}:{:02x}:{:02x}.{}", domain, bus, dev, func)
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
    fn to_cdi_device(&self) -> CdiDevice {
        let render_path = format!("/dev/dri/renderD{}", self.render_minor);
        let card_path = format!("/dev/dri/card{}", self.card_id.unwrap_or(self.device_index));

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
        };

        let render_stat = stat_device_node(&render_path);
        let card_stat = stat_device_node(&card_path);

        let device_nodes = vec![
            DeviceNode {
                path: render_path,
                host_path: None,
                r#type: Some("c".into()),
                major: render_stat.as_ref().map(|s| s.major),
                minor: render_stat.as_ref().map(|s| s.minor),
                file_mode: render_stat.as_ref().map(|s| s.file_mode),
                permissions: Some("rwm".into()),
                uid: render_stat.as_ref().map(|s| s.uid),
                gid: render_stat.as_ref().map(|s| s.gid),
            },
            DeviceNode {
                path: card_path,
                host_path: None,
                r#type: Some("c".into()),
                major: card_stat.as_ref().map(|s| s.major),
                minor: card_stat.as_ref().map(|s| s.minor),
                file_mode: card_stat.as_ref().map(|s| s.file_mode),
                permissions: Some("rwm".into()),
                uid: card_stat.as_ref().map(|s| s.uid),
                gid: card_stat.as_ref().map(|s| s.gid),
            },
        ];

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

fn build_shared_edits() -> ContainerEdits {
    let mut edits = ContainerEdits::default();

    if Path::new("/dev/kfd").exists() {
        let kfd_stat = stat_device_node("/dev/kfd");
        edits.device_nodes.push(DeviceNode {
            path: "/dev/kfd".into(),
            host_path: None,
            r#type: Some("c".into()),
            major: kfd_stat.as_ref().map(|s| s.major),
            minor: kfd_stat.as_ref().map(|s| s.minor),
            file_mode: kfd_stat.as_ref().map(|s| s.file_mode),
            permissions: Some("rwm".into()),
            uid: kfd_stat.as_ref().map(|s| s.uid),
            gid: kfd_stat.as_ref().map(|s| s.gid),
        });
    }

    for lib_path in &["/opt/rocm/lib", "/opt/rocm/lib64"] {
        if Path::new(lib_path).is_dir() {
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

    edits.additional_gids = gpu_supplementary_gids_from_group_file("/etc/group");
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

fn gpu_supplementary_gids_from_group_file(path: &str) -> Vec<u32> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut gids: Vec<u32> = GPU_SUPPLEMENTARY_GROUPS
        .iter()
        .filter_map(|name| lookup_group_gid_in(&content, name))
        .collect();
    gids.sort_unstable();
    gids.dedup();
    gids
}

fn discover_amd_gpus() -> Vec<DiscoveredGpu> {
    let kfd_nodes = discover_kfd_gpus();
    if kfd_nodes.is_empty() {
        return Vec::new();
    }

    let node_id_to_index: HashMap<u32, usize> = kfd_nodes
        .iter()
        .enumerate()
        .map(|(i, g)| (g.node_id, i))
        .collect();

    kfd_nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let device_index = node.render_minor.saturating_sub(128);
            let render_device = format!("/sys/class/drm/renderD{}", node.render_minor);
            let device_path = Path::new(&render_device).join("device");

            let gpu_type = if node.device_id != 0 {
                detect_amd_gpu_type_from_id(node.device_id)
            } else if let Some(name) = read_sysfs_string(&device_path.join("product_name")) {
                normalize_gpu_name(&name)
            } else {
                "amdgpu".into()
            };
            let memory_mb = node.vram_bytes / (1024 * 1024);
            let numa_node = read_numa_node(&device_path);
            let pci_bdf = Some(bdf_from_location_id(node.location_id, node.domain));
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
            let card_id = find_card_for_render_minor(node.render_minor);

            debug!(
                device_index,
                render_minor = node.render_minor,
                gpu_type = %gpu_type,
                memory_mb,
                numa_node = ?numa_node,
                pci_bdf = ?pci_bdf,
                compute_partition = ?compute_partition,
                "discovered AMD GPU via KFD topology"
            );

            DiscoveredGpu {
                device_index,
                render_minor: node.render_minor,
                card_id,
                gpu_type,
                memory_mb,
                numa_node,
                pci_bdf,
                link_type,
                links,
                compute_partition,
                memory_partition,
                unique_id,
            }
        })
        .collect()
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

fn find_card_for_render_minor(render_minor: u32) -> Option<u32> {
    let render_name = format!("renderD{render_minor}");
    let drm_dir = Path::new("/sys/class/drm");
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

fn read_numa_node(device_path: &Path) -> Option<u32> {
    read_sysfs_string(&device_path.join("numa_node")).and_then(|s| {
        let val: i32 = s.parse().ok()?;
        if val >= 0 {
            Some(val as u32)
        } else {
            None
        }
    })
}

fn read_sysfs_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(unix)]
struct DeviceStat {
    major: i64,
    minor: i64,
    file_mode: u32,
    uid: u32,
    gid: u32,
}

#[cfg(unix)]
fn stat_device_node(path: &str) -> Option<DeviceStat> {
    let meta = std::fs::metadata(path).ok()?;
    let rdev = meta.rdev();
    if rdev == 0 {
        return None;
    }
    let major = (((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)) as i64;
    let minor = ((rdev & 0xff) | ((rdev >> 12) & !0xff)) as i64;
    let mode = meta.mode();
    // Lower 12 bits: file permission bits (e.g. 0o666)
    let file_mode = mode & 0o7777;
    let uid = meta.uid();
    let gid = meta.gid();
    Some(DeviceStat {
        major,
        minor,
        file_mode,
        uid,
        gid,
    })
}

#[cfg(test)]
mod tests {
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
    fn test_bdf_from_location_id() {
        assert_eq!(bdf_from_location_id(1280, 0), "0000:05:00.0");
        assert_eq!(bdf_from_location_id(10496, 0), "0000:29:00.0");
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
        assert_eq!(gpus[0].vram_bytes, 206_141_652_992);
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
                num_xcc: 4,
                vram_bytes: 0,
                gfx_target_version: 0,
                io_links: vec![KfdIoLink {
                    node_to: 3,
                    link_type: 2,
                    weight: 20,
                    max_bandwidth: 64000,
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
                num_xcc: 4,
                vram_bytes: 0,
                gfx_target_version: 0,
                io_links: vec![KfdIoLink {
                    node_to: 2,
                    link_type: 2,
                    weight: 20,
                    max_bandwidth: 64000,
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
            gpu_supplementary_gids_from_group_file(group.to_str().unwrap()),
            vec![44, 109]
        );
    }

    #[test]
    fn test_discover_does_not_panic() {
        let specs = discover_to_cdi();
        let _ = specs;
    }

    #[test]
    fn test_discovered_gpu_to_cdi_device() {
        let gpu = DiscoveredGpu {
            device_index: 3,
            render_minor: 131,
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

        let dev = gpu.to_cdi_device();
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
    fn test_build_shared_edits_kfd() {
        let _ = build_shared_edits();
    }

    #[test]
    fn test_discovered_spec_structure() {
        let gpu = DiscoveredGpu {
            device_index: 0,
            render_minor: 128,
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

        let devices = vec![gpu.to_cdi_device()];
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
        assert_eq!(kfd_nodes[0].vram_bytes / (1024 * 1024), 196592);
        assert_eq!(
            bdf_from_location_id(kfd_nodes[0].location_id, kfd_nodes[0].domain),
            "0000:05:00.0"
        );
    }
}
