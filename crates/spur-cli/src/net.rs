// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur net` subcommands for WireGuard mesh management.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use spur_net::address::AddressPool;
use spur_net::mesh::{self, MeshMembership};
use spur_net::wireguard::{self, WgConfig, WgPeer};

/// Network mesh management.
#[derive(Parser, Debug)]
#[command(name = "net", about = "Manage WireGuard mesh network")]
pub struct NetArgs {
    #[command(subcommand)]
    pub command: NetCommand,
}

#[derive(Subcommand, Debug)]
pub enum NetCommand {
    /// Initialize the WireGuard mesh on the controller node.
    ///
    /// Generates a keypair, assigns the .1 address, writes the config,
    /// and brings up the interface.
    Init {
        /// CIDR block for the mesh (default: 10.44.0.0/16)
        #[arg(long, default_value = "10.44.0.0/16")]
        cidr: String,
        /// WireGuard interface name
        #[arg(long, default_value = "spur0")]
        interface: String,
        /// WireGuard listen port
        #[arg(long, default_value_t = 51820)]
        port: u16,
        /// Config directory
        #[arg(long, default_value = "/etc/wireguard")]
        config_dir: PathBuf,
    },
    /// Join the WireGuard mesh from a compute node.
    ///
    /// Generates a local keypair, connects to the controller to exchange
    /// keys, and brings up the interface.
    Join {
        /// Controller's real IP and WireGuard port (e.g. 203.0.113.1:51820)
        #[arg(long)]
        endpoint: String,
        /// Controller's WireGuard public key
        #[arg(long)]
        server_key: String,
        /// IP address to assign to this node (e.g. 10.44.0.2)
        #[arg(long)]
        address: String,
        /// CIDR prefix length
        #[arg(long, default_value_t = 16)]
        prefix_len: u8,
        /// WireGuard interface name
        #[arg(long, default_value = "spur0")]
        interface: String,
        /// Config directory
        #[arg(long, default_value = "/etc/wireguard")]
        config_dir: PathBuf,
    },
    /// Show WireGuard mesh status.
    Status {
        /// WireGuard interface name
        #[arg(long, default_value = "spur0")]
        interface: String,
    },
    /// Add a peer to the running WireGuard interface.
    AddPeer {
        /// Peer's WireGuard public key
        #[arg(long)]
        key: String,
        /// Peer's allowed IP (e.g. 10.44.0.3/32)
        #[arg(long)]
        allowed_ip: String,
        /// Peer's pod CIDR (e.g. 10.42.3.0/24), folded into AllowedIPs so
        /// WireGuard forwards the peer's pod traffic. Required for a
        /// native-routing CNI (Calico `bird`) over the mesh.
        #[arg(long)]
        pod_cidr: Option<String>,
        /// Also install a kernel route (`<pod-cidr> dev <iface>`). OFF by
        /// default: a native-routing CNI owns the FIB routes and spur must not
        /// fight it. Use only when NO CNI manages routes (e.g. bare-mesh test).
        #[arg(long)]
        program_routes: bool,
        /// Peer's endpoint (e.g. 203.0.113.2:51820)
        #[arg(long)]
        endpoint: Option<String>,
        /// WireGuard interface name
        #[arg(long, default_value = "spur0")]
        interface: String,
    },
    /// Apply a full-mesh peering from a membership file on the local node.
    ///
    /// Adds every other node as a direct peer with pod-CIDR-aware AllowedIPs,
    /// turning the default hub-and-spoke into a full mesh a native-routing CNI
    /// can ride. Run on each node (e.g. SSH fan-out from the head) with the
    /// SAME membership file.
    ///
    /// This REPLACES the hub-and-spoke fallback: the membership file MUST list
    /// every live mesh node (nodes it omits lose the controller's catch-all
    /// route and become unreachable). It is additive — it does not remove peers
    /// or routes for nodes dropped from the file.
    Mesh {
        /// Path to the membership file (JSON: {"nodes":[{public_key, mesh_ip, endpoint, pod_cidr?}, ...]}).
        #[arg(long)]
        config: PathBuf,
        /// This node's mesh IP (the entry to skip), e.g. 10.44.0.2.
        #[arg(long = "self")]
        self_ip: String,
        /// Also install kernel pod-CIDR routes. OFF by default (the CNI owns
        /// FIB routes; see `add-peer --program-routes`).
        #[arg(long)]
        program_routes: bool,
        /// WireGuard interface name
        #[arg(long, default_value = "spur0")]
        interface: String,
        /// Print the computed peers/routes without applying them.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = NetArgs::try_parse_from(&args)?;

    match args.command {
        NetCommand::Init {
            cidr,
            interface,
            port,
            config_dir,
        } => cmd_init(&cidr, &interface, port, &config_dir),
        NetCommand::Join {
            endpoint,
            server_key,
            address,
            prefix_len,
            interface,
            config_dir,
        } => cmd_join(
            &endpoint,
            &server_key,
            &address,
            prefix_len,
            &interface,
            &config_dir,
        ),
        NetCommand::Status { interface } => cmd_status(&interface),
        NetCommand::AddPeer {
            key,
            allowed_ip,
            pod_cidr,
            program_routes,
            endpoint,
            interface,
        } => cmd_add_peer(
            &key,
            &allowed_ip,
            pod_cidr.as_deref(),
            program_routes,
            endpoint.as_deref(),
            &interface,
        ),
        NetCommand::Mesh {
            config,
            self_ip,
            program_routes,
            interface,
            dry_run,
        } => cmd_mesh(&config, &self_ip, &interface, program_routes, dry_run),
    }
}

fn cmd_init(cidr: &str, interface: &str, port: u16, config_dir: &Path) -> Result<()> {
    eprintln!("Initializing WireGuard mesh...");

    // Generate keypair
    let keypair = wireguard::generate_keypair().context("failed to generate WireGuard keypair")?;

    // Allocate .1 for the controller
    let mut pool = AddressPool::new(cidr)?;
    let controller_ip = pool.allocate()?;

    let config = WgConfig {
        private_key: keypair.private_key,
        address: format!("{}/{}", controller_ip, pool.prefix_len()),
        listen_port: Some(port),
        peers: vec![],
    };

    // Write config
    std::fs::create_dir_all(config_dir)?;
    let config_path = config_dir.join(format!("{}.conf", interface));
    config.write_to(&config_path)?;
    eprintln!("Config written to {}", config_path.display());

    // Bring up interface
    wireguard::interface_up(interface)?;

    eprintln!();
    eprintln!("WireGuard mesh initialized:");
    eprintln!("  Interface:  {}", interface);
    eprintln!("  Address:    {}/{}", controller_ip, pool.prefix_len());
    eprintln!("  Port:       {}", port);
    eprintln!("  Public key: {}", keypair.public_key);
    eprintln!();
    eprintln!("To add compute nodes, run on each node:");
    eprintln!("  spur net join \\");
    eprintln!("    --endpoint <this-host-ip>:{} \\", port);
    eprintln!("    --server-key {} \\", keypair.public_key);
    eprintln!("    --address <assigned-ip>");
    eprintln!();
    eprintln!("Then add the node as a peer on this controller:");
    eprintln!("  spur net add-peer --key <node-pubkey> --allowed-ip <node-ip>/32 --endpoint <node-real-ip>:{}", port);

    Ok(())
}

fn cmd_join(
    endpoint: &str,
    server_key: &str,
    address: &str,
    prefix_len: u8,
    interface: &str,
    config_dir: &Path,
) -> Result<()> {
    eprintln!("Joining WireGuard mesh...");

    // Generate local keypair
    let keypair = wireguard::generate_keypair().context("failed to generate WireGuard keypair")?;

    let config = WgConfig {
        private_key: keypair.private_key,
        address: format!("{}/{}", address, prefix_len),
        listen_port: Some(51820),
        peers: vec![WgPeer {
            public_key: server_key.to_string(),
            // Route all mesh traffic through the controller initially
            // (in a full mesh, each peer would have its own entry)
            allowed_ips: format!("{}/{}", address_network(address, prefix_len)?, prefix_len),
            endpoint: Some(endpoint.to_string()),
            persistent_keepalive: Some(25),
        }],
    };

    // Write config
    std::fs::create_dir_all(config_dir)?;
    let config_path = config_dir.join(format!("{}.conf", interface));
    config.write_to(&config_path)?;

    // Bring up interface
    wireguard::interface_up(interface)?;

    eprintln!();
    eprintln!("Joined WireGuard mesh:");
    eprintln!("  Interface:  {}", interface);
    eprintln!("  Address:    {}/{}", address, prefix_len);
    eprintln!("  Server:     {}", endpoint);
    eprintln!("  Public key: {}", keypair.public_key);
    eprintln!();
    eprintln!("Add this node as a peer on the controller:");
    eprintln!(
        "  spur net add-peer --key {} --allowed-ip {}/32",
        keypair.public_key, address
    );

    Ok(())
}

fn cmd_status(interface: &str) -> Result<()> {
    let output = std::process::Command::new("wg")
        .args(["show", interface])
        .output()
        .context("failed to run `wg show`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such device") {
            eprintln!(
                "Interface {} is not up. Run `spur net init` or `spur net join` first.",
                interface
            );
            return Ok(());
        }
        bail!("wg show failed: {}", stderr);
    }

