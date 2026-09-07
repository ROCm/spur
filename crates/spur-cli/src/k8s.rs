// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur k8s` subcommands: drive the SPUR-managed k0s cluster.

use anyhow::Result;
use clap::{Parser, Subcommand};

use spur_core::k0s::SiloPhase;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    ClusterAddNodesRequest, ClusterDownRequest, ClusterKubeconfigRequest,
    ClusterRemoveNodesRequest, ClusterReportSiloRequest, ClusterStatusRequest,
    ClusterStatusResponse, ClusterUpRequest,
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
        /// HA control-plane count: 1, 3, or 5 (default: [cluster] config, else 1).
        #[arg(long)]
        replicas: Option<u32>,
        /// Explicit control-plane nodes (repeatable; 1, 3, or 5). First is the etcd bootstrap.
        /// Overrides --replicas.
        #[arg(long = "control-plane-nodes", value_delimiter = ',')]
        control_plane_nodes: Vec<String>,
        /// Scope the cluster to a subset of nodes (hostlist, e.g. "gpu[01-08]"), unioned with
        /// --partition/--selector; empty = whole inventory. Resolved once at up time (not re-evaluated).
        #[arg(long)]
        nodes: Option<String>,
        /// Scope the cluster to a partition's nodes.
        #[arg(long)]
        partition: Option<String>,
        /// Scope the cluster to nodes matching every key=value label (repeatable).
        #[arg(long = "selector", value_parser = parse_key_val)]
        selector: Vec<(String, String)>,
    },
    /// Add worker nodes to a running cluster (scoped clusters only; no down/reset needed).
    AddNodes {
        /// Nodes to add (hostlist, e.g. "gpu[09-12]"), unioned with --partition/--selector.
        #[arg(long)]
        nodes: Option<String>,
        /// Add a partition's nodes.
        #[arg(long)]
        partition: Option<String>,
        /// Add nodes matching every key=value label (repeatable).
        #[arg(long = "selector", value_parser = parse_key_val)]
        selector: Vec<(String, String)>,
    },
    /// Remove worker nodes from a running cluster: cordon + drain, then `k0s reset` the node
    /// (destructive — the node re-downloads/re-seeds on a later add). Use `spur node drain` instead
    /// for a temporary "stop scheduling here".
    RemoveNodes {
        /// Worker nodes to remove (hostlist, e.g. "gpu[09-12]").
        #[arg(long)]
        nodes: String,
        /// Max seconds to wait for the k8s drain per node (0 = server default).
        #[arg(long)]
        drain_timeout: Option<u32>,
        /// Proceed even if a node has running jobs (they are left running — this only skips the
        /// busy-node check) or its drain does not complete (bypasses PodDisruptionBudgets).
        #[arg(long)]
        force: bool,
    },
    /// Tear the k0s cluster down.
    Down {
        /// Also `k0s reset` each node (destructive: wipes cluster state).
        #[arg(long)]
        reset: bool,
    },
    /// Show cluster phase + per-node component status.
    Status,
    /// Print a kubeconfig to stdout. Default: your own scope. `--admin`: cluster-admin (admins only).
    /// `--user X`: another user's scope (admins only).
    Kubeconfig {
        /// Mint a scoped kubeconfig for this SPUR user; targeting anyone but yourself needs admin.
        #[arg(long)]
        user: Option<String>,
        /// Fetch the cluster-admin kubeconfig instead of a scoped one (requires cluster admin).
        #[arg(long, conflicts_with = "user")]
        admin: bool,
    },
    /// Prepare a node for the platform stack, and install it.
    Silo {
        #[command(subcommand)]
        command: SiloCommand,
    },
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
            replicas,
            control_plane_nodes,
            nodes,
            partition,
            selector,
        } => {
            cmd_up(
                &controller,
                control_plane_node,
                replicas,
                control_plane_nodes,
                nodes,
                partition,
                selector,
            )
            .await
        }
        K8sCommand::AddNodes {
            nodes,
            partition,
            selector,
        } => cmd_add_nodes(&controller, nodes, partition, selector).await,
        K8sCommand::RemoveNodes {
            nodes,
            drain_timeout,
            force,
        } => cmd_remove_nodes(&controller, nodes, drain_timeout, force).await,
        K8sCommand::Down { reset } => cmd_down(&controller, reset).await,
        K8sCommand::Status => cmd_status(&controller).await,
        K8sCommand::Kubeconfig { user, admin } => cmd_kubeconfig(&controller, user, admin).await,
        K8sCommand::Silo { command } => match command {
            SiloCommand::PrepareNode {
                data_disk,
                force_format,
                dry_run,
            } => crate::prepare_node::cmd_prepare_node(data_disk, force_format, dry_run).await,
            SiloCommand::Install {
                release,
                size,
                domain,
                cert_option,
                tls_cert,
                tls_key,
                force,
            } => {
                let opts = SiloOptions {
                    release,
                    size,
                    domain,
                    cert_option,
                    tls_cert,
                    tls_key,
                };
                cmd_install_silo(&controller, &opts, force).await
            }
        },
        K8sCommand::InstallK0s {
            version,
            path,
            force,
        } => cmd_install_k0s(&version, &path, force).await,
    }
}

