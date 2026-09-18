// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur auth-keys` — generate JWKS files for native authentication.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(
    name = "auth-keys",
    about = "Generate JWKS files for native Spur authentication"
)]
pub struct AuthKeysArgs {
    #[command(subcommand)]
    pub command: AuthKeysCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthKeysCommand {
    /// HMAC (oct) signing set for user-RPC mint/verify (`auth.jwks`).
    Hmac {
        #[arg(long)]
        kid: String,
        #[arg(long)]
        out: String,
    },
    /// Ed25519 signing + verification pair (cred, node, or controller keys).
    Ed25519 {
        #[arg(long)]
        kid: String,
        #[arg(long)]
        signing: String,
        #[arg(long)]
        verify: String,
    },
}

pub fn main() -> Result<()> {
    main_with_args(std::env::args().collect())
}

pub fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = AuthKeysArgs::try_parse_from(args)?;
    match parsed.command {
        AuthKeysCommand::Hmac { kid, out } => {
            let doc = spur_core::native_jwks::generate_hmac_jwks(&kid);
            write_mode_0600(Path::new(&out), doc.as_bytes())
                .with_context(|| format!("write {out}"))?;
            eprintln!("wrote HMAC JWKS kid={kid} to {out} (mode 0600)");
            Ok(())
        }
        AuthKeysCommand::Ed25519 {
            kid,
            signing,
            verify,
        } => {
            let (sign, ver) = spur_core::native_jwks::generate_ed25519_jwks(&kid);
            write_mode_0600(Path::new(&signing), sign.as_bytes())
                .with_context(|| format!("write {signing}"))?;
            write_mode_0600(Path::new(&verify), ver.as_bytes())
                .with_context(|| format!("write {verify}"))?;
            eprintln!("wrote Ed25519 JWKS kid={kid} signing={signing} verify={verify} (mode 0600)");
            Ok(())
        }
    }
}

fn write_mode_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn parser_is_well_formed() {
        AuthKeysArgs::command().debug_assert();
    }
}
