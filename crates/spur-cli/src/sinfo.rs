use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{GetNodesRequest, GetPartitionsRequest, NodeInfo, PartitionInfo};

use crate::format_engine;

/// View information about nodes and partitions.
#[derive(Parser, Debug)]
#[command(name = "sinfo", about = "View cluster information")]
pub struct SinfoArgs {
    /// Show only this partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Show only nodes in these states
    #[arg(short = 't', long)]
    pub states: Option<String>,

    /// Show only these nodes (hostlist)
    #[arg(short = 'n', long)]
    pub nodes: Option<String>,

    /// Output format
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Long format
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Node-oriented (one line per node)
    #[arg(short = 'N', long)]
    pub node_oriented: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
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
    let args = SinfoArgs::try_parse_from(&args)?;

    let fmt = if let Some(ref f) = args.format {
        f.clone()
    } else if args.long {
        "%#P %5a %.10l %.4D %.6t %.8c %.8m %N".to_string()
    } else if args.node_oriented {
        "%#N %.6D %#P %.11T %.4c %.8m %G".to_string()
    } else {
        format_engine::SINFO_DEFAULT_FORMAT.to_string()
    };

    let fields = format_engine::parse_format(&fmt, &format_engine::sinfo_header);

    let mut client = SlurmControllerClient::connect(args.controller)
        .await
        .context("failed to connect to spurctld")?;

    // Get partitions
    let partitions_resp = client
        .get_partitions(GetPartitionsRequest {
            name: args.partition.clone().unwrap_or_default(),
        })
        .await
        .context("failed to get partitions")?;

    let partitions = partitions_resp.into_inner().partitions;

    // Get nodes
    let nodes_resp = client
        .get_nodes(GetNodesRequest {
            states: Vec::new(),
            partition: args.partition.unwrap_or_default(),
            nodelist: args.nodes.unwrap_or_default(),
        })
        .await
        .context("failed to get nodes")?;

    let nodes = nodes_resp.into_inner().nodes;

    // Print header
    if !args.noheader {
        println!("{}", format_engine::format_header(&fields));
    }
    for line in render_sinfo_output(&fields, &partitions, &nodes, args.node_oriented) {
        println!("{}", line);
    }

    Ok(())
}

fn group_nodes_by_state<'a>(nodes: &[&'a NodeInfo]) -> Vec<(i32, Vec<&'a NodeInfo>)> {
    let mut groups: BTreeMap<i32, Vec<&'a NodeInfo>> = BTreeMap::new();
    for node in nodes {
        groups.entry(node.state).or_default().push(node);
    }
    groups.into_iter().collect()
}

fn render_sinfo_output(
    fields: &[format_engine::FormatField],
    partitions: &[PartitionInfo],
    nodes: &[NodeInfo],
    node_oriented: bool,
) -> Vec<String> {
    let mut lines = Vec::new();

    if node_oriented {
        for node in nodes {
            let row = format_engine::format_row(fields, &|spec| {
                resolve_node_field(node, partitions, spec)
            });
            lines.push(row);
        }
    } else {
        for part in partitions {
            let part_nodes: Vec<_> = nodes.iter().filter(|n| n.partition == part.name).collect();
            let state_groups = group_nodes_by_state(&part_nodes);

            if state_groups.is_empty() {
                let row = format_engine::format_row(fields, &|spec| {
                    resolve_partition_field(part, &[], spec)
                });
                lines.push(row);
            } else {
                for (_, group_nodes) in &state_groups {
                    let row = format_engine::format_row(fields, &|spec| {
                        resolve_partition_field(part, group_nodes, spec)
                    });
                    lines.push(row);
                }
            }
        }
    }

    lines
}

fn resolve_node_field(
    node: &spur_proto::proto::NodeInfo,
    _partitions: &[spur_proto::proto::PartitionInfo],
    spec: char,
) -> String {
    match spec {
        'N' | 'n' => node.name.clone(),
        'P' | 'R' => node.partition.clone(),
        't' | 'T' => node_state_str(node.state),
        'c' => {
            if let Some(ref r) = node.total_resources {
                r.cpus.to_string()
            } else {
                "0".into()
            }
        }
        'm' => {
            if let Some(ref r) = node.total_resources {
                r.memory_mb.to_string()
            } else {
                "0".into()
            }
        }
        'G' => {
            if let Some(ref r) = node.total_resources {
                if r.gpus.is_empty() {
                    "(null)".into()
                } else {
                    r.gpus
                        .iter()
                        .map(|g| format!("gpu:{}:{}", g.gpu_type, 1))
                        .collect::<Vec<_>>()
                        .join(",")
                }
            } else {
                "(null)".into()
            }
        }
        'D' => "1".into(),
        'a' => {
            if node.state == spur_proto::proto::NodeState::NodeDown as i32 {
                "down".into()
            } else {
                "up".into()
            }
        }
        'O' => node.cpu_load.to_string(),
        'e' => node.free_memory_mb.to_string(),
        'l' => "UNLIMITED".into(), // Would need partition context
        _ => "?".into(),
    }
}

