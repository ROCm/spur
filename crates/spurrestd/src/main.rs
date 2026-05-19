// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod routes;

use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Parser)]
#[command(name = "spurrestd", about = "Spur REST API daemon (spurrestd)")]
struct Args {
    /// REST API listen address
    #[arg(long, default_value = "0.0.0.0:6820")]
    listen: String,

    /// spurctl gRPC address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    controller: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[derive(Clone)]
pub struct AppState {
    pub controller_addr: String,
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

    let state = Arc::new(AppState {
        controller_addr: args.controller,
    });

    let app = Router::new()
        // Native Spur API
        .nest("/api/v1", routes::spur_routes())
        // Slurm-compatible API (drop-in replacement)
        .nest("/slurm/v0.0.42", routes::spur_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    info!(addr = %args.listen, "spurrestd listening");
    axum::serve(listener, app).await?;

    Ok(())
}
