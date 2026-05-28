// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod config;
pub mod fixture;
pub mod ssh;

pub mod gpu;
pub mod multi_node;
pub mod single_node;

use std::time::Duration;

use anyhow::{bail, Result};

use fixture::NativeHostFixture;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Parse job state from `squeue -t all` output (2-letter Slurm codes).
pub fn job_state(squeue_output: &str, job_id: u32) -> Option<String> {
    let id = job_id.to_string();
    for line in squeue_output.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&id.as_str()) {
            continue;
        }
        for field in fields.iter().skip(1) {
            if matches!(
                *field,
                "PD" | "R" | "CD" | "CG" | "F" | "CA" | "TO" | "NF" | "PR" | "S"
            ) {
                return Some(field.to_string());
            }
        }
    }
    None
}

pub async fn wait_job(fixture: &NativeHostFixture, job_id: u32, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let sq = fixture.squeue_all().await.unwrap_or_default();
        let state = job_state(&sq, job_id);
        match state.as_deref() {
            Some("CD" | "F" | "CA" | "TO") => return Ok(()),
            None => return Ok(()),
            _ => {}
        }
        if start.elapsed() > timeout {
            bail!("timeout waiting for job {job_id} (last state {:?})", state);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub async fn wait_final_state(
    fixture: &NativeHostFixture,
    job_id: u32,
    timeout: Duration,
) -> Result<String> {
    let start = std::time::Instant::now();
    let mut last = String::new();
    loop {
        let sq = fixture.squeue_all().await.unwrap_or_default();
        let state = job_state(&sq, job_id);
        match state.as_deref() {
            Some(s @ ("CD" | "F" | "CA" | "TO")) => return Ok(s.to_string()),
            Some(s) => last = s.to_string(),
            None => {
                if !last.is_empty() {
                    return Ok(last);
                }
                return Ok("GONE".into());
            }
        }
        if start.elapsed() > timeout {
            return Ok("TIMEDOUT".into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub async fn assert_eventually<F, Fut>(timeout: Duration, interval: Duration, msg: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if f().await {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out after {timeout:?}: {msg}"
        );
        tokio::time::sleep(interval).await;
    }
}
