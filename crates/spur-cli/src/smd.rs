// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::{GetNodesRequest, NodeInfo, NodeState};

/// Node health monitoring daemon.
///
/// Queries the controller for node status and reports unhealthy nodes.
/// Can run in continuous watch mode with configurable polling interval.
#[derive(Parser, Debug)]
#[command(name = "smd", about = "Node health monitoring")]
pub struct SmdArgs {
    /// Continuous monitoring mode
    #[arg(short = 'w', long)]
    pub watch: bool,

    /// Polling interval in seconds (only with --watch)
    #[arg(short = 'i', long, default_value = "10")]
    pub interval: u64,

    /// Only show unhealthy nodes
    #[arg(short = 'u', long)]
    pub unhealthy_only: bool,

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
    let args = crate::clap_exit::parse_or_exit::<SmdArgs>(&args);

    loop {
        let channel = crate::authclient::connect(&args.controller)
            .await
            .context("failed to connect to spurctld")?;
        let mut client = spur_proto::controller_client(channel);

        let nodes = client
            .get_nodes(GetNodesRequest {
                states: Vec::new(),
                partition: String::new(),
                nodelist: String::new(),
            })
            .await
            .context("failed to get nodes")?
            .into_inner()
            .nodes;

        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S");
        println!("=== Node Health Report ({}) ===", now);
        println!(
            "{:<20} {:<10} {:<8} {:<12} REASON",
            "NODE", "STATE", "LOAD", "FREE_MEM_MB"
        );

        let mut unhealthy_count = 0u32;
        for node in &nodes {
            let state_str = node_state_str(node);
            let is_unhealthy = is_unhealthy(node);

            if is_unhealthy {
                unhealthy_count += 1;
            }

            if args.unhealthy_only && !is_unhealthy {
                continue;
            }

            let reason = if node.state_reason.is_empty() {
                "-"
            } else {
                &node.state_reason
            };

            println!(
                "{:<20} {:<10} {:<8} {:<12} {}",
                node.name, state_str, node.cpu_load, node.free_memory_mb, reason
            );
        }

        if unhealthy_count > 0 {
            eprintln!("\n{} unhealthy node(s) detected", unhealthy_count);
        } else {
            eprintln!("\nAll {} node(s) healthy", nodes.len());
        }

        if !args.watch {
            break;
        }
        println!();
        tokio::time::sleep(tokio::time::Duration::from_secs(args.interval)).await;
    }

    Ok(())
}

/// A node the controller is not hearing from counts even before it is marked
/// Down: it takes no new work, so it is the earliest sign worth surfacing.
fn is_unhealthy(node: &NodeInfo) -> bool {
    node.not_responding
        || node.state == NodeState::NodeDown as i32
        || node.state == NodeState::NodeError as i32
        || node.state == NodeState::NodeDrain as i32
}

fn node_state_str(node: &NodeInfo) -> String {
    let base = match spur_core::node::node_overlay(node) {
        Some(spur_core::node::NodeOverlay::Reserved { maint: true }) => "maint",
        Some(spur_core::node::NodeOverlay::Reserved { maint: false }) => "resv",
        _ => spur_core::node::NodeState::from_proto_i32(node.state)
            .map(|s| s.short())
            .unwrap_or("???"),
    };
    // Slurm's `*`: the node is not responding and takes no new work.
    if node.not_responding {
        format!("{base}*")
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(state: NodeState, not_responding: bool) -> NodeInfo {
        NodeInfo {
            state: state as i32,
            not_responding,
            ..Default::default()
        }
    }

    #[test]
    fn a_not_responding_node_is_unhealthy_and_starred() {
        let silent = node(NodeState::NodeMixed, true);
        assert!(is_unhealthy(&silent));
        assert_eq!(node_state_str(&silent), "mix*");
    }

    #[test]
    fn a_responding_up_node_is_healthy() {
        let up = node(NodeState::NodeIdle, false);
        assert!(!is_unhealthy(&up));
        assert_eq!(node_state_str(&up), "idle");
    }

    #[test]
    fn down_error_and_drained_nodes_are_unhealthy() {
        for state in [
            NodeState::NodeDown,
            NodeState::NodeError,
            NodeState::NodeDrain,
        ] {
            assert!(is_unhealthy(&node(state, false)), "{state:?}");
        }
    }
}
