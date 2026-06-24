// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Golden tests for RPC metrics export.

use spur_metrics::{encode_rpc_metrics, RpcOperationSnapshot, RpcStatsSnapshot};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn normalize_exposition(body: &str) -> String {
    let mut out = String::new();
    let mut block: Vec<&str> = Vec::new();

    fn flush(out: &mut String, block: &mut Vec<&str>) {
        if block.is_empty() {
            return;
        }

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

fn sample_snapshot() -> RpcStatsSnapshot {
    RpcStatsSnapshot {
        by_operation: vec![
            RpcOperationSnapshot {
                operation: "GetJobs".into(),
                count: 10,
                total_time_us: 500,
            },
            RpcOperationSnapshot {
                operation: "SubmitJob".into(),
                count: 2,
                total_time_us: 3000,
            },
        ],
    }
}

#[test]
fn golden_rpc_metrics() {
    let body = normalize_exposition(&encode_rpc_metrics(&sample_snapshot()));
    let path = fixtures_dir().join("rpc.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

/// Regenerate `tests/fixtures/rpc.prom` after intentional encoder changes:
/// `cargo test -p spur-metrics --test rpc_export_golden refresh_golden_fixtures -- --ignored --exact`
#[test]
#[ignore = "manual fixture refresh"]
fn refresh_golden_fixtures() {
    let body = normalize_exposition(&encode_rpc_metrics(&sample_snapshot()));
    std::fs::write(fixtures_dir().join("rpc.prom"), body).expect("write golden fixture");
}
