mod cluster;
mod raft;
mod raft_server;
mod scheduler_loop;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing::info;

use cluster::ClusterManager;

#[derive(Parser)]
#[command(name = "spurctld", about = "Spur controller daemon (spurctld)")]
struct Args {
    /// Configuration file path
    #[arg(short = 'f', long, default_value = "/etc/spur/spur.conf")]
    config: PathBuf,

    /// gRPC listen address (overrides config file)
    #[arg(long)]
    listen: Option<String>,

    /// State directory
    #[arg(long, default_value = "/var/spool/spur")]
    state_dir: PathBuf,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Foreground mode (don't daemonize)
    #[arg(short = 'D', long)]
    foreground: bool,
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

    info!(version = env!("CARGO_PKG_VERSION"), "spurctld starting");

    // Load config if it exists, otherwise use defaults
    let mut config = if args.config.exists() {
        spur_core::config::SlurmConfig::load(&args.config)?
    } else {
        info!("no config file found, using defaults");
        spur_core::config::SlurmConfig {
            cluster_name: "spur".into(),
            controller: spur_core::config::ControllerConfig {
                listen_addr: "[::]:6817".into(),
                state_dir: args.state_dir.to_string_lossy().into(),
                ..Default::default()
            },
            ..default_config()
        }
    };

    // CLI --listen overrides config file; otherwise use config's listen_addr.
    let listen_addr = args
        .listen
        .clone()
        .unwrap_or_else(|| config.controller.listen_addr.clone());

    // Keep config in sync so downstream code sees the final address.
    config.controller.listen_addr = listen_addr.clone();

    let raft_handle: Option<Arc<raft::RaftHandle>> = if !config.controller.peers.is_empty() {
        let node_id = config
            .controller
            .node_id
            .or_else(raft::detect_node_id_from_hostname)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Raft peers configured but node_id could not be determined. \
                 Set controller.node_id in spur.conf or use a hostname ending \
                 in -N (e.g. spurctld-0)."
                )
            })?;
        info!(
            node_id,
            peers = ?config.controller.peers,
            "initializing Raft consensus"
        );
        let handle = raft::start_raft(node_id, &config.controller.peers, &args.state_dir).await?;
        info!(node_id, "Raft node started");

        let raft_addr: std::net::SocketAddr = config.controller.raft_listen_addr.parse()?;
        let raft_instance = handle.raft.clone();
        tokio::spawn(async move {
            if let Err(e) = raft_server::serve_raft(raft_addr, raft_instance).await {
                tracing::error!(error = %e, "raft internal gRPC server failed");
            }
        });

        Some(Arc::new(handle))
    } else {
        info!("single-node mode (no peers configured)");
        None
    };

    // Initialize cluster manager
    let cluster = Arc::new(ClusterManager::new(config.clone(), &args.state_dir)?);

    if let Some(ref rh) = raft_handle {
        rh.store.set_applier(cluster.clone());
        cluster.set_raft(rh.raft.clone());
    }

    // Start scheduler loop (only schedules when this node is Raft leader)
    let sched_cluster = cluster.clone();
    let sched_raft = raft_handle.clone();
    let sched_handle = tokio::spawn(async move {
        scheduler_loop::run(sched_cluster, sched_raft).await;
    });

    // Start node health checker (90s timeout, only on leader)
    let health_cluster = cluster.clone();
    let health_raft = raft_handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Some(ref r) = health_raft {
                if !r.is_leader() {
                    continue;
                }
            }
            health_cluster.check_node_health(90);
        }
    });

    // Start gRPC server
    let addr = listen_addr.parse()?;
    info!(%addr, "gRPC server listening");
    server::serve(addr, cluster, raft_handle).await?;

    sched_handle.abort();
    Ok(())
}

fn default_config() -> spur_core::config::SlurmConfig {
    spur_core::config::SlurmConfig {
        cluster_name: "spur".into(),
        controller: Default::default(),
        accounting: Default::default(),
        scheduler: Default::default(),
        auth: Default::default(),
        partitions: vec![spur_core::config::PartitionConfig {
            name: "default".into(),
            default: true,
            state: "UP".into(),
            nodes: "localhost".into(),
            max_time: None,
            default_time: None,
            max_nodes: None,
            min_nodes: 1,
            allow_accounts: Vec::new(),
            allow_groups: Vec::new(),
            priority_tier: 1,
            preempt_mode: String::new(),
        }],
        nodes: Vec::new(),
        network: Default::default(),
        logging: Default::default(),
        kubernetes: Default::default(),
        notifications: Default::default(),
        power: Default::default(),
        federation: Default::default(),
        topology: None,
        licenses: std::collections::HashMap::new(),
    }
}
