mod cluster;
mod leader_election;
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

    /// Enable K8s Lease-based leader election (for HA deployments)
    #[arg(long)]
    enable_leader_election: bool,

    /// K8s namespace for the leader election Lease (default: spur)
    #[arg(long, default_value = "spur")]
    election_namespace: String,
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

    // If leader election is enabled, wait until we acquire the Lease
    if args.enable_leader_election {
        info!("leader election enabled, acquiring K8s Lease...");
        leader_election::acquire_lease(&args.election_namespace).await?;
        info!("leader election: this instance is now the leader");
    }

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

    // Initialize cluster manager
    let cluster = Arc::new(ClusterManager::new(config.clone(), &args.state_dir)?);

    // Start scheduler loop
    let sched_cluster = cluster.clone();
    let sched_handle = tokio::spawn(async move {
        scheduler_loop::run(sched_cluster).await;
    });

    // Start node health checker (90s timeout)
    let health_cluster = cluster.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            health_cluster.check_node_health(90);
        }
    });

    // Start gRPC server
    let addr = listen_addr.parse()?;
    info!(%addr, "gRPC server listening");
    server::serve(addr, cluster).await?;

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