    println!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn cmd_add_peer(
    key: &str,
    allowed_ip: &str,
    pod_cidr: Option<&str>,
    program_routes: bool,
    endpoint: Option<&str>,
    interface: &str,
) -> Result<()> {
    // Validate CIDRs before shelling out to `wg`/`ip` so bad input fails fast.
    mesh::validate_cidr(allowed_ip).context("--allowed-ip")?;
    let pod_cidr = pod_cidr.map(str::trim).filter(|s| !s.is_empty());
    if let Some(pod) = pod_cidr {
        mesh::validate_cidr(pod).context("--pod-cidr")?;
    }

    // When a pod CIDR is given, fold it into AllowedIPs so WireGuard forwards
    // the peer's pod traffic (native-routing CNI over the mesh).
    let allowed_ips = match pod_cidr {
        Some(pod) => format!("{},{}", allowed_ip, pod),
        None => allowed_ip.to_string(),
    };

    let peer = WgPeer {
        public_key: key.to_string(),
        allowed_ips: allowed_ips.clone(),
        endpoint: endpoint.map(|s| s.to_string()),
        persistent_keepalive: Some(25),
    };

    wireguard::add_peer(interface, &peer)?;

    // AllowedIPs (above) is all WireGuard needs. The kernel FIB route is the
    // CNI's job in native-routing mode; only program it when asked (no CNI).
    if program_routes {
        if let Some(pod) = pod_cidr {
            wireguard::add_route(interface, pod)?;
        }
    }

    eprintln!("Peer added to {}:", interface);
    eprintln!("  Public key:  {}", key);
    eprintln!("  Allowed IPs: {}", allowed_ips);
    if let Some(ep) = endpoint {
        eprintln!("  Endpoint:    {}", ep);
    }
    if let Some(pod) = pod_cidr {
        if program_routes {
            eprintln!("  Pod route:   {} dev {}", pod, interface);
        } else {
            eprintln!(
                "  Pod CIDR:    {} (AllowedIPs only; CNI owns the route)",
                pod
            );
        }
    }

    Ok(())
}

fn cmd_mesh(
    config: &Path,
    self_ip: &str,
    interface: &str,
    program_routes: bool,
    dry_run: bool,
) -> Result<()> {
    let self_ip = self_ip.trim();
    let raw = std::fs::read_to_string(config)
        .with_context(|| format!("failed to read mesh membership file {}", config.display()))?;
    let membership: MeshMembership = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse mesh membership file {}", config.display()))?;

    // Validate the membership up front: self must be listed, every mesh_ip is an
    // IP, and every pod_cidr is a CIDR (fail fast before touching `wg`/`ip`).
    if !membership.nodes.iter().any(|n| n.mesh_ip.trim() == self_ip) {
        bail!(
            "--self {} is not present in {} (nodes: {})",
            self_ip,
            config.display(),
            membership
                .nodes
                .iter()
                .map(|n| n.mesh_ip.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for n in &membership.nodes {
        n.mesh_ip
            .trim()
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid mesh_ip in membership: {:?}", n.mesh_ip))?;
        if let Some(pod) = n
            .pod_cidr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            mesh::validate_cidr(pod)
                .with_context(|| format!("invalid pod_cidr for node {}", n.mesh_ip))?;
        }
    }

    let peers = mesh::mesh_peers_for(self_ip, &membership.nodes);
    let routes = mesh::mesh_pod_routes(self_ip, &membership.nodes);

    if dry_run {
        eprintln!("Mesh apply (dry run) for {} on {}:", self_ip, interface);
        for p in &peers {
            eprintln!(
                "  peer {}  allowed-ips {}  endpoint {}",
                p.public_key,
                p.allowed_ips,
                p.endpoint.as_deref().unwrap_or("-")
            );
        }
        if program_routes {
            for r in &routes {
                eprintln!("  route {} dev {}", r, interface);
            }
        } else if !routes.is_empty() {
            eprintln!(
                "  (routes not programmed — CNI owns them; pass --program-routes to install)"
            );
        }
        return Ok(());
    }

    // This replaces the hub-and-spoke fallback (each peer's AllowedIPs is set,
    // not appended), so an omitted node loses reachability. Warn loudly.
    eprintln!(
        "note: applying full mesh — this replaces the hub-and-spoke fallback; \
         the membership file must list every live node."
    );

    let applied = mesh::apply_mesh(interface, self_ip, &membership.nodes, program_routes)?;
    eprintln!(
        "Mesh applied on {}: {} peer(s){} for {}.",
        interface,
        applied,
        if program_routes {
            format!(", {} pod route(s)", routes.len())
        } else {
            " (AllowedIPs only; CNI owns routes)".to_string()
        },
        self_ip
    );
    Ok(())
}

/// Given an IP and prefix length, compute the network address.
fn address_network(ip: &str, prefix_len: u8) -> Result<String> {
    let addr: Ipv4Addr = ip.parse().context("invalid IP address")?;
    let mask = !((1u32 << (32 - prefix_len)) - 1);
    let network = u32::from(addr) & mask;
    Ok(Ipv4Addr::from(network).to_string())
}
