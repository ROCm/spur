// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur node` subcommands for node lifecycle management.

use crate::scontrol::{is_all_node_pattern, resolve_node_names};
use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use spur_proto::proto::{
    AbortRecoveryRequest, RecoverNodeRequest, SelfLabelNodeRequest, SelfUndrainNodeRequest,
    UpdateNodeRequest,
};

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
        /// Use the node's own WireGuard identity (for recovery scripts).
        /// Restricts labels to the spur.recovery/ namespace.
        #[arg(long = "self")]
        self_: bool,
    },
    /// Drain one or more nodes: stop scheduling new jobs while existing jobs finish.
    Drain {
        /// Node names: ALL, a comma-separated list, and/or a hostlist range (e.g. "n1,n2", "n[1-4]")
        node: String,
        /// Reason for draining
        #[arg(long)]
        reason: Option<String>,
    },
    /// Return a node to idle (clear a drain hold).
    Undrain {
        /// Node name
        node: String,
        /// Use the node's own WireGuard identity (for recovery scripts).
        /// Only works on system-initiated drains.
        #[arg(long = "self")]
        self_: bool,
    },
    /// Manage auto-recovery on nodes.
    Recovery {
        #[command(subcommand)]
        command: RecoveryCommand,
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

#[derive(Subcommand, Debug)]
pub enum RecoveryCommand {
    /// Hand a drained node to the recovery script (admin-only).
    Trigger {
        /// Node name
        node: String,
    },
    /// Abort auto-recovery and reclassify as admin drain.
    Abort {
        /// Node name
        node: String,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = crate::clap_exit::parse_or_exit::<NodeArgs>(&args);
    let controller = parsed.controller;
    match parsed.command {
        NodeCommand::Label {
            node,
            labels,
            self_,
        } => cmd_label(&controller, node, labels, self_).await,
        NodeCommand::Drain { node, reason } => cmd_drain(&controller, node, reason).await,
        NodeCommand::Undrain { node, self_ } => cmd_undrain(&controller, &node, self_).await,
        NodeCommand::Recovery { command } => cmd_recovery(&controller, command).await,
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

async fn cmd_label(
    controller: &str,
    node_pattern: String,
    label_args: Vec<String>,
    self_: bool,
) -> Result<()> {
    let (set_labels, remove_labels) = parse_label_args(&label_args)?;

    if self_ {
        let mut client =
            spur_proto::controller_client(crate::authclient::connect(controller).await?);
        // --self: use SelfLabelNode RPC (node's own identity, spur.recovery/ namespace only)
        client
            .self_label_node(SelfLabelNodeRequest {
                name: node_pattern.clone(),
                set_labels: set_labels.clone(),
                remove_labels: remove_labels.clone(),
            })
            .await
            .context("SelfLabelNode RPC failed")?;
        for (k, v) in &set_labels {
            println!("  {node_pattern}: {k}={v}");
        }
        for k in &remove_labels {
            println!("  {node_pattern}: {k} removed");
        }
        return Ok(());
    }

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

async fn cmd_undrain(controller: &str, node: &str, self_: bool) -> Result<()> {
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);

    if self_ {
        client
            .self_undrain_node(SelfUndrainNodeRequest {
                name: node.to_string(),
            })
            .await
            .context("SelfUndrainNode RPC failed")?;
    } else {
        client
            .update_node(UpdateNodeRequest {
                name: node.to_string(),
                state: Some(spur_proto::proto::NodeState::NodeIdle.into()),
                reason: None,
                labels: HashMap::new(),
                remove_labels: vec![],
            })
            .await
            .context("UpdateNode RPC failed")?;
    }

    println!("Node {node} undrained");
    Ok(())
}

async fn cmd_recovery(controller: &str, command: RecoveryCommand) -> Result<()> {
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);

    match command {
        RecoveryCommand::Trigger { node } => {
            client
                .recover_node(RecoverNodeRequest { name: node.clone() })
                .await
                .context("RecoverNode RPC failed")?;
            println!("Recovery triggered for node {node}");
        }
        RecoveryCommand::Abort { node } => {
            client
                .abort_recovery(AbortRecoveryRequest { name: node.clone() })
                .await
                .context("AbortRecovery RPC failed")?;
            println!("Recovery aborted for node {node}");
        }
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
    async fn drain_all_resolves_registered_nodes() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_get_node_names(vec!["n1".into(), "n2".into()]);

        main_with_args(vec![
            "node".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "drain".into(),
            "ALL".into(),
        ])
        .await
        .unwrap();

        assert_eq!(capture.drain_node_names(), vec!["n1", "n2"]);
    }

    #[tokio::test]
    async fn remove_all_forces_every_registered_node() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_get_node_names(vec!["n1".into(), "n2".into()]);

        main_with_args(vec![
            "node".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "remove".into(),
            "ALL".into(),
            "--force".into(),
        ])
        .await
        .unwrap();

        assert_eq!(
            capture.deregister_node_calls(),
            vec![("n1".to_string(), true), ("n2".to_string(), true)]
        );
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
