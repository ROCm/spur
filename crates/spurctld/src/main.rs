// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod accounting;
mod association_cache;
mod cluster;
mod fairshare_cache;
mod limits_cache;
mod metrics_proto;
mod metrics_server;
mod raft;
mod raft_server;
mod rest;
mod rpc_middleware;
mod rpc_stats;
mod sched_stats;
mod scheduler_loop;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing::info;

use cluster::ClusterManager;
use rpc_stats::RpcStatsCollector;
use sched_stats::SchedStatsCollector;

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
        spur_core::config::SlurmConfig::load_from_file(&args.config)?
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

    // Background update check (non-blocking — does not delay startup)
    spur_update::spawn_startup_check(
        "ROCm/spur",
        env!("CARGO_PKG_VERSION"),
        config.update.check_on_startup,
        config.update.auto_update,
        &config.update.channel,
        &config.update.cache_dir,
        spur_update::SPUR_BINARIES,
    );

    // Initialize cluster manager first so Raft recovery can apply entries
    let cluster = Arc::new(ClusterManager::new(config.clone(), &args.state_dir)?);

    // Raft is always-on. When no peers are configured, run a single-node
    // cluster that self-elects instantly (same pattern as Apache Kudu).
    let (peers, node_id) = if config.controller.peers.is_empty() {
        let raft_addr = config.controller.raft_listen_addr.clone();
        info!("single-node Raft mode (no peers configured)");
        (vec![raft_addr], 1u64)
    } else {
        let id = config
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
            node_id = id,
            peers = ?config.controller.peers,
            "initializing Raft consensus"
        );
        (config.controller.peers.clone(), id)
    };

    let handle = raft::start_raft(node_id, &peers, &args.state_dir, cluster.clone()).await?;
    info!(node_id, "Raft node started");

    let raft_addr: std::net::SocketAddr = config.controller.raft_listen_addr.parse()?;
    let raft_instance = handle.raft.clone();
    tokio::spawn(async move {
        if let Err(e) = raft_server::serve_raft(raft_addr, raft_instance).await {
            tracing::error!(error = %e, "raft internal gRPC server failed");
        }
    });

    let raft_handle = Arc::new(handle);
    cluster.set_raft(raft_handle.raft.clone());

    let sched_stats = Arc::new(SchedStatsCollector::new(config.scheduler.plugin.clone()));
    cluster.set_sched_stats(sched_stats.clone());

    // Build accounting PgPool (best-effort — scheduling works without it)
    let accounting_pool = if config.accounting.database_url.is_empty() {
        info!("accounting disabled (database_url not configured)");
        None
    } else {
        match sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&config.accounting.database_url)
            .await
        {
            Ok(pool) => {
                if let Err(e) = accounting::db::migrate(&pool).await {
                    tracing::error!(error = %e, "accounting migration failed; disabling accounting");
                    None
                } else {
                    info!("accounting database connected");
                    let notifier = accounting::AccountingNotifier::new(pool.clone());
                    cluster.set_accounting(notifier);

                    cluster.fairshare_cache().spawn_refresh_loop(
                        pool.clone(),
                        config.scheduler.fairshare_halflife_days,
                        config.accounting.fairshare_refresh_secs as u64,
                    );

                    cluster.qos_cache().spawn_refresh_loop(
                        pool.clone(),
                        config.accounting.fairshare_refresh_secs as u64,
                    );

                    cluster.association_cache().spawn_refresh_loop(
                        pool.clone(),
                        config.accounting.fairshare_refresh_secs as u64,
                    );

                    Some(pool)
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to connect to accounting database; job history will not be recorded"
                );
                None
            }
        }
    };

    // Start scheduler loop (only schedules when this node is Raft leader)
    let sched_cluster = cluster.clone();
    let sched_raft = raft_handle.clone();
    let sched_handle = tokio::spawn(async move {
        scheduler_loop::run(sched_cluster, sched_raft).await;
    });

    // Start node health checker (only on leader).
    let hb_timeout = config.controller.heartbeat_timeout_secs.unwrap_or(90);
    let health_cluster = cluster.clone();
    let health_raft = raft_handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            if !health_raft.is_leader() {
                continue;
            }
            let evicted = health_cluster.check_node_health(hb_timeout);
            for fin in &evicted {
                if let Some(job) = health_cluster.get_job(fin.job_id) {
                    let c = health_cluster.clone();
                    tokio::spawn(async move {
                        crate::scheduler_loop::send_cancel_to_agents(&c, &job, 9).await;
                    });
                }
            }
            health_cluster.complete_evicted_steps(&evicted);
        }
    });

    let rpc_stats = Arc::new(RpcStatsCollector::new());

    if config.metrics.enabled {
        let metrics_addr = config
            .metrics
            .effective_listen_addr()
            .map_err(|e| anyhow::anyhow!(e))?;
        let metrics_cluster = cluster.clone();
        let metrics_raft = raft_handle.clone();
        let metrics_rpc_stats = rpc_stats.clone();
        let metrics_sched_stats = sched_stats.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics_server::serve(
                metrics_addr,
                metrics_cluster,
                metrics_raft,
                metrics_rpc_stats,
                metrics_sched_stats,
            )
            .await
            {
                tracing::error!(error = %e, "OpenMetrics metrics server failed");
            }
        });
    }

    if config.rest_api.enabled {
        let rest_addr: std::net::SocketAddr = config.controller.rest_addr.parse()?;
        let rest_cluster = cluster.clone();
        let rest_raft = raft_handle.clone();
        tokio::spawn(async move {
            if let Err(e) = rest::serve(rest_addr, rest_cluster, rest_raft).await {
                tracing::error!(error = %e, "REST API server failed");
            }
        });
    }

    // Start gRPC server
    let addr = listen_addr.parse()?;
    info!(%addr, "gRPC server listening");
    server::serve(
        addr,
        cluster,
        raft_handle,
        rpc_stats,
        sched_stats,
        accounting_pool,
    )
    .await?;

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
            selector: Default::default(),
            max_time: None,
            default_time: None,
            max_nodes: None,
            min_nodes: 1,
            allow_accounts: Vec::new(),
            allow_groups: Vec::new(),
            deny_accounts: Vec::new(),
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
        isolation: Default::default(),
        licenses: std::collections::HashMap::new(),
        burst_buffer: Default::default(),
        update: Default::default(),
        metrics: Default::default(),
        rest_api: Default::default(),
        hooks: Default::default(),
        devices: Default::default(),
        admission: Default::default(),
    }
}
