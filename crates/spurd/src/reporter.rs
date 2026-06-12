// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
use spur_devices::{resolve_link_type, DeviceRegistry, LinkType};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{RegisterAgentRequest, ResourceSet as ProtoResourceSet};
use tracing::{debug, info, warn};

/// Discovers and reports node resources to the controller.
pub struct NodeReporter {
    pub hostname: String,
    pub controller_addr: String,
    pub resources: ResourceSet,
    pub node_address: spur_net::NodeAddress,
    pub labels: HashMap<String, String>,
    pub free_memory_mb: AtomicU64,
    pub cpu_load: AtomicU64,
}

impl NodeReporter {
    pub fn new(
        hostname: String,
        controller_addr: String,
        resources: ResourceSet,
        node_address: spur_net::NodeAddress,
        labels: HashMap<String, String>,
    ) -> Self {
        Self {
            hostname,
            controller_addr,
            resources,
            node_address,
            labels,
            free_memory_mb: AtomicU64::new(0),
            cpu_load: AtomicU64::new(0),
        }
    }

    /// Register with the controller.
    pub async fn register(&self) -> anyhow::Result<()> {
        let mut client = SlurmControllerClient::connect(self.controller_addr.clone())
            .await
            .context("failed to connect to spurctld for registration")?;

        let resp = client
            .register_agent(RegisterAgentRequest {
                hostname: self.hostname.clone(),
                resources: Some(resource_to_proto(&self.resources)),
                version: env!("CARGO_PKG_VERSION").into(),
                address: self.node_address.ip.clone(),
                port: self.node_address.port as u32,
                wg_pubkey: String::new(),
                labels: self.labels.clone(),
            })
            .await
            .context("registration failed")?;

        let inner = resp.into_inner();
        if inner.accepted {
            info!("registered with controller");
        } else {
            anyhow::bail!("controller rejected registration: {}", inner.message);
        }

        Ok(())
    }

    /// Periodic heartbeat loop.
    pub async fn heartbeat_loop(&self) {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));

        loop {
            interval.tick().await;

            let (load, free_mem) = read_system_metrics();
            self.cpu_load.store(load as u64, Ordering::Relaxed);
            self.free_memory_mb.store(free_mem, Ordering::Relaxed);

            match SlurmControllerClient::connect(self.controller_addr.clone()).await {
                Ok(mut client) => {
                    match client
                        .heartbeat(spur_proto::proto::HeartbeatRequest {
                            hostname: self.hostname.clone(),
                            cpu_load: load,
                            free_memory_mb: free_mem,
                            running_jobs: vec![],
                        })
                        .await
                    {
                        Ok(_) => debug!(load, free_mem, "heartbeat sent"),
                        Err(e) => warn!(error = %e, "heartbeat failed"),
                    }
                }
                Err(e) => warn!(error = %e, "heartbeat connection failed"),
            }
        }
    }
}

/// Discover local node resources from sysfs / /proc + device registry.
pub fn discover_resources(registry: &DeviceRegistry) -> ResourceSet {
    let cpus = discover_cpus();
    let memory_mb = discover_memory_mb();
    let gpus = gpus_from_registry(registry);

    ResourceSet {
        cpus,
        memory_mb,
        gpus,
        generic: generic_from_registry(registry),
    }
}

fn build_peer_gpus(links: Option<&[i32]>, gpu_device_ids: &[u32]) -> Vec<u32> {
    let Some(links) = links else {
        return Vec::new();
    };
    links
        .iter()
        .enumerate()
        .filter_map(|(j, &w)| {
            if w > 0 && j < gpu_device_ids.len() {
                Some(gpu_device_ids[j])
            } else {
                None
            }
        })
        .collect()
}