#[derive(Subcommand, Debug)]
pub enum SiloCommand {
    /// Prepare THIS node to run k0s and the platform stack (local; no controller needed). Run as
    /// root, and run it before `spur k8s silo install`. Gives the k0s data directory a dedicated
    /// disk, which cannot be done once the node has run k0s.
    PrepareNode {
        /// Block device to mount at /var/lib/k0s, e.g. /dev/sdb. It is formatted ext4 when it
        /// carries no filesystem. Omit to leave the data directory on the root filesystem.
        #[arg(long)]
        data_disk: Option<String>,
        /// Erase a filesystem already on --data-disk. Without this a non-ext4 device is refused.
        #[arg(long)]
        force_format: bool,
        /// Report what would change without touching the node.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install the platform stack (cluster-forge) onto the k0s cluster from THIS node, then record
    /// the outcome on the controller. Brings the cluster up first when it is not running yet.
    /// Not usable from a workstation.
    Install {
        /// cluster-forge release to deploy: a tag, a branch, or a release archive URL.
        /// Empty = the pinned release.
        #[arg(long)]
        release: Option<String>,
        /// Cluster size profile. `small` builds no cluster-values repository, so it cannot
        /// disable an application.
        #[arg(long, value_parser = ["small", "medium", "large"], default_value = "medium")]
        size: String,
        /// Ingress domain for the platform stack.
        #[arg(long)]
        domain: String,
        /// Certificate source. `existing` needs --tls-cert and --tls-key.
        #[arg(long, value_parser = ["existing", "generate"], default_value = "existing")]
        cert_option: String,
        /// Path to the TLS certificate, when --cert-option is `existing`.
        #[arg(long)]
        tls_cert: Option<String>,
        /// Path to the TLS private key, when --cert-option is `existing`.
        #[arg(long)]
        tls_key: Option<String>,
        /// Reinstall even when the controller already reports the stack installed.
        #[arg(long)]
        force: bool,
    },
}

fn effective_user() -> String {
    whoami::username().unwrap_or_else(|_| "unknown".into())
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s}"))?;
    if k.is_empty() {
        return Err(format!("empty selector key in {s}"));
    }
    if v.is_empty() {
        return Err(format!("empty selector value in {s}"));
    }
    Ok((k.to_string(), v.to_string()))
}

