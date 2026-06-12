// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod agent_server;
pub mod container;
mod executor;
mod landlock;
pub mod pmi;
mod reporter;
mod seccomp;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;
use tracing::{info, warn};

use spur_core::config::SlurmConfig;
use spur_devices::cdi::cache::CdiCache;
use spur_devices::DeviceRegistry;

use reporter::NodeReporter;

/// Parse a "key=value" string into a validated label.
fn parse_label(s: &str) -> Result<String, String> {
    if s.contains('=') && s.split('=').next().map_or(false, |k| !k.is_empty()) {
        Ok(s.to_string())
    } else {
        Err(format!("invalid label format '{s}', expected key=value"))
    }
}

#[derive(Parser)]
#[command(name = "spurd", about = "Spur node agent daemon")]
struct Args {
    /// Configuration file path
    #[arg(short = 'f', long, default_value = "/etc/spur/spur.conf")]
    config: std::path::PathBuf,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    controller: String,

    /// Agent gRPC listen address
    #[arg(long, default_value = "[::]:6818")]
    listen: String,

    /// Node name (defaults to hostname)
    #[arg(short = 'N', long)]
    hostname: Option<String>,

    /// Advertised IP address for the controller to reach this agent.
    /// If not set, auto-detected from WireGuard interface or hostname resolution.
    #[arg(long, env = "SPUR_NODE_ADDRESS")]
    address: Option<String>,

    /// Node labels for partition routing (key=value pairs).
    /// Can be specified multiple times: --label pool=gpu --label rack=a
    #[arg(long = "label", value_parser = parse_label, env = "SPUR_NODE_LABELS", value_delimiter = ',')]
    labels: Vec<String>,

    /// Foreground mode
    #[arg(short = 'D', long)]
    foreground: bool,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log_level.parse().unwrap()),
        )
        .init();

    let hostname = args.hostname.unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".into())
    });

    // Parse listen port from the listen address for registration
    let listen_port: u16 = args
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6818);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        hostname = %hostname,
        controller = %args.controller,
        listen = %args.listen,
        "spurd starting"
    );

    // Load config from spur.conf (best-effort: missing file is fine)
    let config = match SlurmConfig::load_from_file(&args.config) {
        Ok(config) => {
            info!(path = %args.config.display(), "loaded spur.conf");
            Some(config)
        }
        Err(e) => {
            warn!(
                path = %args.config.display(),
                error = %e,
                "failed to load spur.conf, using default config"
            );
            None
        }
    };
    let hooks_config = config.as_ref().map(|c| c.hooks.clone()).unwrap_or_default();

    // Background update check (non-blocking)
    spur_update::spawn_startup_check(
        "ROCm/spur",
        env!("CARGO_PKG_VERSION"),
        true,
        false, // auto_update
        "stable",
        "/var/cache/spur",
        spur_update::SPUR_BINARIES,
    );

    // Detect node address (explicit --address > WireGuard > hostname)
    let node_address = if let Some(ref addr) = args.address {
        info!(ip = %addr, "using explicit node address");
        spur_net::address::NodeAddress {
            ip: addr.clone(),
            hostname: hostname.clone(),
            port: listen_port,
            source: spur_net::address::AddressSource::Static,
        }
    } else {
        let wg_interface = std::env::var("SPUR_WG_INTERFACE").unwrap_or_else(|_| "spur0".into());
        spur_net::detect_node_address(&hostname, listen_port, &wg_interface)
    };
    info!(
        ip = %node_address.ip,
        port = node_address.port,
        source = ?node_address.source,
        "node address detected"
    );

    // Initialize device registry (CDI cache, GRES config, and discovery).
    let registry = init_device_registry(config.as_ref());
    let registry = Arc::new(Mutex::new(registry));

    // Discover local resources (CPU/memory from sysfs, GPUs from device registry)
    let resources = {
        let reg = registry.lock().await;
        reporter::discover_resources(&reg)
    };
    info!(
        cpus = resources.cpus,
        memory_mb = resources.memory_mb,
        gpus = resources.gpus.len(),
        "resources discovered"
    );

    // Parse node labels from CLI/env
    let labels: HashMap<String, String> = args
        .labels
        .iter()
        .filter_map(|s| {
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();

    // Create the node reporter
    let reporter = Arc::new(NodeReporter::new(
        hostname.clone(),
        args.controller.clone(),
        resources,
        node_address,
        labels,
    ));

    // Register with controller
    reporter.register().await?;

    // Start heartbeat loop
    let hb_reporter = reporter.clone();
    tokio::spawn(async move {
        hb_reporter.heartbeat_loop().await;
    });

    // Start agent gRPC server (receives job launches from spurctld)
    let agent_service =
        agent_server::AgentService::new(reporter.clone(), hooks_config, registry.clone());
    agent_service.start_monitor(args.controller.clone());

    let addr = args.listen.parse()?;
    info!(%addr, "agent gRPC server listening");

    tonic::transport::Server::builder()
        .add_service(spur_proto::proto::slurm_agent_server::SlurmAgentServer::new(agent_service))
        .serve(addr)
        .await?;

    Ok(())
}

fn init_device_registry(config: Option<&SlurmConfig>) -> DeviceRegistry {
    let default_devices = spur_core::config::DevicesConfig::default();
    let devices_config = config.map(|c| &c.devices).unwrap_or(&default_devices);

    let cdi_cache = CdiCache::load(&devices_config.cdi_spec_dirs, devices_config.auto_detect);

    let gres_entries: Vec<spur_devices::GresEntry> = devices_config
        .gres
        .iter()
        .map(|g| spur_devices::GresEntry {
            name: g.name.clone(),
            r#type: g.r#type.clone(),
            file: g.file.clone(),
            multiple_files: g.multiple_files.clone(),
            count: g.count,
            cores: g.cores.clone(),
            links: g.links.clone(),
            flags: g.flags.clone(),
        })
        .collect();
    let gres_cache = spur_devices::GresCache::from_entries(&gres_entries);

    let mut registry = DeviceRegistry::new();
    registry.populate(&cdi_cache, &gres_cache);

    info!(
        injectable_devices = registry.injectable_count(),
        countable = registry.countable_count(),
        "device registry initialized"
    );

    registry
}
