// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Read-only inventory of RDMA devices and GPUs from sysfs. Every path is
//! resolved under a caller-supplied root so tests can use fixture trees.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub num: u32,
    /// Normalized state name, e.g. "ACTIVE" or "DOWN".
    pub state: String,
    pub rate_gbps: Option<f64>,
    pub netdev: Option<String>,
    pub mtu: Option<u32>,
}

impl Port {
    pub fn is_active(&self) -> bool {
        self.state == "ACTIVE"
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RdmaDevice {
    pub name: String,
    pub ports: Vec<Port>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inventory {
    /// `None` when `/sys/class/infiniband` does not exist on this host.
    pub rdma: Option<Vec<RdmaDevice>>,
    /// `None` when the KFD topology is not readable.
    pub gpu_count: Option<usize>,
}

pub fn collect(root: &Path) -> Inventory {
    Inventory {
        rdma: collect_rdma(root),
        gpu_count: count_gpus(root),
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn sorted_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    Some(entries)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn collect_rdma(root: &Path) -> Option<Vec<RdmaDevice>> {
    let devices = sorted_entries(&root.join("sys/class/infiniband"))?;
    Some(
        devices
            .iter()
            .map(|dev| RdmaDevice {
                name: file_name(dev),
                ports: collect_ports(root, dev),
            })
            .collect(),
    )
}

fn collect_ports(root: &Path, dev: &Path) -> Vec<Port> {
    let mut ports: Vec<Port> = sorted_entries(&dev.join("ports"))
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            let num = file_name(p).parse().ok()?;
            Some(read_port(root, dev, p, num))
        })
        .collect();
    ports.sort_by_key(|p| p.num);
    ports
}

fn read_port(root: &Path, dev: &Path, port_dir: &Path, num: u32) -> Port {
    let netdev = port_netdev(dev, port_dir);
    let mtu = netdev
        .as_deref()
        .and_then(|n| read_trimmed(&root.join("sys/class/net").join(n).join("mtu")))
        .and_then(|s| s.parse().ok());
    Port {
        num,
        state: read_trimmed(&port_dir.join("state"))
            .map(|s| parse_state(&s))
            .unwrap_or_else(|| "UNKNOWN".to_string()),
        rate_gbps: read_trimmed(&port_dir.join("rate")).and_then(|s| parse_rate_gbps(&s)),
        netdev,
        mtu,
    }
}

/// RoCE ports expose their netdev through the GID attributes; fall back to the
/// PCI function's netdev for providers that do not populate them.
fn port_netdev(dev: &Path, port_dir: &Path) -> Option<String> {
    read_trimmed(&port_dir.join("gid_attrs/ndevs/0"))
        .filter(|n| !n.is_empty())
        .or_else(|| {
            let nets = sorted_entries(&dev.join("device/net"))?;
            match nets.as_slice() {
                [only] => Some(file_name(only)),
                _ => None,
            }
        })
}

/// Sysfs reports "4: ACTIVE"; keep only the name.
fn parse_state(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).trim().to_uppercase()
}

/// Sysfs reports "400 Gb/sec (4X NDR)"; keep the leading number.
fn parse_rate_gbps(raw: &str) -> Option<f64> {
    raw.split_whitespace().next()?.parse().ok()
}

/// GPU nodes in the KFD topology have SIMDs; CPU nodes report zero.
fn count_gpus(root: &Path) -> Option<usize> {
    let nodes = sorted_entries(&root.join("sys/class/kfd/kfd/topology/nodes"))?;
    Some(
        nodes
            .iter()
            .filter(|n| simd_count(&n.join("properties")) > 0)
            .count(),
    )
}

fn simd_count(properties: &Path) -> u64 {
    read_trimmed(properties)
        .and_then(|s| {
            s.lines().find_map(|l| {
                let (k, v) = l.split_once(char::is_whitespace)?;
                (k == "simd_count").then(|| v.trim().parse().ok())?
            })
        })
        .unwrap_or(0)
}

/// Counters that advance when a port transmits. Providers expose different
/// subsets, so a port is judged on whichever of these it has.
const TX_COUNTERS: &[&str] = &[
    "counters/port_xmit_data",
    "counters/port_xmit_packets",
    "hw_counters/tx_bytes",
    "hw_counters/tx_pkts",
    "hw_counters/tx_roce_pkts",
];

pub fn read_tx_counters(root: &Path, dev: &str, port: u32) -> Vec<(&'static str, u64)> {
    let dir = root
        .join("sys/class/infiniband")
        .join(dev)
        .join("ports")
        .join(port.to_string());
    TX_COUNTERS
        .iter()
        .filter_map(|c| {
            let v = read_trimmed(&dir.join(c))?.parse().ok()?;
            Some((*c, v))
        })
        .collect()
}