/// Convert injectable GPU registry entries to `GpuResource` for the scheduler.
fn gpus_from_registry(registry: &DeviceRegistry) -> Vec<GpuResource> {
    let gpu_entries: Vec<_> = registry
        .list()
        .iter()
        .filter(|e| e.is_injectable() && e.gres_name == "gpu")
        .collect();

    let gpu_device_ids: Vec<u32> = gpu_entries.iter().map(|e| e.device_id).collect();

    gpu_entries
        .iter()
        .map(|entry| GpuResource {
            device_id: entry.device_id,
            gpu_type: entry.resource_type.clone().unwrap_or_default(),
            memory_mb: entry.memory_mb,
            peer_gpus: build_peer_gpus(entry.links.as_deref(), &gpu_device_ids),
            link_type: link_type_to_gpu(resolve_link_type(entry)),
        })
        .collect()
}

fn generic_from_registry(registry: &DeviceRegistry) -> HashMap<String, u64> {
    let mut generic = HashMap::new();
    for entry in registry.list() {
        if !entry.is_countable_pool() {
            continue;
        }
        let key = match &entry.resource_type {
            Some(t) if !t.is_empty() => format!("{}:{}", entry.gres_name, t),
            _ => entry.gres_name.clone(),
        };
        *generic.entry(key).or_insert(0) += entry.capacity;
    }
    generic
}

fn link_type_to_gpu(lt: LinkType) -> GpuLinkType {
    match lt {
        LinkType::Xgmi => GpuLinkType::XGMI,
        LinkType::Nvlink => GpuLinkType::NVLink,
        LinkType::Pcie => GpuLinkType::PCIe,
    }
}

/// Count online CPUs from sysfs.
fn discover_cpus() -> u32 {
    // Try /sys/devices/system/cpu/online first
    if let Ok(online) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        if let Some(count) = parse_cpu_range(online.trim()) {
            return count;
        }
    }

    // Fallback: count /proc/cpuinfo processors
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        return cpuinfo
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count() as u32;
    }

    // Last resort
    num_cpus()
}

/// Parse "0-191" or "0-63,128-191" into a total count.
fn parse_cpu_range(s: &str) -> Option<u32> {
    let mut count = 0u32;
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start_s, end_s)) = part.split_once('-') {
            let start: u32 = start_s.parse().ok()?;
            let end: u32 = end_s.parse().ok()?;
            count += end - start + 1;
        } else {
            let _: u32 = part.parse().ok()?;
            count += 1;
        }
    }
    Some(count)
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// Read total memory from /proc/meminfo.
fn discover_memory_mb() -> u64 {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let rest = rest.trim();
                if let Some(kb_str) = rest.strip_suffix("kB") {
                    if let Ok(kb) = kb_str.trim().parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
    }
    0
}

/// Read current load average and free memory.
fn read_system_metrics() -> (u32, u64) {
    let load = read_load_avg();
    let free_mem = read_free_memory_mb();
    (load, free_mem)
}

fn read_load_avg() -> u32 {
    if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(first) = loadavg.split_whitespace().next() {
            if let Ok(load) = first.parse::<f64>() {
                return (load * 100.0) as u32;
            }
        }
    }
    0
}

fn read_free_memory_mb() -> u64 {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let rest = rest.trim();
                if let Some(kb_str) = rest.strip_suffix("kB") {
                    if let Ok(kb) = kb_str.trim().parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
    }
    0
}

