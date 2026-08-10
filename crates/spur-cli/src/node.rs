// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur node` subcommands for node lifecycle management.

use crate::scontrol::{is_all_node_pattern, resolve_node_names};
use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use spur_proto::proto::UpdateNodeRequest;

/// Node management commands.
#[derive(Parser, Debug)]
#[command(name = "node", about = "Manage cluster nodes")]
pub struct NodeArgs {
    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    controller: String,

    #[command(subcommand)]
    pub command: NodeCommand,
}

#[derive(Subcommand, Debug)]
pub enum NodeCommand {
    /// Set or remove labels on one or more nodes.
    ///
    /// Labels are key=value pairs used for partition routing.
    /// Append a trailing dash to remove a label (e.g., "pool-").
    Label {
        /// Node names: ALL, a comma-separated list, and/or a hostlist range (e.g. "n1,n2", "n[1-4]")
        node: String,
        /// Labels to set (key=value) or remove (key-)
        #[arg(required = true)]
        labels: Vec<String>,
    },
    /// Drain one or more nodes: stop scheduling new jobs while existing jobs finish.
    Drain {
        /// Node names: ALL, a comma-separated list, and/or a hostlist range (e.g. "n1,n2", "n[1-4]")
        node: String,
        /// Reason for draining
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove one or more nodes from the cluster entirely.
    ///
    /// ALL removes every registered node, and with --force this evicts jobs from
    /// every node.
    Remove {
        /// Node names: ALL, a comma-separated list, and/or a hostlist range (e.g. "n1,n2", "n[1-4]")
        node: String,
        /// Force removal even if jobs are running (jobs will be failed with NODE_FAIL)
        #[arg(long)]
        force: bool,
        /// Reason for removal
        #[arg(long)]
        reason: Option<String>,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = NodeArgs::try_parse_from(args)?;
    let controller = parsed.controller;
    match parsed.command {
        NodeCommand::Label { node, labels } => cmd_label(&controller, node, labels).await,
        NodeCommand::Drain { node, reason } => cmd_drain(&controller, node, reason).await,
        NodeCommand::Remove {
            node,
            force,
            reason,
        } => cmd_remove(&controller, node, force, reason).await,
    }
}

fn parse_label_args(label_args: &[String]) -> Result<(HashMap<String, String>, Vec<String>)> {
    let mut set_labels: HashMap<String, String> = HashMap::new();
    let mut remove_labels: Vec<String> = Vec::new();

    for arg in label_args {
        if let Some((k, v)) = arg.split_once('=') {
            if k.is_empty() {
                bail!("invalid label: '{arg}', key cannot be empty");
            }
            set_labels.insert(k.to_string(), v.to_string());
        } else if let Some(key) = arg.strip_suffix('-') {
            if key.is_empty() {
                bail!("invalid label removal: '{arg}'");
            }
            remove_labels.push(key.to_string());
        } else {
            bail!("invalid label format: '{arg}', expected key=value or key-");
        }
    }

    Ok((set_labels, remove_labels))
}

async fn cmd_label(controller: &str, node_pattern: String, label_args: Vec<String>) -> Result<()> {
    let (set_labels, remove_labels) = parse_label_args(&label_args)?;
    let nodes = expand_node_pattern(&node_pattern)?;
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
    let nodes = if let Some(nodes) = nodes {
        nodes
    } else {
        resolve_node_names(&mut client, &node_pattern).await?
    };

    let mut failed: Vec<String> = Vec::new();
    for node in &nodes {
        match client
            .update_node(UpdateNodeRequest {
                name: node.to_string(),
                state: None,
                reason: None,
                labels: set_labels.clone(),
                remove_labels: remove_labels.clone(),
            })
            .await
        {
            Ok(_) => {
                for (k, v) in &set_labels {
                    println!("  {node}: {k}={v}");
                }
                for k in &remove_labels {
                    println!("  {node}: {k} removed");
                }
            }
            Err(e) => {
                eprintln!("error: {node}: {e}");
                failed.push(node.clone());
            }
        }
    }

    if !failed.is_empty() {
        bail!(
            "failed on {} of {} node(s): {}",
            failed.len(),
            nodes.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

async fn cmd_drain(controller: &str, node_pattern: String, reason: Option<String>) -> Result<()> {
    let nodes = expand_node_pattern(&node_pattern)?;
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
    let nodes = if let Some(nodes) = nodes {
        nodes
    } else {
        resolve_node_names(&mut client, &node_pattern).await?
    };

    let mut failed: Vec<String> = Vec::new();
    for node in &nodes {
        match client
            .drain_node(spur_proto::proto::DrainNodeRequest {
                name: node.to_string(),
                reason: reason.clone().unwrap_or_default(),
            })
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                if resp.running_jobs > 0 {
                    println!(
                        "Node {node} set to draining ({} running job{} will finish first)",
                        resp.running_jobs,
                        if resp.running_jobs == 1 { "" } else { "s" }
                    );
                } else {
                    println!("Node {node} set to drain");
                }
                if let Some(r) = &reason {
                    println!("  reason: {r}");
                }
            }
            Err(e) => {
                eprintln!("error: {node}: {e}");
                failed.push(node.clone());
            }
        }
    }
    if !failed.is_empty() {
        bail!(
            "failed on {} of {} node(s): {}",
            failed.len(),
            nodes.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

async fn cmd_remove(
    controller: &str,
    node_pattern: String,
    force: bool,
    reason: Option<String>,
) -> Result<()> {
    let nodes = expand_node_pattern(&node_pattern)?;
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
    let nodes = if let Some(nodes) = nodes {
        nodes
    } else {
        resolve_node_names(&mut client, &node_pattern).await?
    };

    let mut failed: Vec<String> = Vec::new();
    for node in &nodes {
        match client
            .deregister_node(spur_proto::proto::DeregisterNodeRequest {
                name: node.to_string(),
                force,
                reason: reason.clone().unwrap_or_default(),
            })
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                if resp.evicted_jobs_count > 0 {
                    println!(
                        "Node {node} removed from cluster ({} job{} evicted)",
                        resp.evicted_jobs_count,
                        if resp.evicted_jobs_count == 1 {
                            ""
                        } else {
                            "s"
                        }
                    );
                } else {
                    println!("Node {node} removed from cluster");
                }
                if let Some(r) = &reason {
                    println!("  reason: {r}");
                }
            }
            Err(e) => {
                eprintln!("error: {node}: {e}");
                failed.push(node.clone());
            }
        }
    }
    if !failed.is_empty() {
        bail!(
            "failed on {} of {} node(s): {}",
            failed.len(),
            nodes.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

fn expand_node_pattern(pattern: &str) -> Result<Option<Vec<String>>> {
    if is_all_node_pattern(pattern) {
        return Ok(None);
    }
    spur_core::hostlist::expand(pattern)
        .map(Some)
        .context("invalid node name pattern")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_label_set() {
        let args = vec!["pool=gpu".to_string(), "tier=high".to_string()];
        let (set, remove) = parse_label_args(&args).unwrap();
        assert_eq!(set.get("pool").unwrap(), "gpu");
        assert_eq!(set.get("tier").unwrap(), "high");
        assert!(remove.is_empty());
    }

    #[test]
    fn test_parse_label_remove() {
        let args = vec!["pool-".to_string()];
        let (set, remove) = parse_label_args(&args).unwrap();
        assert!(set.is_empty());
        assert_eq!(remove, vec!["pool"]);
    }

    #[test]
    fn test_parse_label_mixed() {
        let args = vec!["env=prod".to_string(), "old_tag-".to_string()];
        let (set, remove) = parse_label_args(&args).unwrap();
        assert_eq!(set.get("env").unwrap(), "prod");
        assert_eq!(remove, vec!["old_tag"]);
    }

    #[test]
    fn test_parse_label_empty_key_set() {
        let args = vec!["=value".to_string()];
        assert!(parse_label_args(&args).is_err());
    }

    #[test]
    fn test_parse_label_empty_key_remove() {
        let args = vec!["-".to_string()];
        assert!(parse_label_args(&args).is_err());
    }

    #[test]
    fn test_parse_label_invalid_format() {
        let args = vec!["noequalsnodash".to_string()];
        assert!(parse_label_args(&args).is_err());
    }

    #[test]
    fn test_parse_label_value_ending_in_dash() {
        let args = vec!["env=prod-".to_string()];
        let (set, remove) = parse_label_args(&args).unwrap();
        assert_eq!(set.get("env").unwrap(), "prod-");
        assert!(remove.is_empty());
    }

    #[tokio::test]
    async fn label_all_resolves_registered_nodes_case_insensitively() {
        for pattern in ["ALL", "all", "All"] {
            let (addr, capture) = crate::mock_controller::spawn().await;
            capture.set_get_node_names(vec!["n1".into(), "n2".into()]);

            main_with_args(vec![
                "node".into(),
                "--controller".into(),
                format!("http://{addr}"),
                "label".into(),
                pattern.into(),
                "pool=gpu".into(),
            ])
            .await
            .unwrap();

            assert_eq!(capture.update_node_names(), vec!["n1", "n2"]);
        }
    }

    #[tokio::test]
    async fn label_all_rejects_an_empty_cluster() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let err = main_with_args(vec![
            "node".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "label".into(),
            "ALL".into(),
            "pool=gpu".into(),
        ])
        .await
        .unwrap_err();

        assert!(err.to_string().contains("no nodes registered"));
        assert!(capture.update_node_names().is_empty());
    }

    #[tokio::test]
    async fn label_hostlist_does_not_query_all_nodes() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        main_with_args(vec![
            "node".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "label".into(),
            "node[1-2]".into(),
            "pool=gpu".into(),
        ])
        .await
        .unwrap();

        assert_eq!(capture.update_node_names(), vec!["node1", "node2"]);
    }

    #[tokio::test]
    async fn invalid_hostlist_is_rejected_before_connecting() {
        let addr = crate::mock_controller::unreachable_addr().await;
        let err = main_with_args(vec![
            "node".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "label".into(),
            "node[".into(),
            "pool=gpu".into(),
        ])
        .await
        .unwrap_err();

        assert!(err.to_string().contains("invalid node name pattern"));
    }
}
