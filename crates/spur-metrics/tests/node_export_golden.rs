// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Golden and catalog tests for node metrics export.

use spur_core::config::MetricsExpositionFormat;
use spur_core::node::{Node, NodeState};
use spur_core::resource::{GpuLinkType, GpuResource, ResourceAllocations, ResourceSet};
use spur_metrics::node::NodeMetricsSnapshot;
use spur_metrics::{
    encode_nodes_metrics, encode_nodes_metrics_with_format, node_state_metric_suffix,
};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn normalize_exposition(body: &str) -> String {
    // prometheus-client's Family uses a HashMap internally, so sample ordering for
    // labeled metrics is not stable. Normalize by sorting sample lines within
    // each metric block (HELP/TYPE preserved).
    let mut out = String::new();
    let mut block: Vec<&str> = Vec::new();

    fn flush(out: &mut String, block: &mut Vec<&str>) {
        if block.is_empty() {
            return;
        }

        // Preserve HELP/TYPE in their original order at the top of the block.
        let mut headers: Vec<&str> = Vec::new();
        let mut samples: Vec<&str> = Vec::new();
        for &line in block.iter() {
            if line.starts_with("# HELP ") || line.starts_with("# TYPE ") {
                headers.push(line);
            } else if !line.is_empty() {
                samples.push(line);
            }
        }
        samples.sort_unstable();

        for h in headers {
            out.push_str(h);
            out.push('\n');
        }
        for s in samples {
            out.push_str(s);
            out.push('\n');
        }

        block.clear();
    }

    for line in body.lines() {
        if line.starts_with("# HELP ") {
            flush(&mut out, &mut block);
        }
        if line == "# EOF" {
            flush(&mut out, &mut block);
            out.push_str("# EOF\n");
            continue;
        }
        block.push(line);
    }
    flush(&mut out, &mut block);
    out
}

fn resources(cpus: u32, memory_mb: u64, gpu_count: u32) -> ResourceSet {
    let mut gpus = Vec::new();
    for i in 0..gpu_count {
        gpus.push(GpuResource {
            device_id: i,
            gpu_type: "mi300x".into(),
            memory_mb: 0,
            peer_gpus: vec![],
            link_type: GpuLinkType::XGMI,
        });
    }
    ResourceSet {
        cpus,
        memory_mb,
        gpus,
        generic: Default::default(),
    }
}

fn sample_snapshot() -> NodeMetricsSnapshot {
    let mut n1 = Node::new("node-a".into(), resources(8, 16384, 2));
    n1.state = NodeState::Idle;
    n1.cpu_load = 12;
    n1.free_memory_mb = 4096;

    let mut n2 = Node::new("node-b".into(), resources(4, 8192, 0));
    n2.state = NodeState::Allocated;
    n2.alloc_resources = ResourceAllocations::with_scalar(2, 4096);

    NodeMetricsSnapshot::collect([&n1, &n2])
}

#[test]
fn golden_node_metrics_slurm_0_0_4() {
    let body = normalize_exposition(&encode_nodes_metrics_with_format(
        &sample_snapshot(),
        MetricsExpositionFormat::Slurm_0_0_4,
    ));
    let path = fixtures_dir().join("nodes.slurm_0_0_4.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn golden_node_metrics_openmetrics_1_0() {
    let body = normalize_exposition(&encode_nodes_metrics_with_format(
        &sample_snapshot(),
        MetricsExpositionFormat::OpenMetrics_1_0,
    ));
    let path = fixtures_dir().join("nodes.openmetrics_1_0.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn node_metrics_catalog_uses_spur_prefix_and_gauges() {
    let body = encode_nodes_metrics(&sample_snapshot());
    assert!(!body.contains("slurm_"));
    assert!(body.contains("spur_nodes "));
    assert!(body.contains("# TYPE spur_nodes gauge"));
    for &state in &NodeState::ALL {
        let suffix = node_state_metric_suffix(state);
        assert!(body.contains(&format!("spur_nodes_{suffix} ")));
        assert!(body.contains(&format!("# TYPE spur_nodes_{suffix} gauge")));
    }
    assert!(body.contains("spur_node_cpus{node=\"node-a\"} "));
    assert!(body.contains("spur_node_memory_bytes{node=\"node-a\"} "));
}

/// Regenerate `tests/fixtures/nodes.*.prom` after intentional encoder changes:
/// `cargo test -p spur-metrics --test node_export_golden refresh_golden_fixtures -- --ignored --exact`
#[test]
#[ignore = "manual fixture refresh"]
fn refresh_golden_fixtures() {
    let snap = sample_snapshot();
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("fixtures dir");
    std::fs::write(
        dir.join("nodes.slurm_0_0_4.prom"),
        normalize_exposition(&encode_nodes_metrics_with_format(
            &snap,
            MetricsExpositionFormat::Slurm_0_0_4,
        )),
    )
    .expect("write slurm fixture");
    std::fs::write(
        dir.join("nodes.openmetrics_1_0.prom"),
        normalize_exposition(&encode_nodes_metrics_with_format(
            &snap,
            MetricsExpositionFormat::OpenMetrics_1_0,
        )),
    )
    .expect("write openmetrics fixture");
}
