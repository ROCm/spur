// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod db;
mod fairshare;
mod server;

use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "spurdbd", about = "Spur accounting daemon (spurdbd)")]
struct Args {
    /// Database URL
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgresql://spur:spur@localhost/spur"
    )]
    database_url: String,

    /// gRPC listen address
    #[arg(long, default_value = "[::]:6819")]
    listen: String,

    /// Foreground mode
    #[arg(short = 'D', long)]
    foreground: bool,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Run database migrations on startup
    #[arg(long)]
    migrate: bool,
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

    info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %args.listen,
        "spurdbd starting"
    );

    // Connect to database
    let pool = db::connect(&args.database_url).await?;

    // Run migrations if requested
    if args.migrate {
        db::migrate(&pool).await?;
        info!("database migrations complete");
    }

    // Start gRPC server
    let addr = args.listen.parse()?;
    info!(%addr, "accounting gRPC server listening");
    server::serve(addr, pool).await?;

    Ok(())
}
