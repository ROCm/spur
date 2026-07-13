// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur k8s` subcommands: drive the SPUR-managed k0s cluster.

use anyhow::Result;
use clap::{Parser, Subcommand};

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    ClusterDownRequest, ClusterKubeconfigRequest, ClusterStatusRequest, ClusterUpRequest,
};

/// Manage the SPUR-provisioned k0s cluster.
#[derive(Parser, Debug)]
#[command(name = "k8s", about = "Manage the SPUR-provisioned k0s cluster")]
pub struct K8sArgs {
    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    controller: String,

    #[command(subcommand)]
    pub command: K8sCommand,
}

#[derive(Subcommand, Debug)]
pub enum K8sCommand {
    /// Bring the k0s cluster up (assign roles/IPs, then start each node's component).
    Up {
        /// Control-plane node (default: picked from inventory / [cluster] config).
        #[arg(long)]
        control_plane_node: Option<String>,
        /// Also install the ARC CI runner scale sets.
        #[arg(long)]
        install_arc: bool,
    },
    /// Tear the k0s cluster down.
    Down {
        /// Also `k0s reset` each node (destructive: wipes cluster state).
        #[arg(long)]
        reset: bool,
    },
    /// Show cluster phase + per-node component status.
    Status,
    /// Print the admin kubeconfig to stdout.
    Kubeconfig,
    /// Download + install the k0s binary on THIS node (local; no controller needed).
    /// Run as root for the default /usr/local/bin path.
    InstallK0s {
        /// k0s release tag to install, or "latest". Defaults to spur's pinned version.
        #[arg(long, default_value_t = String::from(spur_core::k0s::K0S_PINNED_VERSION))]
        version: String,
        /// Install path for the k0s binary.
        #[arg(long, default_value_t = String::from(spur_core::k0s::K0S_DEFAULT_BINARY))]
        path: String,
        /// Reinstall even if a k0s binary already exists at --path.
        #[arg(long)]
        force: bool,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = K8sArgs::try_parse_from(args)?;
    let controller = parsed.controller;
    match parsed.command {
        K8sCommand::Up {
            control_plane_node,
            install_arc,
        } => cmd_up(&controller, control_plane_node, install_arc).await,
        K8sCommand::Down { reset } => cmd_down(&controller, reset).await,
        K8sCommand::Status => cmd_status(&controller).await,
        K8sCommand::Kubeconfig => cmd_kubeconfig(&controller).await,
        K8sCommand::InstallK0s {
            version,
            path,
            force,
        } => cmd_install_k0s(&version, &path, force).await,
    }
}

async fn cmd_install_k0s(version: &str, path: &str, force: bool) -> Result<()> {
    let dest = std::path::Path::new(path);
    if dest.exists() && !force {
        eprintln!("k0s already present at {path} (use --force to reinstall)");
        return Ok(());
    }
    eprintln!("Installing k0s {version} -> {path} ...");
    let info = spur_update::k0s::install_k0s(version, dest).await?;
    let short = &info.sha256[..info.sha256.len().min(16)];
    eprintln!(
        "Installed k0s {} to {} (sha256 {}…)",
        info.version,
        info.path.display(),
        short
    );
    Ok(())
}

async fn cmd_up(
    controller: &str,
    control_plane_node: Option<String>,
    install_arc: bool,
) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_up(ClusterUpRequest {
            control_plane_node,
            install_arc,
        })
        .await?
        .into_inner();
    if resp.accepted {
        println!("k0s cluster up requested: {}", resp.message);
    } else {
        eprintln!("k0s cluster up NOT accepted: {}", resp.message);
    }
    for n in resp.nodes {
        println!("  {} [{}] {}", n.node, n.role, n.component_state);
    }
    Ok(())
}

async fn cmd_down(controller: &str, reset: bool) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_down(ClusterDownRequest { reset })
        .await?
        .into_inner();
    if resp.accepted {
        println!("k0s cluster down requested: {}", resp.message);
    } else {
        eprintln!("k0s cluster down NOT accepted: {}", resp.message);
    }
    Ok(())
}

async fn cmd_status(controller: &str) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_status(ClusterStatusRequest {})
        .await?
        .into_inner();
    println!("phase: {}", resp.phase);
    if !resp.control_plane_node.is_empty() {
        println!("control-plane: {}", resp.control_plane_node);
    }
    for n in resp.nodes {
        println!(
            "  {:<24} {:<11} {:<11} enabled={}",
            n.node, n.role, n.component_state, n.enabled
        );
    }
    Ok(())
}

async fn cmd_kubeconfig(controller: &str) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_kubeconfig(ClusterKubeconfigRequest {})
        .await?
        .into_inner();
    // stdout = data (the YAML), so it can be redirected to a kubeconfig file.
    print!("{}", resp.kubeconfig);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_with_control_plane() {
        let args = K8sArgs::try_parse_from([
            "k8s",
            "up",
            "--control-plane-node",
            "head-node",
            "--install-arc",
        ])
        .unwrap();
        match args.command {
            K8sCommand::Up {
                control_plane_node,
                install_arc,
            } => {
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert!(install_arc);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn parses_down_reset_and_status() {
        let args = K8sArgs::try_parse_from(["k8s", "down", "--reset"]).unwrap();
        assert!(matches!(args.command, K8sCommand::Down { reset: true }));
        let args = K8sArgs::try_parse_from(["k8s", "status"]).unwrap();
        assert!(matches!(args.command, K8sCommand::Status));
    }

    #[test]
    fn controller_defaults_and_env() {
        let args = K8sArgs::try_parse_from(["k8s", "status"]).unwrap();
        assert_eq!(args.controller, "http://localhost:6817");
    }
}
