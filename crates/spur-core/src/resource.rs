use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// GPU interconnect type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuLinkType {
    PCIe,
    XGMI,
    NVLink,
}

/// A single GPU resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuResource {
    pub device_id: u32,
    pub gpu_type: String,
    pub memory_mb: u64,
    pub peer_gpus: Vec<u32>,
    pub link_type: GpuLinkType,
}

/// A set of compute resources (node-level or job-level).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceSet {
    pub cpus: u32,
    pub memory_mb: u64,
    pub gpus: Vec<GpuResource>,
    pub generic: HashMap<String, u64>,
}

impl ResourceSet {
    /// Check if this resource set can satisfy a request.
    pub fn can_satisfy(&self, request: &ResourceSet) -> bool {
        if self.cpus < request.cpus || self.memory_mb < request.memory_mb {
            return false;
        }
        // Check GPU count by type
        let avail_gpus = self.gpu_counts();
        let total_avail: u32 = avail_gpus.values().sum();
        let req_gpus = request.gpu_counts();
        for (gpu_type, count) in &req_gpus {
            if gpu_type == "any" {
                // "any" type matches total GPU count regardless of type
                if total_avail < *count {
                    return false;
                }
            } else if avail_gpus.get(gpu_type).copied().unwrap_or(0) < *count {
                return false;
            }
        }
        // Check generic resources
        for (name, count) in &request.generic {
            if self.generic.get(name).copied().unwrap_or(0) < *count {
                return false;
            }
        }
        true
    }

    /// Subtract requested resources, returning the remainder.
    /// GPUs are filtered by device_id — any GPU in `used` with a matching
    /// device_id is removed from the result.
    pub fn subtract(&self, used: &ResourceSet) -> ResourceSet {
        let used_gpu_ids: std::collections::HashSet<u32> =
            used.gpus.iter().map(|g| g.device_id).collect();
        ResourceSet {
            cpus: self.cpus.saturating_sub(used.cpus),
            memory_mb: self.memory_mb.saturating_sub(used.memory_mb),
            gpus: self
                .gpus
                .iter()
                .filter(|g| !used_gpu_ids.contains(&g.device_id))
                .cloned()
                .collect(),
            generic: self
                .generic
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.saturating_sub(used.generic.get(k).copied().unwrap_or(0)),
                    )
                })
                .collect(),
        }
    }

    /// Add resources from another set, accumulating totals.
    pub fn add(&self, other: &ResourceSet) -> ResourceSet {
        let mut gpus = self.gpus.clone();
        let existing_ids: std::collections::HashSet<u32> =
            gpus.iter().map(|g| g.device_id).collect();
        for g in &other.gpus {
            if !existing_ids.contains(&g.device_id) {
                gpus.push(g.clone());
            }
        }
        let mut generic = self.generic.clone();
        for (k, v) in &other.generic {
            *generic.entry(k.clone()).or_insert(0) += v;
        }
        ResourceSet {
            cpus: self.cpus + other.cpus,
            memory_mb: self.memory_mb + other.memory_mb,
            gpus,
            generic,
        }
    }

    /// Count GPUs by type.
    pub fn gpu_counts(&self) -> HashMap<String, u32> {
        let mut counts = HashMap::new();
        for gpu in &self.gpus {
            *counts.entry(gpu.gpu_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    pub fn total_gpus(&self) -> u32 {
        self.gpus.len() as u32
    }
}

/// Parse a GRES string like "gpu:mi300x:4" or "gpu:2".
pub fn parse_gres(gres: &str) -> Option<(String, Option<String>, u32)> {
    let parts: Vec<&str> = gres.split(':').collect();
    match parts.len() {
        1 => Some((parts[0].to_string(), None, 1)),
        2 => {
            if let Ok(count) = parts[1].parse::<u32>() {
                Some((parts[0].to_string(), None, count))
            } else {
                Some((parts[0].to_string(), Some(parts[1].to_string()), 1))
            }
        }
        3 => {
            let count = parts[2].parse::<u32>().ok()?;
            Some((parts[0].to_string(), Some(parts[1].to_string()), count))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_satisfy() {
        let avail = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![
                GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![1],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 1,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![0],
                    link_type: GpuLinkType::XGMI,
                },
            ],
            generic: HashMap::new(),
        };

        let req = ResourceSet {
            cpus: 32,
            memory_mb: 128_000,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            }],
            generic: HashMap::new(),
        };

        assert!(avail.can_satisfy(&req));
    }

    #[test]
    fn test_parse_gres() {
        assert_eq!(
            parse_gres("gpu:mi300x:4"),
            Some(("gpu".into(), Some("mi300x".into()), 4))
        );
        assert_eq!(parse_gres("gpu:2"), Some(("gpu".into(), None, 2)));
        assert_eq!(parse_gres("license"), Some(("license".into(), None, 1)));
    }
}
