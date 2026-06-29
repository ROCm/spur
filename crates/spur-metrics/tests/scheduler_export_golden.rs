// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Golden tests for scheduler metrics export.

use spur_metrics::{encode_scheduler_metrics, SchedStatsSnapshot};
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

fn sample_snapshot() -> SchedStatsSnapshot {
    SchedStatsSnapshot {
        plugin: "backfill".into(),
        cycles: 10,
        cycle_total_time_us: 5000,
        cycle_last_time_us: 600,
        schedule_total_time_us: 1500,
        schedule_last_time_us: 200,
        jobs_submitted: 42,
        jobs_started: 30,
        jobs_finalized: 28,
        jobs_started_last_cycle: 3,
    }
}

#[test]
fn golden_scheduler_metrics() {
    let body = normalize_exposition(&encode_scheduler_metrics(&sample_snapshot()));
    let path = fixtures_dir().join("scheduler.prom");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &body).expect("write golden fixture");
        return;
    }

    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}
