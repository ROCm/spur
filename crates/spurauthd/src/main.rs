// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Login-host credential mint. Controller and agent hosts embed the same
//! server in `spurctld` / `spurd`; this binary is for hosts that only run the CLI.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use spur_core::native_jwks::{HmacKeySet, AUTH_JWKS_PATH};
use spur_core::native_mint::{
    bind_socket, resolve_socket_path, serve, unix_now, CredentialMint, DEFAULT_LIFETIME_SECS,
};

#[derive(Parser, Debug)]
#[command(name = "spurauthd", about = "Spur native credential mint")]
struct Args {
    /// Cluster name (default socket `/run/spur/<cluster>/auth.sock`).
    #[arg(long)]
    cluster: String,
    /// Override the listen socket. `$SPUR_AUTH_SOCKET` also overrides the default.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// HMAC JWKS used to mint user-RPC credentials.
    #[arg(long, default_value = AUTH_JWKS_PATH)]
    jwks: PathBuf,
    /// Credential lifetime in seconds.
    #[arg(long, default_value_t = DEFAULT_LIFETIME_SECS)]
    lifetime_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let socket = match args.socket {
        Some(p) => p,
        None => resolve_socket_path(&args.cluster)?,
    };
    let now = unix_now()?;
    let keys = HmacKeySet::from_path(&args.jwks, now)
        .with_context(|| format!("loading {}", args.jwks.display()))?;
    let mint = Arc::new(CredentialMint::new(
        args.cluster,
        Arc::new(keys),
        args.lifetime_secs,
    )?);
    let listener = bind_socket(&socket).await?;
    info!(socket = %socket.display(), "spurauthd listening");
    serve(listener, mint).await;
    Ok(())
}
