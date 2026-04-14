mod agent;
mod crd;
mod health;
mod heartbeat;
mod job_controller;
mod node_watcher;

use std::net::SocketAddr;
use std::time::Duration;

use backoff::{backoff::Backoff, ExponentialBackoffBuilder};
use clap::{Parser, Subcommand};
use kube::Client;
use tracing::info;

use spur_proto::proto::slurm_agent_server::SlurmAgentServer;

#[derive(Parser)]
#[command(
    name = "spur-k8s-operator",
    about = "Spur Kubernetes operator — bridges K8s and Spur scheduling"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// spurctld gRPC address
    #[arg(long, default_value = "localhost:6817")]
    controller_addr: String,

    /// gRPC listen address for the virtual agent
    #[arg(long, default_value = "[::]:6818")]
    listen: String,

    /// Advertised address for spurctld to reach this operator.
    /// If unset, falls back to POD_IP env var, then hostname.
    /// In K8s, set this to the Service DNS name or use the Downward API
    /// to inject the Pod IP (issue #51).
    #[arg(long, env = "SPUR_OPERATOR_ADDRESS")]
    address: Option<String>,

    /// K8s node label selector
    #[arg(long, default_value = "spur.ai/managed=true")]
    node_selector: String,

    /// HTTP health/metrics server address
    #[arg(long, default_value = "[::]:8080")]
    health_addr: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[derive(Subcommand)]
enum Command {
    /// Print the SpurJob CRD YAML to stdout for `kubectl apply`.
    GenerateCrd,

    /// Run the operator (default if no subcommand given).
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    match args.command.as_ref().unwrap_or(&Command::Run) {
        Command::GenerateCrd => {
            generate_crd();
            return Ok(());
        }
        Command::Run => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log_level.parse().unwrap()),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "spur-k8s-operator starting"
    );

    let client = Client::try_default().await?;

    // Operator's own namespace: injected by K8s Downward API as POD_NAMESPACE.
    // Empty string if not set; callers that require it should validate and error out.
    let operator_namespace = std::env::var("POD_NAMESPACE").unwrap_or_default();

    let listen_addr: SocketAddr = args.listen.parse()?;
    // Issue #51: Use explicit --address flag, then POD_IP env var (K8s Downward
    // API), then listen IP, then hostname. Pod hostnames are unroutable —
    // spurctld can't reach the operator at "spur-k8s-operator-abc123".
    let operator_ip = if let Some(ref addr) = args.address {
        addr.clone()
    } else if let Ok(pod_ip) = std::env::var("POD_IP") {
        pod_ip
    } else if !listen_addr.ip().is_unspecified() {
        listen_addr.ip().to_string()
    } else {
        hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "127.0.0.1".into())
    };
    let operator_port = listen_addr.port() as u32;
    info!(address = %operator_ip, port = operator_port, "operator will advertise this address to spurctld");

    // Spawn health/readiness server (issue #52: retry on failure)
    let health_addr: SocketAddr = args.health_addr.parse()?;
    let health_ctrl_addr = args.controller_addr.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        run_with_retry("health server", || {
            let c = health_client.clone();
            let addr = health_ctrl_addr.clone();
            Box::pin(health::serve(health_addr, c, addr))
        })
        .await;
    });

    // HeartbeatManager is created once so tracked nodes survive watcher restarts.
    let hb = std::sync::Arc::new(heartbeat::HeartbeatManager::new(
        args.controller_addr.clone(),
    ));

    // Spawn heartbeat sender.
    let hb_task = hb.clone();
    tokio::spawn(async move {
        run_with_retry("node heartbeat", || {
            let h = hb_task.clone();
            Box::pin(async move { h.run().await })
        })
        .await;
    });

    // Spawn node watcher — only calls hb.track / hb.untrack, never sends pings.
    let nw_client = client.clone();
    let nw_ctrl_addr = args.controller_addr.clone();
    let nw_op_addr = operator_ip.clone();
    let nw_selector = args.node_selector.clone();
    tokio::spawn(async move {
        run_with_retry("node watcher", || {
            let c = nw_client.clone();
            let ctrl = nw_ctrl_addr.clone();
            let op = nw_op_addr.clone();
            let sel = nw_selector.clone();
            let hb = hb.clone();
            Box::pin(node_watcher::run(c, ctrl, op, operator_port, sel, hb))
        })
        .await;
    });

    // Spawn job controller (issue #52: retry on failure)
    let jc_client = client.clone();
    let jc_ctrl_addr = args.controller_addr.clone();
    let jc_ns = operator_namespace.clone();
    tokio::spawn(async move {
        run_with_retry("job controller", || {
            let c = jc_client.clone();
            let ctrl = jc_ctrl_addr.clone();
            let ns = jc_ns.clone();
            Box::pin(job_controller::run(c, ctrl, ns))
        })
        .await;
    });

    // Start virtual agent gRPC server
    let virtual_agent = agent::VirtualAgent::new(client);
    info!(%listen_addr, "virtual agent gRPC server listening");

    tonic::transport::Server::builder()
        .add_service(SlurmAgentServer::new(virtual_agent))
        .serve(listen_addr)
        .await?;

    Ok(())
}

/// Run an async task with exponential backoff retry on failure (issue #52).
///
/// If the task exits with an error, it is restarted after a delay that doubles
/// each time (1s → 2s → 4s → ... → 60s max). On success the backoff resets.
async fn run_with_retry<F, Fut>(name: &str, mut factory: F) -> !
where
    F: FnMut() -> std::pin::Pin<Box<Fut>>,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut eb = ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_secs(1))
        .with_multiplier(2.0)
        .with_max_interval(Duration::from_secs(60))
        .with_max_elapsed_time(None)
        .build();

    loop {
        match factory().await {
            Ok(()) => {
                tracing::warn!(%name, "task exited cleanly, restarting");
                eb.reset();
            }
            Err(e) => {
                let delay = eb.next_backoff().unwrap_or(Duration::from_secs(60));
                tracing::error!(%name, error = %e, delay_secs = delay.as_secs(), "task failed, retrying");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn generate_crd() {
    use kube::CustomResourceExt;
    let crd = crd::SpurJob::crd();
    print!(
        "{}",
        serde_json::to_string_pretty(&crd).expect("CRD serialization failed")
    );
}
