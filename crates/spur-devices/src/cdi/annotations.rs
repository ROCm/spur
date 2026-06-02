// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use crate::types::LinkType;

pub const PREFIX: &str = "spur.amd.com/";

pub const GPU_TYPE: &str = "spur.amd.com/gpu-type";

pub const MEMORY_MB: &str = "spur.amd.com/memory-mb";

pub const NUMA_NODE: &str = "spur.amd.com/numa-node";

pub const CORES: &str = "spur.amd.com/cores";

pub const LINKS: &str = "spur.amd.com/links";

pub const LINK_TYPE: &str = "spur.amd.com/link-type";

pub const PCI_BDF: &str = "spur.amd.com/pci-bdf";

pub const AUTO_DETECTED: &str = "spur.amd.com/auto-detected";

pub const COMPUTE_PARTITION: &str = "spur.amd.com/compute-partition";

pub const MEMORY_PARTITION: &str = "spur.amd.com/memory-partition";

pub const UNIQUE_ID: &str = "spur.amd.com/unique-id";

pub fn parse_link_type(s: &str) -> Option<LinkType> {
    match s.trim().to_lowercase().as_str() {
        "pcie" => Some(LinkType::Pcie),
        "xgmi" => Some(LinkType::Xgmi),
        "nvlink" => Some(LinkType::Nvlink),
        _ => None,
    }
}

pub fn format_link_type(link_type: LinkType) -> String {
    match link_type {
        LinkType::Pcie => "pcie".into(),
        LinkType::Xgmi => "xgmi".into(),
        LinkType::Nvlink => "nvlink".into(),
    }
}

pub fn parse_cores(s: &str) -> Option<Vec<u32>> {
    let mut cores = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start.trim().parse().ok()?;
            let end: u32 = end.trim().parse().ok()?;
            for i in start..=end {
                cores.push(i);
            }
        } else {
            cores.push(part.parse().ok()?);
        }
    }
    cores.sort_unstable();
    cores.dedup();
    Some(cores)
}

pub fn parse_links(s: &str) -> Option<Vec<i32>> {
    s.split(',').map(|p| p.trim().parse::<i32>().ok()).collect()
}

