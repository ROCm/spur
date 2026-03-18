mod agent;
mod crd;
mod health;
mod job_controller;
mod node_watcher;

use std::net::SocketAddr;

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

    /// K8s namespace for SpurJobs and Pods
    #[arg(long, default_value = "spur")]
    namespace: String,

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

    let listen_addr: SocketAddr = args.listen.parse()?;
    let operator_ip = if listen_addr.ip().is_unspecified() {
        hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "127.0.0.1".into())
    } else {
        listen_addr.ip().to_string()
    };
    let operator_port = listen_addr.port() as u32;

    // Spawn health/readiness server
    let health_addr: SocketAddr = args.health_addr.parse()?;
    let health_ctrl_addr = args.controller_addr.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        if let Err(e) = health::serve(health_addr, health_client, health_ctrl_addr).await {
            tracing::error!(error = %e, "health server exited");
        }
    });

    // Spawn node watcher
    let nw_client = client.clone();
    let nw_ctrl_addr = args.controller_addr.clone();
    let nw_op_addr = operator_ip.clone();
    let nw_ns = args.namespace.clone();
    let nw_selector = args.node_selector.clone();
    tokio::spawn(async move {
        if let Err(e) = node_watcher::run(
            nw_client,
            nw_ctrl_addr,
            nw_op_addr,
            operator_port,
            nw_ns,
            nw_selector,
        )
        .await
        {
            tracing::error!(error = %e, "node watcher exited");
        }
    });

    // Spawn job controller
    let jc_client = client.clone();
    let jc_ctrl_addr = args.controller_addr.clone();
    let jc_ns = args.namespace.clone();
    tokio::spawn(async move {
        if let Err(e) = job_controller::run(jc_client, jc_ctrl_addr, jc_ns).await {
            tracing::error!(error = %e, "job controller exited");
        }
    });

    // Start virtual agent gRPC server
    let virtual_agent = agent::VirtualAgent::new(client, args.namespace);
    info!(%listen_addr, "virtual agent gRPC server listening");

    tonic::transport::Server::builder()
        .add_service(SlurmAgentServer::new(virtual_agent))
        .serve(listen_addr)
        .await?;

    Ok(())
}

fn generate_crd() {
    use kube::CustomResourceExt;
    let crd = crd::SpurJob::crd();
    print!(
        "{}",
        serde_json::to_string_pretty(&crd).expect("CRD serialization failed")
    );
}
