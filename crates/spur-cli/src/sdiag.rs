// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_core::job::JobState as CoreJobState;
use spur_core::node::NodeState as CoreNodeState;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{JobMetrics, NodeMetrics};

/// Display scheduler diagnostics and statistics.
#[derive(Parser, Debug)]
#[command(name = "sdiag", about = "Display scheduling diagnostics")]
pub struct SdiagArgs {
    /// Don't print header
    #[arg(long)]
    pub noheader: bool,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SdiagArgs::try_parse_from(&args)?;

    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    let ping_resp = client.ping(()).await.context("failed to ping controller")?;
    let ping = ping_resp.into_inner();

    let job_metrics = client
        .get_job_metrics(())
        .await
        .context("failed to get job metrics")?
        .into_inner();

    let node_metrics = client
        .get_node_metrics(())
        .await
        .context("failed to get node metrics")?
        .into_inner();

    let server_time = ping
        .server_time
        .as_ref()
        .map(|t| {
            chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32)
                .unwrap_or_default()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "N/A".into());

    if !args.noheader {
        println!("***********************************************");
        println!("sdiag output at {}", server_time);
        println!("***********************************************");
        println!();
    }

    println!("Server Information:");
    println!("  Hostname          : {}", ping.hostname);
    println!("  Version           : {}", ping.version);
    println!("  Server Time       : {}", server_time);

    if !ping.federation_peers.is_empty() {
        println!("  Federation Peers  : {}", ping.federation_peers.join(", "));
    }

    print_job_statistics(&job_metrics);
    print_node_statistics(&node_metrics);

    Ok(())
}

fn job_count(metrics: &JobMetrics, state: CoreJobState) -> u64 {
    let wire = state.to_proto_i32();
    metrics
        .by_state
        .iter()
        .find(|e| e.state == wire)
        .map(|e| e.count)
        .unwrap_or(0)
}

fn node_count(metrics: &NodeMetrics, state: CoreNodeState) -> u64 {
    let wire = state.to_proto_i32();
    metrics
        .by_state
        .iter()
        .find(|e| e.state == wire)
        .map(|e| e.count)
        .unwrap_or(0)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    if bytes >= GIB as u64 {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else if bytes >= MIB as u64 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= KIB as u64 {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn print_job_statistics(metrics: &JobMetrics) {
    println!();
    println!("Job Statistics:");
    println!("  Total Jobs        : {}", metrics.total);

    for &state in &CoreJobState::ALL {
        let count = job_count(metrics, state);
        println!("  {:18}: {}", state.display(), count);
    }

    if metrics.held_pending > 0 {
        println!("  Held (pending)    : {}", metrics.held_pending);
    }

    println!("  CPUs Allocated    : {}", metrics.running_cpus);
    println!(
        "  Memory Allocated  : {}",
        format_bytes(metrics.running_memory_bytes)
    );
    println!("  GPUs Allocated    : {}", metrics.running_gpus);

    let completed = job_count(metrics, CoreJobState::Completed);
    let finished: u64 = CoreJobState::ALL
        .iter()
        .filter(|s| s.is_terminal())
        .map(|s| job_count(metrics, *s))
        .sum();
    let success_rate = if finished > 0 {
        (completed as f64 / finished as f64) * 100.0
    } else {
        0.0
    };
    let active: u64 = CoreJobState::ALL
        .iter()
        .filter(|s| s.is_active())
        .map(|s| job_count(metrics, *s))
        .sum();

    println!();
    println!("Derived Statistics:");
    println!("  Finished Jobs     : {}", finished);
    println!("  Success Rate      : {:.1}%", success_rate);
    println!(
        "  Active Jobs       : {} (running + completing + suspended)",
        active
    );
}

fn print_node_statistics(metrics: &NodeMetrics) {
    println!();
    println!("Node Statistics:");
    println!("  Total Nodes       : {}", metrics.total);

    for &state in &CoreNodeState::ALL {
        let count = node_count(metrics, state);
        println!("  {:18}: {}", state.display_upper(), count);
    }

    println!("  Total CPUs        : {}", metrics.total_cpus);
    println!("  Allocated CPUs    : {}", metrics.alloc_cpus);
    println!(
        "  Total Memory      : {}",
        format_bytes(metrics.total_memory_bytes)
    );
    println!(
        "  Allocated Memory  : {}",
        format_bytes(metrics.alloc_memory_bytes)
    );
    println!("  Total GPUs        : {}", metrics.total_gpus);
    println!("  Allocated GPUs    : {}", metrics.alloc_gpus);
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::JobState;

    #[test]
    fn job_count_reads_proto_entries() {
        let metrics = JobMetrics {
            total: 2,
            by_state: vec![
                spur_proto::proto::JobStateCount {
                    state: JobState::JobPending as i32,
                    count: 1,
                },
                spur_proto::proto::JobStateCount {
                    state: JobState::JobRunning as i32,
                    count: 1,
                },
            ],
            held_pending: 0,
            running_cpus: 4,
            running_memory_bytes: 0,
            running_gpus: 0,
        };
        assert_eq!(job_count(&metrics, CoreJobState::Pending), 1);
        assert_eq!(job_count(&metrics, CoreJobState::Running), 1);
        assert_eq!(job_count(&metrics, CoreJobState::OutOfMemory), 0);
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(8_388_608), "8.0 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GiB");
    }

    #[test]
    fn derived_job_totals_use_terminal_and_active_flags() {
        let metrics = JobMetrics {
            total: 6,
            by_state: vec![
                spur_proto::proto::JobStateCount {
                    state: JobState::JobPending as i32,
                    count: 1,
                },
                spur_proto::proto::JobStateCount {
                    state: JobState::JobRunning as i32,
                    count: 1,
                },
                spur_proto::proto::JobStateCount {
                    state: JobState::JobSuspended as i32,
                    count: 1,
                },
                spur_proto::proto::JobStateCount {
                    state: JobState::JobNodeFail as i32,
                    count: 1,
                },
                spur_proto::proto::JobStateCount {
                    state: JobState::JobCompleted as i32,
                    count: 2,
                },
            ],
            held_pending: 0,
            running_cpus: 0,
            running_memory_bytes: 0,
            running_gpus: 0,
        };

        let finished: u64 = CoreJobState::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| job_count(&metrics, *s))
            .sum();
        let active: u64 = CoreJobState::ALL
            .iter()
            .filter(|s| s.is_active())
            .map(|s| job_count(&metrics, *s))
            .sum();

        assert_eq!(finished, 3); // NODE_FAIL + 2 COMPLETED
        assert_eq!(active, 2); // RUNNING + SUSPENDED
    }
}
