// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::checks::{evaluate, has_failures, tx_probe, Options, Status};
use crate::sysfs::{collect, read_tx_counters};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn add_gpus(root: &Path, n: usize) {
    write(
        root,
        "sys/class/kfd/kfd/topology/nodes/0/properties",
        "simd_count 0\n",
    );
    for i in 0..n {
        let rel = format!("sys/class/kfd/kfd/topology/nodes/{}/properties", i + 1);
        write(root, &rel, "cpu_cores_count 0\nsimd_count 304\n");
    }
}

fn add_nic(root: &Path, dev: &str, state: &str, rate: &str, mtu: u32) {
    let port = format!("sys/class/infiniband/{dev}/ports/1");
    write(root, &format!("{port}/state"), state);
    write(root, &format!("{port}/rate"), rate);
    let netdev = format!("eth_{dev}");
    write(root, &format!("{port}/gid_attrs/ndevs/0"), &netdev);
    write(
        root,
        &format!("sys/class/net/{netdev}/mtu"),
        &mtu.to_string(),
    );
    write(root, &format!("{port}/counters/port_xmit_data"), "100\n");
}

fn add_healthy_nics(root: &Path, n: usize) {
    for i in 0..n {
        add_nic(
            root,
            &format!("ionic_{i}"),
            "4: ACTIVE\n",
            "400 Gb/sec (4X NDR)\n",
            9000,
        );
    }
}

fn findings(root: &Path, opts: &Options) -> Vec<(&'static str, Status, String)> {
    evaluate(&collect(root), opts)
        .into_iter()
        .map(|f| (f.check, f.status, f.detail))
        .collect()
}

fn status_of(root: &Path, opts: &Options, check: &str) -> Status {
    findings(root, opts)
        .into_iter()
        .find(|(c, _, _)| *c == check)
        .unwrap()
        .1
}

fn gpu_match() -> Options {
    Options {
        expected_nics: None,
        match_gpus: true,
    }
}

#[test]
fn healthy_node_passes_every_check() {
    let dir = TempDir::new().unwrap();
    add_gpus(dir.path(), 8);
    add_healthy_nics(dir.path(), 8);
    let result = evaluate(&collect(dir.path()), &gpu_match());
    assert_eq!(result.len(), 4);
    assert!(
        result.iter().all(|f| f.status == Status::Pass),
        "{result:?}"
    );
}

#[test]
fn missing_infiniband_class_skips_instead_of_failing() {
    let dir = TempDir::new().unwrap();
    add_gpus(dir.path(), 8);
    let result = evaluate(&collect(dir.path()), &gpu_match());
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].status, Status::Skip);
    assert!(!has_failures(&result));
}

#[test]
fn device_count_mismatch_with_gpus_fails() {
    let dir = TempDir::new().unwrap();
    add_gpus(dir.path(), 8);
    add_healthy_nics(dir.path(), 7);
    assert_eq!(
        status_of(dir.path(), &gpu_match(), "rdma_devices"),
        Status::Fail
    );
}

#[test]
fn gpu_match_can_be_disabled_and_explicit_count_wins() {
    let dir = TempDir::new().unwrap();
    add_gpus(dir.path(), 8);
    add_healthy_nics(dir.path(), 4);
    let off = Options {
        expected_nics: None,
        match_gpus: false,
    };
    assert_eq!(status_of(dir.path(), &off, "rdma_devices"), Status::Pass);
    let explicit = Options {
        expected_nics: Some(4),
        match_gpus: true,
    };
    assert_eq!(
        status_of(dir.path(), &explicit, "rdma_devices"),
        Status::Pass
    );
}

#[test]
fn no_gpus_and_no_nics_is_a_skip() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("sys/class/infiniband")).unwrap();
    assert_eq!(
        status_of(dir.path(), &gpu_match(), "rdma_devices"),
        Status::Skip
    );
}

#[test]
fn down_port_fails_and_names_the_device() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 2);
    add_nic(
        dir.path(),
        "ionic_9",
        "1: DOWN\n",
        "400 Gb/sec (4X NDR)\n",
        9000,
    );
    let result = findings(dir.path(), &Options::default());
    let (_, status, detail) = result.iter().find(|(c, _, _)| *c == "port_state").unwrap();
    assert_eq!(*status, Status::Fail);
    assert!(detail.contains("ionic_9/1=DOWN"), "{detail}");
}

