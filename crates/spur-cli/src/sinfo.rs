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

fn group_nodes_by_display_state<'a>(nodes: &[&'a NodeInfo]) -> Vec<(String, Vec<&'a NodeInfo>)> {
    let mut groups: BTreeMap<String, Vec<&'a NodeInfo>> = BTreeMap::new();
    for node in nodes {
        let key = effective_state_str(node);
        groups.entry(key).or_default().push(node);
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
            let state_groups = group_nodes_by_display_state(&part_nodes);

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
        't' | 'T' => effective_state_str(node),
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
                effective_state_str(nodes[0])
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

fn effective_state_str(node: &NodeInfo) -> String {
    if !node.active_reservation.is_empty()
        && node.state == spur_proto::proto::NodeState::NodeIdle as i32
    {
        return "resv".into();
    }
    match node.state {
        0 => "idle",
        1 => "alloc",
        2 => "mix",
        3 => "down",
        4 => "drain",
        5 => "drng",
        6 => "err",
        7 => "unk",
        8 => "susp",
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
    fn test_group_nodes_by_display_state_mixed() {
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "p"),
            make_node("n2", NodeState::NodeIdle, "p"),
            make_node("n3", NodeState::NodeDown, "p"),
            make_node("n4", NodeState::NodeDrain, "p"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_display_state(&refs);

        assert_eq!(groups.len(), 3);
        // BTreeMap ordering: alphabetical — "down", "drain", "idle"
        assert_eq!(groups[0].0, "down");
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].0, "drain");
        assert_eq!(groups[1].1.len(), 1);
        assert_eq!(groups[2].0, "idle");
        assert_eq!(groups[2].1.len(), 2);
    }

    #[test]
    fn test_group_nodes_by_display_state_all_same() {
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "p"),
            make_node("n2", NodeState::NodeIdle, "p"),
            make_node("n3", NodeState::NodeIdle, "p"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_display_state(&refs);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "idle");
        assert_eq!(groups[0].1.len(), 3);
    }

    #[test]
    fn test_group_nodes_by_display_state_empty() {
        let groups = group_nodes_by_display_state(&[]);
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

        let idle_line = lines
            .iter()
            .find(|l| l.contains("idle"))
            .expect("no idle row");
        assert!(
            idle_line.contains("2"),
            "idle row should show 2 nodes: {idle_line}"
        );
        assert!(
            idle_line.contains("n1"),
            "idle row should list n1: {idle_line}"
        );
        assert!(
            idle_line.contains("n2"),
            "idle row should list n2: {idle_line}"
        );
        assert!(
            !idle_line.contains("n3"),
            "idle row should not list n3: {idle_line}"
        );

        let down_line = lines
            .iter()
            .find(|l| l.contains("down"))
            .expect("no down row");
        assert!(
            down_line.contains("1"),
            "down row should show 1 node: {down_line}"
        );
        assert!(
            down_line.contains("n3"),
            "down row should list n3: {down_line}"
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

    // --- effective_state_str tests ---

    fn make_reserved_node(
        name: &str,
        state: NodeState,
        partition: &str,
        reservation: &str,
    ) -> NodeInfo {
        let mut n = make_node(name, state, partition);
        n.active_reservation = reservation.into();
        n
    }

    #[test]
    fn test_effective_state_idle_reserved() {
        let node = make_reserved_node("n1", NodeState::NodeIdle, "p", "maint");
        assert_eq!(effective_state_str(&node), "resv");
    }

    #[test]
    fn test_effective_state_alloc_reserved() {
        let node = make_reserved_node("n1", NodeState::NodeAllocated, "p", "maint");
        assert_eq!(effective_state_str(&node), "alloc");
    }

    #[test]
    fn test_effective_state_mixed_reserved() {
        let node = make_reserved_node("n1", NodeState::NodeMixed, "p", "maint");
        assert_eq!(effective_state_str(&node), "mix");
    }

    #[test]
    fn test_effective_state_all_base_states() {
        let cases = [
            (NodeState::NodeIdle, "idle"),
            (NodeState::NodeAllocated, "alloc"),
            (NodeState::NodeMixed, "mix"),
            (NodeState::NodeDown, "down"),
            (NodeState::NodeDrain, "drain"),
            (NodeState::NodeDraining, "drng"),
            (NodeState::NodeError, "err"),
            (NodeState::NodeUnknown, "unk"),
            (NodeState::NodeSuspended, "susp"),
        ];
        for (state, expected) in cases {
            let node = make_node("n1", state, "p");
            assert_eq!(
                effective_state_str(&node),
                expected,
                "state {:?} should display as {}",
                state,
                expected
            );
        }
    }

    // --- grouping tests ---

    #[test]
    fn test_group_separates_reserved_idle_from_idle() {
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "p"),
            make_node("n2", NodeState::NodeIdle, "p"),
            make_reserved_node("n3", NodeState::NodeIdle, "p", "maint"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_display_state(&refs);

        assert_eq!(groups.len(), 2, "expected idle + resv groups: {groups:?}");
        assert_eq!(groups[0].0, "idle");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "resv");
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn test_group_alloc_reserved_stays_with_alloc() {
        let nodes = vec![
            make_node("n1", NodeState::NodeAllocated, "p"),
            make_reserved_node("n2", NodeState::NodeAllocated, "p", "maint"),
        ];
        let refs: Vec<&NodeInfo> = nodes.iter().collect();
        let groups = group_nodes_by_display_state(&refs);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "alloc");
        assert_eq!(groups[0].1.len(), 2);
    }

    // --- render integration tests ---

    #[test]
    fn test_render_reserved_and_idle_rows() {
        let fields = default_fields();
        let partitions = vec![make_partition("default", true)];
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "default"),
            make_node("n2", NodeState::NodeIdle, "default"),
            make_reserved_node("n3", NodeState::NodeIdle, "default", "maint"),
        ];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, false);
        assert_eq!(
            lines.len(),
            2,
            "expected 2 rows (idle + resv), got: {lines:?}"
        );

        let idle_line = lines
            .iter()
            .find(|l| l.contains("idle"))
            .expect("no idle row");
        assert!(idle_line.contains("n1"), "idle row should list n1");
        assert!(idle_line.contains("n2"), "idle row should list n2");
        assert!(!idle_line.contains("n3"), "idle row should not list n3");

        let resv_line = lines
            .iter()
            .find(|l| l.contains("resv"))
            .expect("no resv row");
        assert!(resv_line.contains("n3"), "resv row should list n3");
        assert!(!resv_line.contains("n1"), "resv row should not list n1");
    }

    #[test]
    fn test_render_node_oriented_reserved() {
        let fields =
            format_engine::parse_format("%#N %.6D %#P %.11T", &format_engine::sinfo_header);
        let partitions = vec![make_partition("batch", true)];
        let nodes = vec![
            make_node("n1", NodeState::NodeIdle, "batch"),
            make_reserved_node("n2", NodeState::NodeIdle, "batch", "maint"),
        ];

        let lines = render_sinfo_output(&fields, &partitions, &nodes, true);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("n1"));
        assert!(lines[0].contains("idle"));
        assert!(lines[1].contains("n2"));
        assert!(lines[1].contains("resv"));
    }
}