/// Fold repeated `--selector key=val` into a map, rejecting a duplicate key rather than silently
/// dropping the earlier value (last-wins would change the intended AND scope).
fn selector_map(
    selector: Vec<(String, String)>,
) -> Result<std::collections::HashMap<String, String>, anyhow::Error> {
    let mut map = std::collections::HashMap::new();
    for (k, v) in selector {
        if map.insert(k.clone(), v).is_some() {
            anyhow::bail!("duplicate --selector key {k}");
        }
    }
    Ok(map)
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

/// Report the outcome to the controller. A failed report is not fatal to an install that already
/// succeeded, so this warns and returns rather than propagating: losing the status record is worse
/// reported than it is silently swallowed, but it must not turn a good install into a bad exit.
async fn report_silo(
    controller: &str,
    phase: SiloPhase,
    release: &str,
    size: &str,
    domain: &str,
    message: &str,
) {
    let req = ClusterReportSiloRequest {
        phase: phase.as_str().to_string(),
        release: release.to_string(),
        size: size.to_string(),
        domain: domain.to_string(),
        message: message.to_string(),
        caller: effective_user(),
    };
    let sent = async {
        let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
        client.cluster_report_silo(req).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = sent {
        eprintln!("warning: could not record silo status on the controller: {e}");
    }
}

pub(crate) struct SiloOptions {
    release: Option<String>,
    size: String,
    domain: String,
    cert_option: String,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

async fn cluster_status(controller: &str) -> Result<ClusterStatusResponse> {
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    Ok(client
        .cluster_status(ClusterStatusRequest {})
        .await?
        .into_inner())
}

/// How long to wait for the cluster to reach ready after asking for it.
const CLUSTER_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const CLUSTER_READY_POLL: std::time::Duration = std::time::Duration::from_secs(10);

/// Bring the cluster up and wait for it, the way `spur k8s up` does.
///
/// `cluster_up` is a request rather than an action: spurctld reconciles toward ready on its own
/// schedule, so asking is not enough and this has to wait for the result.
async fn ensure_cluster_ready(
    controller: &str,
    status: ClusterStatusResponse,
) -> Result<ClusterStatusResponse> {
    eprintln!(
        "The k0s cluster is {}, so SPUR is bringing it up ...",
        status.phase
    );
    {
        let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
        // `caller` is what spurctld authorises against, so a default request is refused as
        // non-admin however the command was invoked.
        let resp = client
            .cluster_up(ClusterUpRequest {
                caller: effective_user(),
                ..Default::default()
            })
            .await?
            .into_inner();
        if !resp.accepted {
            anyhow::bail!(
                "the k0s cluster did not accept the bring-up: {}",
                resp.message
            );
        }
    }
    let deadline = std::time::Instant::now() + CLUSTER_READY_TIMEOUT;
    loop {
        let status = cluster_status(controller).await?;
        if status.phase == "ready" {
            eprintln!("The k0s cluster is ready");
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the k0s cluster is still {} after {}s — check `spur k8s status`",
                status.phase,
                CLUSTER_READY_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(CLUSTER_READY_POLL).await;
    }
}

/// Where the gateway certificate comes from. `existing` names two files, so both have to be there
/// before anything is created; `generate` builds a pair from the domain.
fn cert_source(o: &SiloOptions) -> Result<crate::silo::certs::CertSource<'_>> {
    if o.cert_option != "existing" {
        return Ok(crate::silo::certs::CertSource::Generate { domain: &o.domain });
    }
    let (cert, key) = (
        o.tls_cert.as_deref().unwrap_or_default(),
        o.tls_key.as_deref().unwrap_or_default(),
    );
    if cert.is_empty() || key.is_empty() {
        anyhow::bail!(
            "--cert-option existing needs --tls-cert and --tls-key; pass --cert-option generate \
             to let the deployer make a self-signed pair"
        );
    }
    Ok(crate::silo::certs::CertSource::Existing { cert, key })
}

async fn cmd_install_silo(controller: &str, o: &SiloOptions, force: bool) -> Result<()> {
    // Resolve before anything is reported: an omitted --release takes the pinned version, and the
    // cluster has to record the version it actually runs rather than an empty string.
    let resolved = crate::silo::release::resolve(o.release.as_deref().unwrap_or_default())?;
    let (release, size, domain) = (
        resolved.revision.as_str(),
        o.size.as_str(),
        o.domain.as_str(),
    );
    crate::silo::preflight::report_unmet_prerequisites().await;
    crate::silo::preflight::require_install_binaries(&resolved)?;

    let mut status = cluster_status(controller).await?;
    if let Some(silo) = &status.silo {
        if silo.phase == SiloPhase::Installed.as_str() && !force {
            eprintln!(
                "platform stack already installed (release {}, size {}) — use --force to reinstall",
                silo.release, silo.size
            );
            return Ok(());
        }
    }
    if status.phase != "ready" {
        status = ensure_cluster_ready(controller, status).await?;
    }

    let cert_source = cert_source(o)?;

    let dir = crate::silo::kubeconfig::private_dir()?;
    crate::silo::kubeconfig::stage(controller, effective_user()).await?;
    crate::silo::storage::ensure_storage_classes().await?;
    crate::silo::certs::ensure_cluster_tls(cert_source, &dir).await?;
    // Before the deployer, so the Envoy proxy pods schedule as soon as the platform stack creates
    // them rather than sitting Pending until an operator notices.
    let mut control_plane = status.control_plane_nodes.clone();
    if control_plane.is_empty() && !status.control_plane_node.is_empty() {
        control_plane.push(status.control_plane_node.clone());
    }
    let gateway_node = crate::silo::gateway_node::ensure_first_node_label(&control_plane).await?;
    crate::silo::ensure_helm_chart_config_crd().await?;

    report_silo(controller, SiloPhase::Installing, release, size, domain, "").await;
    let options = crate::silo::Options {
        release: &resolved,
        size,
        domain,
    };
    if let Err(e) = crate::silo::install(&options, &dir).await {
        let msg = format!("{e:#}");
        report_silo(controller, SiloPhase::Failed, release, size, domain, &msg).await;
        return Err(e);
    }

    // After the platform stack, not before: the MetalLB CRD arrives with it.
    if let Err(e) = crate::silo::metallb::ensure_load_balancer_pool(&gateway_node).await {
        eprintln!("warning: {e}");
    }
    if let Err(e) = crate::silo::cnpg::repair_unrecoverable_databases().await {
        eprintln!("warning: {e}");
    }
    report_silo(controller, SiloPhase::Installed, release, size, domain, "").await;
    println!("platform stack installed (size {size}, domain {domain})");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_up(
    controller: &str,
    control_plane_node: Option<String>,
    replicas: Option<u32>,
    control_plane_nodes: Vec<String>,
    nodes: Option<String>,
    partition: Option<String>,
    selector: Vec<(String, String)>,
) -> Result<()> {
    let selector = selector_map(selector)?;
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    let resp = client
        .cluster_up(ClusterUpRequest {
            control_plane_node,
            control_plane_replicas: replicas,
            control_plane_nodes,
            caller: effective_user(),
            nodes: nodes.unwrap_or_default(),
            partition: partition.unwrap_or_default(),
            selector,
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

async fn cmd_add_nodes(
    controller: &str,
    nodes: Option<String>,
    partition: Option<String>,
    selector: Vec<(String, String)>,
) -> Result<()> {
    let selector = selector_map(selector)?;
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_add_nodes(ClusterAddNodesRequest {
            nodes: nodes.unwrap_or_default(),
            partition: partition.unwrap_or_default(),
            selector,
            caller: effective_user(),
        })
        .await?
        .into_inner();
    if resp.accepted {
        println!("k0s add-nodes requested: {}", resp.message);
    } else {
        eprintln!("k0s add-nodes NOT accepted: {}", resp.message);
    }
    for n in resp.nodes {
        println!("  {} [{}] {}", n.node, n.role, n.component_state);
    }
    Ok(())
}

async fn cmd_remove_nodes(
    controller: &str,
    nodes: String,
    drain_timeout: Option<u32>,
    force: bool,
) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client
        .cluster_remove_nodes(ClusterRemoveNodesRequest {
            nodes,
            caller: effective_user(),
            drain_timeout_secs: drain_timeout,
            force: Some(force),
        })
        .await?
        .into_inner();
    if resp.accepted {
        println!("k0s remove-nodes requested: {}", resp.message);
    } else {
        eprintln!("k0s remove-nodes NOT accepted: {}", resp.message);
    }
    for n in resp.nodes {
        println!("  {} [{}] {}", n.node, n.role, n.component_state);
    }
    Ok(())
}

async fn cmd_down(controller: &str, reset: bool) -> Result<()> {
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    let resp = client
        .cluster_down(ClusterDownRequest {
            reset,
            caller: effective_user(),
        })
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
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    let resp = client
        .cluster_status(ClusterStatusRequest {})
        .await?
        .into_inner();
    println!("phase: {}", resp.phase);
    if !resp.control_plane_nodes.is_empty() {
        println!("control-plane: {}", resp.control_plane_nodes.join(", "));
    } else if !resp.control_plane_node.is_empty() {
        println!("control-plane: {}", resp.control_plane_node);
    }
    // Teardown clears the recorded scope immediately, before node roles finish draining; showing
    // "all nodes" here would misrepresent a cluster that's mid-teardown, not freshly scoped.
    if resp.phase != "down" {
        if resp.member_nodes.is_empty() {
            println!("members: all nodes");
        } else {
            println!("members: {}", resp.member_nodes.join(", "));
        }
    }
    if let Some(silo) = &resp.silo {
        print!("silo: {}", silo.phase);
        if !silo.release.is_empty() {
            print!(" release={}", silo.release);
        }
        if !silo.size.is_empty() {
            print!(" size={}", silo.size);
        }
        if !silo.domain.is_empty() {
            print!(" domain={}", silo.domain);
        }
        println!();
        if !silo.message.is_empty() {
            println!("  {}", silo.message);
        }
    }
    for n in resp.nodes {
        print!(
            "  {:<24} {:<11} {:<11} enabled={}",
            n.node, n.role, n.component_state, n.enabled
        );
        if !n.reason.is_empty() {
            print!("  reason: {}", n.reason);
        }
        println!();
    }
    Ok(())
}

async fn cmd_kubeconfig(controller: &str, user: Option<String>, admin: bool) -> Result<()> {
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    let resp = client
        .cluster_kubeconfig(ClusterKubeconfigRequest {
            user: user.unwrap_or_default(),
            caller: effective_user(),
            admin,
        })
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
        let args =
            K8sArgs::try_parse_from(["k8s", "up", "--control-plane-node", "head-node"]).unwrap();
        match args.command {
            K8sCommand::Up {
                control_plane_node,
                replicas,
                control_plane_nodes,
                ..
            } => {
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert_eq!(replicas, None);
                assert!(control_plane_nodes.is_empty());
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn parses_up_with_node_scope_flags() {
        let args = K8sArgs::try_parse_from([
            "k8s",
            "up",
            "--nodes",
            "gpu[01-08]",
            "--partition",
            "batch",
            "--selector",
            "zone=z1",
            "--selector",
            "gpu=mi300",
        ])
        .unwrap();
        match args.command {
            K8sCommand::Up {
                nodes,
                partition,
                selector,
                ..
            } => {
                assert_eq!(nodes.as_deref(), Some("gpu[01-08]"));
                assert_eq!(partition.as_deref(), Some("batch"));
                assert_eq!(
                    selector,
                    vec![
                        ("zone".to_string(), "z1".to_string()),
                        ("gpu".to_string(), "mi300".to_string())
                    ]
                );
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn selector_without_equals_is_rejected() {
        assert!(K8sArgs::try_parse_from(["k8s", "up", "--selector", "bogus"]).is_err());
    }

    #[test]
    fn selector_with_empty_value_is_rejected() {
        assert!(K8sArgs::try_parse_from(["k8s", "up", "--selector", "gpu="]).is_err());
    }

    #[test]
    fn selector_map_rejects_duplicate_key() {
        let dup = vec![
            ("zone".to_string(), "z1".to_string()),
            ("zone".to_string(), "z2".to_string()),
        ];
        let err = selector_map(dup).unwrap_err().to_string();
        assert!(err.contains("duplicate --selector key zone"), "got: {err}");
        let ok = selector_map(vec![
            ("zone".to_string(), "z1".to_string()),
            ("gpu".to_string(), "mi300".to_string()),
        ])
        .unwrap();
        assert_eq!(ok.len(), 2);
    }

    #[test]
    fn parses_up_with_replicas_and_node_set() {
        let args = K8sArgs::try_parse_from(["k8s", "up", "--replicas", "3"]).unwrap();
        match args.command {
            K8sCommand::Up { replicas, .. } => assert_eq!(replicas, Some(3)),
            _ => panic!("wrong command"),
        }
        let args =
            K8sArgs::try_parse_from(["k8s", "up", "--control-plane-nodes", "cp-1,cp-2,cp-3"])
                .unwrap();
        match args.command {
            K8sCommand::Up {
                control_plane_nodes,
                ..
            } => assert_eq!(control_plane_nodes, vec!["cp-1", "cp-2", "cp-3"]),
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
    fn parses_add_nodes_scope_flags() {
        let args = K8sArgs::try_parse_from(["k8s", "add-nodes", "--nodes", "gpu[09-12]"]).unwrap();
        match args.command {
            K8sCommand::AddNodes { nodes, .. } => assert_eq!(nodes.as_deref(), Some("gpu[09-12]")),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn parses_remove_nodes_with_flags() {
        let args = K8sArgs::try_parse_from([
            "k8s",
            "remove-nodes",
            "--nodes",
            "gpu[09-12]",
            "--drain-timeout",
            "300",
            "--force",
        ])
        .unwrap();
        match args.command {
            K8sCommand::RemoveNodes {
                nodes,
                drain_timeout,
                force,
            } => {
                assert_eq!(nodes, "gpu[09-12]");
                assert_eq!(drain_timeout, Some(300));
                assert!(force);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn remove_nodes_requires_nodes() {
        // --nodes is mandatory for remove-nodes.
        assert!(K8sArgs::try_parse_from(["k8s", "remove-nodes"]).is_err());
    }

    #[test]
    fn controller_defaults_and_env() {
        let args = K8sArgs::try_parse_from(["k8s", "status"]).unwrap();
        assert_eq!(args.controller, "http://localhost:6817");
    }

    #[test]
    fn parses_silo_install_with_flags() {
        let args = K8sArgs::try_parse_from([
            "k8s",
            "silo",
            "install",
            "--release",
            "v1.2.3",
            "--size",
            "large",
            "--domain",
            "cf.example.com",
            "--force",
        ])
        .unwrap();
        match args.command {
            K8sCommand::Silo {
                command:
                    SiloCommand::Install {
                        release,
                        size,
                        domain,
                        force,
                        ..
                    },
            } => {
                assert_eq!(release.as_deref(), Some("v1.2.3"));
                assert_eq!(size, "large");
                assert_eq!(domain, "cf.example.com");
                assert!(force);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn silo_install_defaults_to_medium() {
        // `small` builds no cluster-values repository and so cannot disable an application, which
        // is why it is not the default.
        let args =
            K8sArgs::try_parse_from(["k8s", "silo", "install", "--domain", "cf.example.com"])
                .unwrap();
        match args.command {
            K8sCommand::Silo {
                command:
                    SiloCommand::Install {
                        size,
                        release,
                        force,
                        ..
                    },
            } => {
                assert_eq!(size, "medium");
                assert_eq!(release, None);
                assert!(!force);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn silo_install_requires_domain() {
        assert!(K8sArgs::try_parse_from(["k8s", "silo", "install"]).is_err());
    }

    #[test]
    fn parses_silo_prepare_node() {
        let args = K8sArgs::try_parse_from([
            "k8s",
            "silo",
            "prepare-node",
            "--data-disk",
            "/dev/sdb",
            "--dry-run",
        ])
        .unwrap();
        match args.command {
            K8sCommand::Silo {
                command:
                    SiloCommand::PrepareNode {
                        data_disk,
                        force_format,
                        dry_run,
                    },
            } => {
                assert_eq!(data_disk.as_deref(), Some("/dev/sdb"));
                assert!(!force_format);
                assert!(dry_run);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn the_old_flat_names_are_gone() {
        // Both moved under `silo`, so the flat forms must not silently keep working and leave two
        // spellings of the same command in use.
        assert!(K8sArgs::try_parse_from(["k8s", "install-silo", "--domain", "d"]).is_err());
        assert!(K8sArgs::try_parse_from(["k8s", "prepare-node"]).is_err());
    }

    #[test]
    fn install_silo_rejects_an_unknown_cert_option() {
        assert!(K8sArgs::try_parse_from([
            "k8s",
            "install-silo",
            "--domain",
            "cf.example.com",
            "--cert-option",
            "letsencrypt",
        ])
        .is_err());
    }

    #[test]
    fn install_silo_rejects_an_unknown_size() {
        assert!(K8sArgs::try_parse_from([
            "k8s",
            "install-silo",
            "--domain",
            "cf.example.com",
            "--size",
            "enormous",
        ])
        .is_err());
    }

    #[test]
    fn kubeconfig_bare_is_self_scoped() {
        let args = K8sArgs::try_parse_from(["k8s", "kubeconfig"]).unwrap();
        match args.command {
            K8sCommand::Kubeconfig { user, admin } => {
                assert_eq!(user, None);
                assert!(!admin);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn kubeconfig_admin_flag_parses() {
        let args = K8sArgs::try_parse_from(["k8s", "kubeconfig", "--admin"]).unwrap();
        match args.command {
            K8sCommand::Kubeconfig { admin, .. } => assert!(admin),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn kubeconfig_admin_and_user_conflict() {
        let err =
            K8sArgs::try_parse_from(["k8s", "kubeconfig", "--admin", "--user", "bob"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "--admin and --user must be mutually exclusive"
        );
    }
}
