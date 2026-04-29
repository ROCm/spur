use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
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
    let args = SmdArgs::try_parse_from(&args)?;

    loop {
        let mut client = SlurmControllerClient::connect(args.controller.clone())
            .await
            .context("failed to connect to spurctld")?;

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
            "{:<20} {:<10} {:<8} {:<12} {}",
            "NODE", "STATE", "LOAD", "FREE_MEM_MB", "REASON"
        );

        let mut unhealthy_count = 0u32;
        for node in &nodes {
            let state_str = node_state_str(node);
            let is_unhealthy = node.state == NodeState::NodeDown as i32
                || node.state == NodeState::NodeError as i32
                || node.state == NodeState::NodeDrain as i32;

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

fn node_state_str(node: &NodeInfo) -> &'static str {
    if !node.active_reservation.is_empty() && node.state == NodeState::NodeIdle as i32 {
        return "resv";
    }
    match node.state {
        0 => "idle",
        1 => "alloc",
        2 => "mix",
        3 => "DOWN",
        4 => "drain",
        5 => "drng",
        6 => "ERROR",
        7 => "unk",
        8 => "susp",
        _ => "???",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(name: &str, state: NodeState, reservation: &str) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            state: state as i32,
            active_reservation: reservation.into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_smd_state_reserved() {
        let node = make_node("n1", NodeState::NodeIdle, "maint");
        assert_eq!(node_state_str(&node), "resv");
    }

    #[test]
    fn test_smd_state_alloc_reserved() {
        let node = make_node("n1", NodeState::NodeAllocated, "maint");
        assert_eq!(node_state_str(&node), "alloc");
    }

    #[test]
    fn test_smd_state_idle_no_reservation() {
        let node = make_node("n1", NodeState::NodeIdle, "");
        assert_eq!(node_state_str(&node), "idle");
    }

    #[test]
    fn test_smd_state_down() {
        let node = make_node("n1", NodeState::NodeDown, "");
        assert_eq!(node_state_str(&node), "DOWN");
    }
}
