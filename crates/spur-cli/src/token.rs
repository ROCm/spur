// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur token` subcommands for admission token management.

use anyhow::Result;
use clap::{Parser, Subcommand};

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{CreateTokenRequest, ListTokensRequest, RevokeTokenRequest};

#[derive(Parser, Debug)]
#[command(name = "token", about = "Manage admission tokens")]
pub struct TokenArgs {
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    controller: String,

    #[command(subcommand)]
    pub command: TokenCommand,
}

#[derive(Subcommand, Debug)]
pub enum TokenCommand {
    /// Create a new admission token.
    Create {
        /// Token time-to-live (e.g., "24h", "7d", "3600s").
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List all admission tokens.
    List,
    /// Revoke an admission token by ID.
    Revoke {
        /// Token ID to revoke.
        token_id: String,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = TokenArgs::try_parse_from(args)?;
    let controller = parsed.controller;
    match parsed.command {
        TokenCommand::Create { ttl } => cmd_create(&controller, ttl).await,
        TokenCommand::List => cmd_list(&controller).await,
        TokenCommand::Revoke { token_id } => cmd_revoke(&controller, &token_id).await,
    }
}

fn parse_ttl(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(days) = s.strip_suffix('d') {
        Ok(days.parse::<u32>()? * 86400)
    } else if let Some(hours) = s.strip_suffix('h') {
        Ok(hours.parse::<u32>()? * 3600)
    } else if let Some(mins) = s.strip_suffix('m') {
        Ok(mins.parse::<u32>()? * 60)
    } else if let Some(secs) = s.strip_suffix('s') {
        Ok(secs.parse::<u32>()?)
    } else {
        Ok(s.parse::<u32>()?)
    }
}

async fn cmd_create(controller: &str, ttl: Option<String>) -> Result<()> {
    let ttl_secs = ttl.map(|t| parse_ttl(&t)).transpose()?;

    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client.create_token(CreateTokenRequest { ttl_secs }).await?;

    let inner = resp.into_inner();
    println!("{}", inner.token);
    eprintln!("Token ID: {}", inner.token_id);
    Ok(())
}

async fn cmd_list(controller: &str) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    let resp = client.list_tokens(ListTokensRequest {}).await?;
    let tokens = resp.into_inner().tokens;

    if tokens.is_empty() {
        println!("No tokens.");
        return Ok(());
    }

    println!("{:<8} {:<24} {:<24} STATUS", "ID", "CREATED", "EXPIRES");
    for t in tokens {
        let expires = if t.expires_at.is_empty() {
            "never".to_string()
        } else {
            t.expires_at.clone()
        };
        let status = if t.revoked { "revoked" } else { "active" };
        println!(
            "{:<8} {:<24} {:<24} {}",
            t.id, t.created_at, expires, status
        );
    }
    Ok(())
}

async fn cmd_revoke(controller: &str, token_id: &str) -> Result<()> {
    let mut client = SlurmControllerClient::new(spur_client::connect_channel(controller).await?);
    client
        .revoke_token(RevokeTokenRequest {
            token_id: token_id.to_string(),
        })
        .await?;
    println!("Token {} revoked.", token_id);
    Ok(())
}