pub fn parse_memory_mb(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

pub struct DeviceMetadata {
    pub gpu_type: Option<String>,
    pub memory_mb: u64,
    pub numa_node: Option<u32>,
    pub cores: Option<Vec<u32>>,
    pub links: Option<Vec<i32>>,
    pub link_type: Option<LinkType>,
    pub pci_bdf: Option<String>,
    pub auto_detected: bool,
    pub compute_partition: Option<String>,
    pub memory_partition: Option<String>,
    pub unique_id: Option<String>,
}

impl DeviceMetadata {
    pub fn from_annotations(annotations: &HashMap<String, String>) -> Self {
        Self {
            gpu_type: annotations.get(GPU_TYPE).cloned(),
            memory_mb: annotations
                .get(MEMORY_MB)
                .and_then(|s| parse_memory_mb(s))
                .unwrap_or(0),
            numa_node: annotations.get(NUMA_NODE).and_then(|s| s.parse().ok()),
            cores: annotations.get(CORES).and_then(|s| parse_cores(s)),
            links: annotations.get(LINKS).and_then(|s| parse_links(s)),
            link_type: annotations.get(LINK_TYPE).and_then(|s| parse_link_type(s)),
            pci_bdf: annotations.get(PCI_BDF).cloned(),
            auto_detected: annotations
                .get(AUTO_DETECTED)
                .map(|s| s == "true")
                .unwrap_or(false),
            compute_partition: annotations.get(COMPUTE_PARTITION).cloned(),
            memory_partition: annotations.get(MEMORY_PARTITION).cloned(),
            unique_id: annotations.get(UNIQUE_ID).cloned(),
        }
    }

    pub fn to_annotations(&self) -> HashMap<String, String> {
        let mut ann = HashMap::new();
        if let Some(ref t) = self.gpu_type {
            ann.insert(GPU_TYPE.into(), t.clone());
        }
        if self.memory_mb > 0 {
            ann.insert(MEMORY_MB.into(), self.memory_mb.to_string());
        }
        if let Some(n) = self.numa_node {
            ann.insert(NUMA_NODE.into(), n.to_string());
        }
        if let Some(ref c) = self.cores {
            ann.insert(CORES.into(), format_cores(c));
        }
        if let Some(ref l) = self.links {
            ann.insert(
                LINKS.into(),
                l.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        if let Some(lt) = self.link_type {
            ann.insert(LINK_TYPE.into(), format_link_type(lt));
        }
        if let Some(ref b) = self.pci_bdf {
            ann.insert(PCI_BDF.into(), b.clone());
        }
        if self.auto_detected {
            ann.insert(AUTO_DETECTED.into(), "true".into());
        }
        if let Some(ref cp) = self.compute_partition {
            ann.insert(COMPUTE_PARTITION.into(), cp.clone());
        }
        if let Some(ref mp) = self.memory_partition {
            ann.insert(MEMORY_PARTITION.into(), mp.clone());
        }
        if let Some(ref uid) = self.unique_id {
            ann.insert(UNIQUE_ID.into(), uid.clone());
        }
        ann
    }
}

fn format_cores(cores: &[u32]) -> String {
    if cores.is_empty() {
        return String::new();
    }

    let mut ranges = Vec::new();
    let mut start = cores[0];
    let mut end = cores[0];

    for &c in &cores[1..] {
        if c == end + 1 {
            end = c;
        } else {
            if start == end {
                ranges.push(start.to_string());
            } else {
                ranges.push(format!("{}-{}", start, end));
            }
            start = c;
            end = c;
        }
    }
    if start == end {
        ranges.push(start.to_string());
    } else {
        ranges.push(format!("{}-{}", start, end));
    }

    ranges.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cores_range() {
        assert_eq!(parse_cores("0-15"), Some((0..=15).collect()));
    }

    #[test]
    fn test_parse_cores_list() {
        assert_eq!(parse_cores("0,1,2,3"), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn test_parse_cores_mixed() {
        assert_eq!(
            parse_cores("0-3,8,16-18"),
            Some(vec![0, 1, 2, 3, 8, 16, 17, 18])
        );
    }

    #[test]
    fn test_parse_cores_invalid() {
        assert_eq!(parse_cores("abc"), None);
    }

    #[test]
    fn test_parse_link_type() {
        assert_eq!(parse_link_type("xgmi"), Some(LinkType::Xgmi));
        assert_eq!(parse_link_type("XGMI"), Some(LinkType::Xgmi));
        assert_eq!(parse_link_type("nvlink"), Some(LinkType::Nvlink));
        assert_eq!(parse_link_type("pcie"), Some(LinkType::Pcie));
        assert_eq!(parse_link_type("invalid"), None);
    }

    #[test]
    fn test_format_link_type() {
        assert_eq!(format_link_type(LinkType::Xgmi), "xgmi");
        assert_eq!(format_link_type(LinkType::Nvlink), "nvlink");
        assert_eq!(format_link_type(LinkType::Pcie), "pcie");
    }

    #[test]
    fn test_parse_links() {
        assert_eq!(
            parse_links("-1,4,4,0,0,0,0,0"),
            Some(vec![-1, 4, 4, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn test_parse_links_invalid() {
        assert_eq!(parse_links("a,b"), None);
    }

    #[test]
    fn test_parse_memory_mb() {
        assert_eq!(parse_memory_mb("196608"), Some(196608));
        assert_eq!(parse_memory_mb("  0  "), Some(0));
        assert_eq!(parse_memory_mb("abc"), None);
    }

    #[test]
    fn test_format_cores() {
        assert_eq!(format_cores(&[0, 1, 2, 3]), "0-3");
        assert_eq!(format_cores(&[0, 1, 2, 3, 8, 16, 17, 18]), "0-3,8,16-18");
        assert_eq!(format_cores(&[5]), "5");
        assert_eq!(format_cores(&[]), "");
    }

    #[test]
    fn test_metadata_roundtrip() {
        let meta = DeviceMetadata {
            gpu_type: Some("mi300x".into()),
            memory_mb: 196608,
            numa_node: Some(0),
            cores: Some(vec![0, 1, 2, 3]),
            links: Some(vec![-1, 4, 4, 0]),
            link_type: Some(LinkType::Xgmi),
            pci_bdf: Some("0000:c1:00.0".into()),
            auto_detected: true,
            compute_partition: Some("CPX".into()),
            memory_partition: Some("NPS4".into()),
            unique_id: Some("0123456789abcdef".into()),
        };

        let annotations = meta.to_annotations();
        let parsed = DeviceMetadata::from_annotations(&annotations);

        assert_eq!(parsed.gpu_type.as_deref(), Some("mi300x"));
        assert_eq!(parsed.memory_mb, 196608);
        assert_eq!(parsed.numa_node, Some(0));
        assert_eq!(parsed.cores, Some(vec![0, 1, 2, 3]));
        assert_eq!(parsed.links, Some(vec![-1, 4, 4, 0]));
        assert_eq!(parsed.link_type, Some(LinkType::Xgmi));
        assert_eq!(parsed.pci_bdf.as_deref(), Some("0000:c1:00.0"));
        assert!(parsed.auto_detected);
        assert_eq!(parsed.compute_partition.as_deref(), Some("CPX"));
        assert_eq!(parsed.memory_partition.as_deref(), Some("NPS4"));
        assert_eq!(parsed.unique_id.as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn test_metadata_empty_annotations() {
        let meta = DeviceMetadata::from_annotations(&HashMap::new());
        assert_eq!(meta.gpu_type, None);
        assert_eq!(meta.memory_mb, 0);
        assert!(!meta.auto_detected);
        assert_eq!(meta.compute_partition, None);
        assert_eq!(meta.memory_partition, None);
        assert_eq!(meta.unique_id, None);
    }
}