pub fn allocations_to_proto(
    r: &spur_core::resource::ResourceAllocations,
) -> spur_proto::proto::ResourceAllocations {
    use std::collections::HashMap;
    spur_proto::proto::ResourceAllocations {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        devices: r
            .devices
            .iter()
            .map(|(name, devs)| {
                (
                    name.clone(),
                    spur_proto::proto::DeviceAllocations {
                        devices: devs
                            .iter()
                            .map(|d| spur_proto::proto::AllocatedDevice {
                                device_id: d.device_id,
                                count: d.count,
                            })
                            .collect(),
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
    }
}

pub fn resource_to_proto(r: &ResourceSet) -> ProtoResourceSet {
    ProtoResourceSet {
        cpus: r.cpus,
        memory_mb: r.memory_mb,
        gpus: r
            .gpus
            .iter()
            .map(|g| spur_proto::proto::GpuResource {
                device_id: g.device_id,
                gpu_type: g.gpu_type.clone(),
                memory_mb: g.memory_mb,
                peer_gpus: g.peer_gpus.clone(),
                link_type: match g.link_type {
                    GpuLinkType::XGMI => spur_proto::proto::GpuLinkType::GpuLinkXgmi as i32,
                    GpuLinkType::NVLink => spur_proto::proto::GpuLinkType::GpuLinkNvlink as i32,
                    GpuLinkType::PCIe => spur_proto::proto::GpuLinkType::GpuLinkPcie as i32,
                },
            })
            .collect(),
        generic: r.generic.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_devices::cdi::annotations;
    use spur_devices::cdi::cache::CdiCache;
    use spur_devices::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, DeviceNode};
    use spur_devices::{DeviceRegistry, GresCache, GresEntry};

    #[test]
    fn test_gpus_from_registry_link_type() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [
                    (annotations::GPU_TYPE.into(), "mi300x".into()),
                    (annotations::LINK_TYPE.into(), "xgmi".into()),
                ]
                .into(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/dri/renderD128".into(),
                        host_path: None,
                        r#type: None,
                        major: None,
                        minor: None,
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut reg = DeviceRegistry::new();
        reg.populate(&cache, &GresCache::from_entries(&[]));

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].link_type, GpuLinkType::XGMI);
    }

    #[test]
    fn test_build_peer_gpus_from_links() {
        let gpu_device_ids = [0, 1, 2, 3];
        let links = [-1, 4, 4, 0];
        assert_eq!(build_peer_gpus(Some(&links), &gpu_device_ids), vec![1, 2]);
    }

    #[test]
    fn test_build_peer_gpus_none_links() {
        let gpu_device_ids = [0, 1, 2];
        assert!(build_peer_gpus(None, &gpu_device_ids).is_empty());
    }

    #[test]
    fn test_gpus_from_registry_populates_peer_gpus() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-130]".into()),
            links: Some("-1,4,4,0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 3);
        for gpu in &gpus {
            assert_eq!(gpu.peer_gpus, vec![1, 2]);
        }
    }

    #[test]
    fn test_gpus_from_registry_link_type_inferred_from_gres_links() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-129]".into()),
            links: Some("-1,4,4,0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let gpus = gpus_from_registry(&reg);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].link_type, GpuLinkType::XGMI);
        assert_eq!(gpus[1].link_type, GpuLinkType::XGMI);
    }

    #[test]
    fn test_parse_cpu_range() {
        assert_eq!(parse_cpu_range("0-191"), Some(192));
        assert_eq!(parse_cpu_range("0-63,128-191"), Some(128));
        assert_eq!(parse_cpu_range("0"), Some(1));
        assert_eq!(parse_cpu_range("0-3"), Some(4));
    }

    #[test]
    fn test_discover_cpus() {
        let cpus = discover_cpus();
        assert!(cpus > 0);
    }

    #[test]
    fn test_discover_memory() {
        let mem = discover_memory_mb();
        assert!(mem > 0);
    }

    #[test]
    fn test_discover_resources_includes_countable_gres() {
        let mut reg = DeviceRegistry::new();
        let gres_cache = GresCache::from_entries(&[GresEntry {
            name: "bandwidth".into(),
            r#type: Some("lustre".into()),
            count: Some(4096),
            flags: vec!["count_only".into()],
            ..Default::default()
        }]);
        reg.populate(&CdiCache::new(), &gres_cache);

        let resources = discover_resources(&reg);
        assert_eq!(resources.generic.get("bandwidth:lustre"), Some(&4096));
    }
}
