// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! WireGuard key generation and config file management.
//!
//! Shells out to `wg` and `wg-quick` which are standard on any Linux
//! system with WireGuard installed (in-kernel since Linux 5.6).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use tracing::info;

/// A WireGuard keypair (base64-encoded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgKeypair {
    pub private_key: String,
    pub public_key: String,
}

/// A WireGuard peer entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgPeer {
    pub public_key: String,
    pub allowed_ips: String,
    /// Remote endpoint in `host:port` format. None for the server config
    /// when peers connect inbound.
    pub endpoint: Option<String>,
    pub persistent_keepalive: Option<u16>,
}

/// Full WireGuard interface config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WgConfig {
    pub private_key: String,
    pub address: String,
    pub listen_port: Option<u16>,
    pub peers: Vec<WgPeer>,
}

/// Generate a new WireGuard keypair by calling `wg genkey` and `wg pubkey`.
pub fn generate_keypair() -> anyhow::Result<WgKeypair> {
    let genkey = Command::new("wg")
        .arg("genkey")
        .output()
        .context("failed to run `wg genkey` — is wireguard-tools installed?")?;
    if !genkey.status.success() {
        bail!(
            "wg genkey failed: {}",
            String::from_utf8_lossy(&genkey.stderr)
        );
    }
    let private_key = String::from_utf8(genkey.stdout)
        .context("wg genkey produced non-UTF8")?
        .trim()
        .to_string();

    let pubkey = Command::new("wg")
        .arg("pubkey")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn `wg pubkey`")?;

    use std::io::Write;
    let mut child = pubkey;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(private_key.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "wg pubkey failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let public_key = String::from_utf8(output.stdout)
        .context("wg pubkey produced non-UTF8")?
        .trim()
        .to_string();

    Ok(WgKeypair {
        private_key,
        public_key,
    })
}

impl WgConfig {
    /// Render as a wg-quick compatible config file.
    pub fn to_ini(&self) -> String {
        let mut out = String::new();
        out.push_str("[Interface]\n");
        out.push_str(&format!("PrivateKey = {}\n", self.private_key));
        out.push_str(&format!("Address = {}\n", self.address));
        if let Some(port) = self.listen_port {
            out.push_str(&format!("ListenPort = {}\n", port));
        }

        for peer in &self.peers {
            out.push_str("\n[Peer]\n");
            out.push_str(&format!("PublicKey = {}\n", peer.public_key));
            out.push_str(&format!("AllowedIPs = {}\n", peer.allowed_ips));
            if let Some(ref ep) = peer.endpoint {
                out.push_str(&format!("Endpoint = {}\n", ep));
            }
            if let Some(ka) = peer.persistent_keepalive {
                out.push_str(&format!("PersistentKeepalive = {}\n", ka));
            }
        }

        out
    }

    /// Write config to a file (e.g. `/etc/wireguard/spur0.conf`).
    pub fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        let content = self.to_ini();
        std::fs::write(path, &content)
            .with_context(|| format!("failed to write WireGuard config to {}", path.display()))?;

        // Restrict permissions: owner-only read/write
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// Bring up a WireGuard interface using wg-quick.
pub fn interface_up(interface: &str) -> anyhow::Result<()> {
    let output = Command::new("wg-quick")
        .args(["up", interface])
        .output()
        .context("failed to run wg-quick up")?;
    if !output.status.success() {
        bail!(
            "wg-quick up {} failed: {}",
            interface,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    info!(interface, "WireGuard interface up");
    Ok(())
}

/// Bring down a WireGuard interface using wg-quick.
pub fn interface_down(interface: &str) -> anyhow::Result<()> {
    let output = Command::new("wg-quick")
        .args(["down", interface])
        .output()
        .context("failed to run wg-quick down")?;
    if !output.status.success() {
        // Not fatal — interface may not be up
        tracing::warn!(
            interface,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "wg-quick down failed (interface may not be up)"
        );
    }
    Ok(())
}

/// Add a peer to a running WireGuard interface without restarting.
pub fn add_peer(interface: &str, peer: &WgPeer) -> anyhow::Result<()> {
    let mut args = vec![
        "set".to_string(),
        interface.to_string(),
        "peer".to_string(),
        peer.public_key.clone(),
        "allowed-ips".to_string(),
        peer.allowed_ips.clone(),
    ];
    if let Some(ref ep) = peer.endpoint {
        args.push("endpoint".to_string());
        args.push(ep.clone());
    }
    if let Some(ka) = peer.persistent_keepalive {
        args.push("persistent-keepalive".to_string());
        args.push(ka.to_string());
    }

    let output = Command::new("wg")
        .args(&args)
        .output()
        .context("failed to run `wg set`")?;
    if !output.status.success() {
        bail!(
            "wg set peer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Remove a peer from the mesh interface by its public key (the counterpart to `add_peer`, used
/// when a node leaves the cluster: `wg set <iface> peer <key> remove`). Idempotent.
pub fn remove_peer(interface: &str, public_key: &str) -> anyhow::Result<()> {
    let output = Command::new("wg")
        .args(["set", interface, "peer", public_key, "remove"])
        .output()
        .context("failed to run `wg set peer remove`")?;
    if !output.status.success() {
        bail!(
            "wg set peer remove failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Add (or replace) a kernel route for `cidr` via the WireGuard interface.
///
/// Used to route a peer's pod CIDR over the mesh when a CNI runs in
/// native-routing mode (no overlay) on top of WireGuard. `wg set allowed-ips`
/// only updates the cryptokey routing table — it does not install a kernel
/// route — so this is required alongside it. Uses `ip route replace` so it is
/// idempotent (add-or-update).
pub fn add_route(interface: &str, cidr: &str) -> anyhow::Result<()> {
    let output = Command::new("ip")
        .args(["route", "replace", cidr, "dev", interface])
        .output()
        .context("failed to run `ip route replace` — is iproute2 installed?")?;
    if !output.status.success() {
        bail!(
            "ip route replace {} dev {} failed: {}",
            cidr,
            interface,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    info!(cidr, interface, "route programmed over WireGuard");
    Ok(())
}

/// List the public keys of the interface's current WireGuard peers (`wg show <iface> peers`), so a
/// reconcile can prune peers no longer in the desired membership.
pub fn list_peers(interface: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("wg")
        .args(["show", interface, "peers"])
        .output()
        .context("failed to run `wg show peers`")?;
    if !output.status.success() {
        bail!(
            "wg show {interface} peers failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// The public key of an existing WireGuard interface (`wg show <iface> public-key`).
pub fn interface_public_key(interface: &str) -> anyhow::Result<String> {
    let output = Command::new("wg")
        .args(["show", interface, "public-key"])
        .output()
        .context("failed to run `wg show public-key`")?;
    if !output.status.success() {
        bail!(
            "wg show {interface} public-key failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_to_ini() {
        let config = WgConfig {
            private_key: "aPrivateKeyBase64=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: Some(51820),
            peers: vec![WgPeer {
                public_key: "peerPubKeyBase64=".into(),
                allowed_ips: "10.44.0.2/32".into(),
                endpoint: Some("203.0.113.10:51820".into()),
                persistent_keepalive: Some(25),
            }],
        };
        let ini = config.to_ini();
        assert!(ini.contains("[Interface]"));
        assert!(ini.contains("PrivateKey = aPrivateKeyBase64="));
        assert!(ini.contains("ListenPort = 51820"));
        assert!(ini.contains("[Peer]"));
        assert!(ini.contains("Endpoint = 203.0.113.10:51820"));
        assert!(ini.contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn test_config_no_listen_port() {
        let config = WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.2/16".into(),
            listen_port: None,
            peers: vec![],
        };
        let ini = config.to_ini();
        assert!(!ini.contains("ListenPort"));
    }
}
