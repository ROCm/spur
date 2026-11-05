// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::sysfs::{read_tx_counters, Inventory, RdmaDevice};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// The check could not be evaluated on this host; never fails the node.
    Skip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub check: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Finding {
    fn new(check: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            check,
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Required RDMA device count. When unset, the GPU count is used if
    /// `match_gpus` is true and GPUs are visible.
    pub expected_nics: Option<usize>,
    pub match_gpus: bool,
}

pub fn evaluate(inv: &Inventory, opts: &Options) -> Vec<Finding> {
    let Some(devices) = &inv.rdma else {
        return vec![Finding::new(
            "rdma_devices",
            Status::Skip,
            "/sys/class/infiniband not present",
        )];
    };
    vec![
        device_count(devices, inv.gpu_count, opts),
        port_state(devices),
        link_rate(devices),
        netdev_mtu(devices),
    ]
}

pub fn has_failures(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.status == Status::Fail)
}

fn device_count(devices: &[RdmaDevice], gpus: Option<usize>, opts: &Options) -> Finding {
    let expected = opts
        .expected_nics
        .or_else(|| gpus.filter(|_| opts.match_gpus).filter(|g| *g > 0));
    match expected {
        Some(want) if want != devices.len() => Finding::new(
            "rdma_devices",
            Status::Fail,
            format!("expected {want} RDMA devices, found {}", devices.len()),
        ),
        Some(want) => Finding::new("rdma_devices", Status::Pass, format!("{want} devices")),
        None if devices.is_empty() => Finding::new(
            "rdma_devices",
            Status::Skip,
            "no RDMA devices, none expected",
        ),
        None => Finding::new(
            "rdma_devices",
            Status::Pass,
            format!("{} devices (no expected count)", devices.len()),
        ),
    }
}

fn port_state(devices: &[RdmaDevice]) -> Finding {
    let bad: Vec<String> = devices
        .iter()
        .flat_map(|d| {
            d.ports
                .iter()
                .filter(|p| !p.is_active())
                .map(move |p| format!("{}/{}={}", d.name, p.num, p.state))
        })
        .collect();
    if bad.is_empty() {
        Finding::new("port_state", Status::Pass, "all ports ACTIVE")
    } else {
        Finding::new(
            "port_state",
            Status::Fail,
            format!("not ACTIVE: {}", bad.join(", ")),
        )
    }
}

fn active_ports(
    devices: &[RdmaDevice],
) -> impl Iterator<Item = (&RdmaDevice, &crate::sysfs::Port)> {
    devices.iter().flat_map(|d| {
        d.ports
            .iter()
            .filter(|p| p.is_active())
            .map(move |p| (d, p))
    })
}

fn link_rate(devices: &[RdmaDevice]) -> Finding {
    let rates: Vec<(String, f64)> = active_ports(devices)
        .filter_map(|(d, p)| Some((format!("{}/{}", d.name, p.num), p.rate_gbps?)))
        .collect();
    uniform("link_rate", &rates, "Gb/s")
}

fn netdev_mtu(devices: &[RdmaDevice]) -> Finding {
    let mtus: Vec<(String, f64)> = active_ports(devices)
        .filter_map(|(d, p)| Some((format!("{}/{}", d.name, p.num), f64::from(p.mtu?))))
        .collect();
    uniform("netdev_mtu", &mtus, "")
}

/// Pass when every sample is equal, fail listing the odd ones out, skip when
/// there is nothing to compare.
fn uniform(check: &'static str, samples: &[(String, f64)], unit: &str) -> Finding {
    let distinct: BTreeSet<u64> = samples.iter().map(|(_, v)| v.to_bits()).collect();
    match distinct.len() {
        0 => Finding::new(check, Status::Skip, "no readable values on ACTIVE ports"),
        1 => Finding::new(
            check,
            Status::Pass,
            format!("{}{unit} on {} ports", samples[0].1, samples.len()),
        ),
        _ => {
            let listing: Vec<String> = samples
                .iter()
                .map(|(n, v)| format!("{n}={v}{unit}"))
                .collect();
            Finding::new(
                check,
                Status::Fail,
                format!("mixed: {}", listing.join(", ")),
            )
        }
    }
}

/// Flags ACTIVE ports whose tx counters do not advance while `run` generates
/// traffic through them. A NIC can report ACTIVE with a dead control path, so
/// link state alone does not prove it can send. `run` receives the device name
/// and port number and returns whether the traffic command succeeded.
pub fn tx_probe(
    root: &Path,
    devices: &[RdmaDevice],
    mut run: impl FnMut(&str, u32) -> io::Result<bool>,
) -> Finding {
    const CHECK: &str = "tx_probe";
    let mut stalled = Vec::new();
    let mut errors = Vec::new();
    let mut probed = 0;
    for (dev, port) in active_ports(devices) {
        let id = format!("{}/{}", dev.name, port.num);
        let before = read_tx_counters(root, &dev.name, port.num);
        if before.is_empty() {
            continue;
        }
        match run(&dev.name, port.num) {
            Ok(true) => {}
            Ok(false) => {
                errors.push(format!("{id}: probe command failed"));
                continue;
            }
            Err(e) => {
                errors.push(format!("{id}: probe command did not run: {e}"));
                continue;
            }
        }
        probed += 1;
        let after = read_tx_counters(root, &dev.name, port.num);
        let advanced = before
            .iter()
            .any(|(name, b)| after.iter().any(|(n, a)| n == name && a > b));
        if !advanced {
            stalled.push(id);
        }
    }
    if !errors.is_empty() || !stalled.is_empty() {
        let mut parts = errors;
        if !stalled.is_empty() {
            parts.push(format!(
                "tx counters did not advance: {}",
                stalled.join(", ")
            ));
        }
        return Finding::new(CHECK, Status::Fail, parts.join("; "));
    }
    if probed == 0 {
        return Finding::new(CHECK, Status::Skip, "no ACTIVE port exposes tx counters");
    }
    Finding::new(CHECK, Status::Pass, format!("{probed} ports transmitted"))
}