fn resolve_partition_field(
    part: &spur_proto::proto::PartitionInfo,
    nodes: &[&spur_proto::proto::NodeInfo],
    spec: char,
) -> String {
    match spec {
        'P' | 'R' => {
            if part.is_default {
                format!("{}*", part.name)
            } else {
                part.name.clone()
            }
        }
        'a' => part.state.clone(),
        'l' => {
            if let Some(ref mt) = part.max_time {
                spur_core::config::format_time(Some((mt.seconds / 60) as u32))
            } else {
                "infinite".into()
            }
        }
        'D' => {
            // part.total_nodes may be 0 (not populated by server),
            // so fall back to the actual node count from the query.
            // If node filtering returned 0 matches (e.g. nodes lack partition
            // metadata), fall back to counting entries in the partition's
            // nodelist string, then to part.total_nodes.
            if !nodes.is_empty() {
                nodes.len().to_string()
            } else if !part.nodes.is_empty() {
                part.nodes
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .count()
                    .to_string()
            } else if part.total_nodes > 0 {
                part.total_nodes.to_string()
            } else {
                "0".into()
            }
        }
        't' | 'T' => {
            if nodes.is_empty() {
                "idle".into()
            } else {
                node_state_str(nodes[0].state)
            }
        }
        'N' => {
            if !nodes.is_empty() {
                nodes
                    .iter()
                    .map(|n| n.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                part.nodes.clone()
            }
        }
        'c' => part.total_cpus.to_string(),
        _ => "?".into(),
    }
}

fn node_state_str(state: i32) -> String {
    match state {
        0 => "idle",
        1 => "alloc",
        2 => "mix",
        3 => "down",
        4 => "drain",
        5 => "drng",
        6 => "err",
        _ => "unk",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::NodeState;

    fn make_node(name: &str, state: NodeState, partition: &str) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            state: state as i32,
            partition: partition.into(),
            ..Default::default()
        }
    }

    fn make_partition(name: &str, is_default: bool) -> PartitionInfo {
        PartitionInfo {
            name: name.into(),
            state: "up".into(),
            is_default,
            ..Default::default()
        }
    }

    fn default_fields() -> Vec<format_engine::FormatField> {
        format_engine::parse_format(
            format_engine::SINFO_DEFAULT_FORMAT,
            &format_engine::sinfo_header,
        )
    }

    #[test]
    fn test_group_nodes_by_state_mixed() {
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "p"),
            make_node("n2", NodeState::NodeIdle, "p"),
            make_node("n3", NodeState::NodeDown, "p"),
            make_node("n4", NodeState::NodeDrain, "p"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_state(&refs);

        assert_eq!(groups.len(), 3);
        // BTreeMap ordering: idle(0), down(3), drain(4)
        assert_eq!(groups[0].0, NodeState::NodeIdle as i32);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, NodeState::NodeDown as i32);
        assert_eq!(groups[1].1.len(), 1);
        assert_eq!(groups[2].0, NodeState::NodeDrain as i32);
        assert_eq!(groups[2].1.len(), 1);
    }

    #[test]
    fn test_group_nodes_by_state_all_same() {
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "p"),
            make_node("n2", NodeState::NodeIdle, "p"),
            make_node("n3", NodeState::NodeIdle, "p"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_state(&refs);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, NodeState::NodeIdle as i32);
        assert_eq!(groups[0].1.len(), 3);
    }

    #[test]
    fn test_group_nodes_by_state_empty() {
        let groups = group_nodes_by_state(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_render_partition_groups_by_state() {
        let fields = default_fields();
        let partitions = vec![make_partition("batch", true)];
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "batch"),
            make_node("n2", NodeState::NodeIdle, "batch"),
            make_node("n3", NodeState::NodeDown, "batch"),
        ];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, false);

        assert_eq!(
            lines.len(),
            2,
            "expected 2 rows (idle + down), got: {lines:?}"
        );
        assert!(
            lines[0].contains("idle"),
            "first row should be idle: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("2"),
            "idle row should show 2 nodes: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("n1"),
            "idle row should list n1: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("n2"),
            "idle row should list n2: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains("n3"),
            "idle row should not list n3: {}",
            lines[0]
        );

        assert!(
            lines[1].contains("down"),
            "second row should be down: {}",
            lines[1]
        );
        assert!(
            lines[1].contains("1"),
            "down row should show 1 node: {}",
            lines[1]
        );
        assert!(
            lines[1].contains("n3"),
            "down row should list n3: {}",
            lines[1]
        );
    }

    #[test]
    fn test_render_all_idle_single_row() {
        let fields = default_fields();
        let partitions = vec![make_partition("batch", true)];
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "batch"),
            make_node("n2", NodeState::NodeIdle, "batch"),
            make_node("n3", NodeState::NodeIdle, "batch"),
        ];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("idle"));
        assert!(lines[0].contains("3"));
    }

    #[test]
    fn test_render_empty_partition() {
        let fields = default_fields();
        let partitions = vec![make_partition("empty", false)];
        let nodes: Vec<NodeInfo> = vec![];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("idle"));
    }

    #[test]
    fn test_render_node_oriented_unchanged() {
        let fields =
            format_engine::parse_format("%#N %.6D %#P %.11T", &format_engine::sinfo_header);
        let partitions = vec![make_partition("batch", true)];
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "batch"),
            make_node("n2", NodeState::NodeDown, "batch"),
        ];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, true);
        assert_eq!(
            lines.len(),
            2,
            "node-oriented should emit one line per node"
        );
        assert!(lines[0].contains("n1"));
        assert!(lines[0].contains("idle"));
        assert!(lines[1].contains("n2"));
        assert!(lines[1].contains("down"));
    }
}
