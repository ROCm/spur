// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur token` subcommands for admission token management.

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Mint a USER credential for authenticating RPCs (distinct from the admission tokens above,
    /// which admit a node to the cluster).
    ///
    /// Signed locally from `[auth] jwt_key` in the config, so it needs read access to that file —
    /// run it on the controller host. Local signing is deliberate: under `[auth] mode = required`
    /// an RPC-based mint would need a credential to obtain a credential.
    ///
    /// Write the output to `~/.spur/token` (mode 0600) or export it as `SPUR_AUTH_TOKEN`.
    User {
        /// Username the token authenticates as.
        #[arg(long)]
        user: String,
        /// Mark the token as a cluster admin.
        #[arg(long)]
        admin: bool,
        /// Token time-to-live (e.g. "24h", "7d", "3600s"). Default 24h.
        #[arg(long)]
        ttl: Option<String>,
        /// Config file to read `[auth] jwt_key` from.
        #[arg(long, default_value = "/etc/spur/spur.conf")]
        config: String,
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
        TokenCommand::User {
            user,
            admin,
            ttl,
            config,
        } => cmd_user_token(&user, admin, ttl, &config),
    }
}

/// Mint a user credential locally from the configured signing key.
fn cmd_user_token(user: &str, admin: bool, ttl: Option<String>, config_path: &str) -> Result<()> {
    let ttl_secs = match ttl.as_deref() {
        Some(t) => parse_ttl(t)? as u64,
        None => 86_400,
    };
    let cfg = spur_core::config::SlurmConfig::load_from_file(std::path::Path::new(config_path))
        .map_err(|e| anyhow::anyhow!("read {config_path}: {e}"))?;
    let key = cfg.auth.jwt_key.as_deref().unwrap_or_default();
    if key.is_empty() {
        anyhow::bail!(
            "[auth] jwt_key is not set in {config_path}; a signing key is required to mint user \
             credentials (the same key is used for node admission)"
        );
    }
    // uid is carried for reference only — the controller re-resolves uid/gid from the username
    // through NSS, so a stale or wrong uid here cannot influence what a job runs as.
    let uid = spur_core::auth::resolve_unix_credentials(user)
        .map(|(uid, _)| uid)
        .unwrap_or(0);
    let token = spur_core::auth::generate_token(user, uid, admin, key.as_bytes(), ttl_secs)
        .map_err(|e| anyhow::anyhow!("mint token: {e}"))?;
    // stdout = the token alone, so it can be redirected straight into ~/.spur/token.
    println!("{token}");
    eprintln!(
        "minted a {} credential for {user}, valid {}h. Store it as ~/.spur/token (chmod 600) or \
         export SPUR_AUTH_TOKEN.",
        if admin { "cluster-admin" } else { "user" },
        ttl_secs / 3600
    );
    Ok(())
}

fn parse_ttl(s: &str) -> Result<u32> {
    let s = s.trim();
    let (value, unit_secs) = if let Some(days) = s.strip_suffix('d') {
        (days, 86_400u64)
    } else if let Some(hours) = s.strip_suffix('h') {
        (hours, 3_600)
    } else if let Some(mins) = s.strip_suffix('m') {
        (mins, 60)
    } else if let Some(secs) = s.strip_suffix('s') {
        (secs, 1)
    } else {
        (s, 1)
    };
    // u64 can't overflow here (max is u32::MAX * 86_400), so the message reports seconds vs seconds.
    let secs = value.parse::<u32>()? as u64 * unit_secs;
    if secs == 0 {
        anyhow::bail!("TTL {s} must be a positive duration");
    }
    if secs > u32::MAX as u64 {
        anyhow::bail!(
            "TTL {s} is {secs} seconds, over the {} second maximum",
            u32::MAX
        );
    }
    Ok(secs as u32)
}

async fn cmd_create(controller: &str, ttl: Option<String>) -> Result<()> {
    let ttl_secs = ttl.map(|t| parse_ttl(&t)).transpose()?;

    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
    let resp = client.create_token(CreateTokenRequest { ttl_secs }).await?;

    let inner = resp.into_inner();
    println!("{}", inner.token);
    eprintln!("Token ID: {}", inner.token_id);
    Ok(())
}

async fn cmd_list(controller: &str) -> Result<()> {
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
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
    let mut client = spur_proto::controller_client(crate::authclient::connect(controller).await?);
    client
        .revoke_token(RevokeTokenRequest {
            token_id: token_id.to_string(),
        })
        .await?;
    println!("Token {} revoked.", token_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ttl_expands_each_suffix() {
        assert_eq!(parse_ttl("30s").unwrap(), 30);
        assert_eq!(parse_ttl("5m").unwrap(), 300);
        assert_eq!(parse_ttl("2h").unwrap(), 7_200);
        assert_eq!(parse_ttl("1d").unwrap(), 86_400);
        assert_eq!(parse_ttl("90").unwrap(), 90, "bare value is seconds");
        assert_eq!(parse_ttl("  7h  ").unwrap(), 25_200, "padding is trimmed");
    }

    /// 50000d is 4.32e9 seconds, past u32, where the unchecked multiply wrapped
    /// to roughly 290 days and silently issued a shorter-lived token.
    #[test]
    fn parse_ttl_rejects_values_that_overflow() {
        assert!(parse_ttl("50000d").is_err());
        assert!(parse_ttl("2000000h").is_err());
        assert!(parse_ttl("100000000m").is_err());
        assert_eq!(
            parse_ttl("4294967295").unwrap(),
            u32::MAX,
            "the ceiling still parses"
        );
    }

    #[test]
    fn parse_ttl_rejects_zero() {
        // A zero TTL used to collapse to a never-expiring token at the server.
        assert!(parse_ttl("0").is_err());
        assert!(parse_ttl("0d").is_err());
        assert!(parse_ttl("0h").is_err());
        assert!(parse_ttl("0s").is_err());
    }

    #[test]
    fn parse_ttl_rejects_non_numeric_values() {
        assert!(parse_ttl("").is_err());
        assert!(parse_ttl("abc").is_err());
        assert!(parse_ttl("-1h").is_err());
        assert!(parse_ttl("1.5h").is_err());
        assert!(parse_ttl("h").is_err());
    }
}