#[test]
fn mixed_link_rate_among_active_ports_fails() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 2);
    add_nic(
        dir.path(),
        "ionic_9",
        "4: ACTIVE\n",
        "200 Gb/sec (4X HDR)\n",
        9000,
    );
    assert_eq!(
        status_of(dir.path(), &Options::default(), "link_rate"),
        Status::Fail
    );
}

#[test]
fn down_port_rate_is_ignored_by_rate_uniformity() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 2);
    add_nic(
        dir.path(),
        "ionic_9",
        "1: DOWN\n",
        "10 Gb/sec (1X QDR)\n",
        9000,
    );
    assert_eq!(
        status_of(dir.path(), &Options::default(), "link_rate"),
        Status::Pass
    );
}

#[test]
fn inconsistent_netdev_mtu_fails() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 2);
    add_nic(
        dir.path(),
        "ionic_9",
        "4: ACTIVE\n",
        "400 Gb/sec (4X NDR)\n",
        1500,
    );
    assert_eq!(
        status_of(dir.path(), &Options::default(), "netdev_mtu"),
        Status::Fail
    );
}

#[test]
fn netdev_falls_back_to_pci_function_when_gid_attrs_missing() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(
        root,
        "sys/class/infiniband/bnxt_re0/ports/1/state",
        "4: ACTIVE\n",
    );
    write(
        root,
        "sys/class/infiniband/bnxt_re0/device/net/enp1s0/address",
        "x",
    );
    write(root, "sys/class/net/enp1s0/mtu", "9000\n");
    let inv = collect(root);
    let port = &inv.rdma.unwrap()[0].ports[0];
    assert_eq!(port.netdev.as_deref(), Some("enp1s0"));
    assert_eq!(port.mtu, Some(9000));
    assert_eq!(port.rate_gbps, None);
}

#[test]
fn unreadable_rate_and_mtu_skip_rather_than_fail() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "sys/class/infiniband/mlx5_0/ports/1/state",
        "4: ACTIVE\n",
    );
    let opts = Options::default();
    assert_eq!(status_of(dir.path(), &opts, "link_rate"), Status::Skip);
    assert_eq!(status_of(dir.path(), &opts, "netdev_mtu"), Status::Skip);
}

fn probe_with(root: &Path, bump: impl Fn(&str) -> Option<u64>) -> (Status, String) {
    let inv = collect(root);
    let f = tx_probe(root, &inv.rdma.unwrap(), |dev, port| {
        if let Some(v) = bump(dev) {
            let rel = format!("sys/class/infiniband/{dev}/ports/{port}/counters/port_xmit_data");
            write(root, &rel, &v.to_string());
        }
        Ok(true)
    });
    (f.status, f.detail)
}

#[test]
fn tx_probe_passes_when_every_counter_advances() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 2);
    let (status, _) = probe_with(dir.path(), |_| Some(5000));
    assert_eq!(status, Status::Pass);
}

#[test]
fn tx_probe_flags_active_port_with_stalled_counter() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 3);
    let (status, detail) = probe_with(dir.path(), |dev| (dev != "ionic_1").then_some(5000));
    assert_eq!(status, Status::Fail);
    assert!(
        detail.contains("ionic_1/1") && !detail.contains("ionic_0"),
        "{detail}"
    );
}

#[test]
fn tx_probe_reports_failed_traffic_command() {
    let dir = TempDir::new().unwrap();
    add_healthy_nics(dir.path(), 1);
    let inv = collect(dir.path());
    let f = tx_probe(dir.path(), &inv.rdma.unwrap(), |_, _| Ok(false));
    assert_eq!(f.status, Status::Fail);
    assert!(f.detail.contains("probe command failed"), "{}", f.detail);
}

#[test]
fn tx_probe_skips_ports_without_counters_and_ignores_down_ports() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "sys/class/infiniband/mlx5_0/ports/1/state",
        "4: ACTIVE\n",
    );
    add_nic(dir.path(), "ionic_9", "1: DOWN\n", "400 Gb/sec\n", 9000);
    let (status, _) = probe_with(dir.path(), |_| None);
    assert_eq!(status, Status::Skip);
}

#[test]
fn tx_counters_use_whichever_the_provider_exposes() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "sys/class/infiniband/bnxt_re0/ports/1/hw_counters/tx_pkts",
        "7\n",
    );
    let counters = read_tx_counters(dir.path(), "bnxt_re0", 1);
    assert_eq!(counters, vec![("hw_counters/tx_pkts", 7)]);
}
