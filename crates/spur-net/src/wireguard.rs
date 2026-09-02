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

/// Accumulates a `[Peer]` block's fields while parsing, before it is known whether the required
/// ones (public key, allowed IPs) were present.
#[derive(Default)]
struct PeerBuilder {
    public_key: Option<String>,
    allowed_ips: Option<String>,
    endpoint: Option<String>,
    persistent_keepalive: Option<u16>,
}

impl PeerBuilder {
    fn build(self) -> Option<WgPeer> {
        Some(WgPeer {
            public_key: self.public_key?,
            allowed_ips: self.allowed_ips?,
            endpoint: self.endpoint,
            persistent_keepalive: self.persistent_keepalive,
        })
    }
}

impl WgConfig {
    /// Parse a wg-quick compatible config file previously written by [`Self::to_ini`]. Tolerates
    /// blank lines and `#`/`;` comments so a manually-annotated file still parses — but [`Self::to_ini`]
    /// always regenerates a normalized file, so those annotations do not survive a subsequent write.
    pub fn parse(content: &str) -> anyhow::Result<Self> {
        let mut private_key = None;
        let mut address = None;
        let mut listen_port = None;
        let mut peers = Vec::new();
        let mut current_peer: Option<PeerBuilder> = None;
        let mut in_interface = false;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.eq_ignore_ascii_case("[Interface]") {
                peers.extend(current_peer.take().and_then(PeerBuilder::build));
                in_interface = true;
                continue;
            }
            if line.eq_ignore_ascii_case("[Peer]") {
                peers.extend(current_peer.take().and_then(PeerBuilder::build));
                in_interface = false;
                current_peer = Some(PeerBuilder::default());
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim().to_string());
            if in_interface {
                match key.to_ascii_lowercase().as_str() {
                    "privatekey" => private_key = Some(value),
                    "address" => address = Some(value),
                    "listenport" => listen_port = value.parse().ok(),
                    _ => {}
                }
            } else if let Some(peer) = current_peer.as_mut() {
                match key.to_ascii_lowercase().as_str() {
                    "publickey" => peer.public_key = Some(value),
                    "allowedips" => peer.allowed_ips = Some(value),
                    "endpoint" => peer.endpoint = Some(value),
                    "persistentkeepalive" => peer.persistent_keepalive = value.parse().ok(),
                    _ => {}
                }
            }
        }
        peers.extend(current_peer.take().and_then(PeerBuilder::build));

        Ok(WgConfig {
            private_key: private_key
                .ok_or_else(|| anyhow::anyhow!("config missing [Interface] PrivateKey"))?,
            address: address
                .ok_or_else(|| anyhow::anyhow!("config missing [Interface] Address"))?,
            listen_port,
            peers,
        })
    }

    /// Read and parse a config file written by [`Self::write_to`].
    pub fn read_from(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read WireGuard config from {}", path.display()))?;
        Self::parse(&content)
    }

    /// Insert or update a peer by public key, so the persisted config matches a live `wg set`.
    pub fn upsert_peer(&mut self, peer: WgPeer) {
        match self
            .peers
            .iter_mut()
            .find(|p| p.public_key == peer.public_key)
        {
            Some(existing) => *existing = peer,
            None => self.peers.push(peer),
        }
    }

    /// Remove a peer by public key. Returns whether a peer was removed.
    pub fn remove_peer_by_key(&mut self, public_key: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.public_key != public_key);
        self.peers.len() != before
    }

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

    /// Write config to a file (e.g. `/etc/wireguard/spur0.conf`), durably: this file is what
    /// `wg-quick` reads on every future boot, so a crash mid-write must never leave it truncated,
    /// and the write must survive a power loss, not just look atomic. Writes to a sibling temp file
    /// (unique per process), fsyncs it, `rename`s over the target, then fsyncs the directory too
    /// (a rename is itself just a directory-entry update, which can be lost on its own).
    pub fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        let content = self.to_ini();
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("wg");
        let tmp_path = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));

        let mut tmp_file = std::fs::File::create(&tmp_path).with_context(|| {
            format!(
                "failed to create temp WireGuard config at {}",
                tmp_path.display()
            )
        })?;
        tmp_file.write_all(content.as_bytes()).with_context(|| {
            format!("failed to write WireGuard config to {}", tmp_path.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        tmp_file
            .sync_all()
            .with_context(|| format!("failed to fsync {}", tmp_path.display()))?;
        drop(tmp_file);

        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to install WireGuard config at {}", path.display()))?;

        // Best-effort: not every filesystem supports fsync on a directory fd.
        match std::fs::File::open(dir) {
            Ok(dir_file) => {
                if let Err(e) = dir_file.sync_all() {
                    tracing::warn!(dir = %dir.display(), error = %e, "failed to fsync WireGuard config directory");
                }
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "failed to open WireGuard config directory for fsync");
            }
        }

        Ok(())
    }
}

/// Serialize concurrent CLI invocations mutating the same config file: an advisory exclusive lock
/// on a sibling `.lock` file, held for the duration of `f`'s read-modify-write. Without this, two
/// concurrent `add-peer` calls can each read the same base config and the second `write_to` silently
/// drops the first's change — the same "live but not persisted" drift this module exists to fix.
pub(crate) fn with_config_lock<T>(
    config_path: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let lock_path = config_path.with_extension("lock");
    // A never-initialized --config-dir has no directory at all yet; create it so `remove_peer_durable`
    // can still take the lock and reach its own "nothing to persist" check instead of failing here.
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create config directory {}", dir.display()))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    lock_file
        .lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    f()
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

/// Add a peer both live (`wg set`) and in the persisted config at `config_path`, so the peer
/// survives an interface reload (`wg-quick` restart, host reboot) instead of only existing in the
/// kernel's live state until the next one. Persists first: if the live `wg set` then fails, the
/// config already reflects the intended state for a future reload or retry to converge on.
pub fn add_peer_durable(interface: &str, config_path: &Path, peer: &WgPeer) -> anyhow::Result<()> {
    with_config_lock(config_path, || {
        let mut config = WgConfig::read_from(config_path).with_context(|| {
            format!(
                "no WireGuard config at {} — run `spur net init` or `spur net join` first",
                config_path.display()
            )
        })?;
        config.upsert_peer(peer.clone());
        config.write_to(config_path)?;
        add_peer(interface, peer)
    })
}

/// Remove a peer both live and from the persisted config at `config_path` — the counterpart to
/// [`add_peer_durable`]. Idempotent, matching [`remove_peer`]: a missing config file or an absent
/// peer are both treated as already-removed rather than an error. Persists the removal first for
/// the same reason [`add_peer_durable`] persists first.
pub fn remove_peer_durable(
    interface: &str,
    config_path: &Path,
    public_key: &str,
) -> anyhow::Result<()> {
    with_config_lock(config_path, || {
        if config_path.exists() {
            let mut config = WgConfig::read_from(config_path)?;
            config.remove_peer_by_key(public_key);
            config.write_to(config_path)?;
        }
        remove_peer(interface, public_key)
    })
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

/// Map each peer's pubkey to its known underlay endpoint (`wg show <iface> endpoints`); peers with
/// `(none)` are skipped. Lets the controller re-advertise worker↔worker endpoints in membership.
pub fn peer_endpoints(
    interface: &str,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let output = Command::new("wg")
        .args(["show", interface, "endpoints"])
        .output()
        .context("failed to run `wg show endpoints`")?;
    if !output.status.success() {
        bail!(
            "wg show {interface} endpoints failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(parse_peer_endpoints(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parse `wg show <iface> endpoints` stdout (tab-separated `<pubkey>\t<endpoint>` per line) into a
/// pubkey→endpoint map. Peers with an empty or `(none)` endpoint are skipped. Split out from
/// [`peer_endpoints`] so the parsing is unit-testable without invoking `wg`.
fn parse_peer_endpoints(stdout: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let (Some(key), Some(endpoint)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (key, endpoint) = (key.trim(), endpoint.trim());
        if key.is_empty() || endpoint.is_empty() || endpoint == "(none)" {
            continue;
        }
        map.insert(key.to_string(), endpoint.to_string());
    }
    map
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
    fn parse_round_trips_to_ini() {
        let config = WgConfig {
            private_key: "aPrivateKeyBase64=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: Some(51820),
            peers: vec![
                WgPeer {
                    public_key: "peerA=".into(),
                    allowed_ips: "10.44.0.2/32".into(),
                    endpoint: Some("203.0.113.10:51820".into()),
                    persistent_keepalive: Some(25),
                },
                WgPeer {
                    public_key: "peerB=".into(),
                    allowed_ips: "10.44.0.3/32".into(),
                    endpoint: None,
                    persistent_keepalive: None,
                },
            ],
        };
        let parsed = WgConfig::parse(&config.to_ini()).unwrap();
        assert_eq!(parsed.private_key, config.private_key);
        assert_eq!(parsed.address, config.address);
        assert_eq!(parsed.listen_port, config.listen_port);
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].public_key, "peerA=");
        assert_eq!(
            parsed.peers[0].endpoint.as_deref(),
            Some("203.0.113.10:51820")
        );
        assert_eq!(parsed.peers[1].public_key, "peerB=");
        assert_eq!(parsed.peers[1].endpoint, None);
    }

    #[test]
    fn parse_tolerates_comments_and_blank_lines() {
        let ini = "# generated by spur\n\
                    [Interface]\n\
                    PrivateKey = key=\n\
                    \n\
                    Address = 10.44.0.1/16\n\
                    ; trailing comment\n\
                    \n\
                    [Peer]\n\
                    PublicKey = peerA=\n\
                    AllowedIPs = 10.44.0.2/32\n";
        let config = WgConfig::parse(ini).unwrap();
        assert_eq!(config.private_key, "key=");
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].public_key, "peerA=");
    }

    #[test]
    fn parse_errors_on_missing_interface_fields() {
        assert!(WgConfig::parse("[Interface]\nAddress = 10.44.0.1/16\n").is_err());
        assert!(WgConfig::parse("[Interface]\nPrivateKey = key=\n").is_err());
    }

    #[test]
    fn parse_drops_incomplete_peer_block() {
        // A [Peer] block missing PublicKey or AllowedIPs is dropped, not left half-built.
        let ini = "[Interface]\nPrivateKey = key=\nAddress = 10.44.0.1/16\n\n\
                   [Peer]\nPublicKey = peerA=\n";
        let config = WgConfig::parse(ini).unwrap();
        assert!(config.peers.is_empty());
    }

    #[test]
    fn upsert_peer_adds_new_then_replaces_existing() {
        let mut config = WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: None,
            peers: vec![],
        };
        config.upsert_peer(WgPeer {
            public_key: "peerA=".into(),
            allowed_ips: "10.44.0.2/32".into(),
            endpoint: None,
            persistent_keepalive: None,
        });
        assert_eq!(config.peers.len(), 1);

        config.upsert_peer(WgPeer {
            public_key: "peerA=".into(),
            allowed_ips: "10.44.0.2/32,10.42.1.0/24".into(),
            endpoint: Some("203.0.113.1:51820".into()),
            persistent_keepalive: Some(25),
        });
        assert_eq!(
            config.peers.len(),
            1,
            "same key updates in place, not appends"
        );
        assert_eq!(config.peers[0].allowed_ips, "10.44.0.2/32,10.42.1.0/24");
    }

    #[test]
    fn remove_peer_by_key_removes_matching_and_reports_absence() {
        let mut config = WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: None,
            peers: vec![WgPeer {
                public_key: "peerA=".into(),
                allowed_ips: "10.44.0.2/32".into(),
                endpoint: None,
                persistent_keepalive: None,
            }],
        };
        assert!(config.remove_peer_by_key("peerA="));
        assert!(config.peers.is_empty());
        assert!(!config.remove_peer_by_key("peerA="), "already gone");
    }

    /// `remove_peer_durable` on a config file that was never created (matching `remove_peer`'s
    /// documented idempotency: "removing an absent peer succeeds") must treat that as nothing to
    /// persist and still attempt the live removal, not error out of the read before ever trying.
    #[test]
    fn remove_peer_durable_treats_missing_config_as_nothing_to_persist() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("spur0.conf"); // never written
        let err = format!(
            "{:#}",
            remove_peer_durable("spur0", &config_path, "peerA=").unwrap_err()
        );
        // The `wg` binary is unavailable in this test environment, so the live half fails — but the
        // failure must come from THAT step, proving the missing-file persist step was skipped rather
        // than erroring on `read_from`.
        assert!(
            err.contains("wg set peer remove") || err.contains("failed to run"),
            "expected only the live wg step to fail, got: {err}"
        );
    }

    /// Same idempotency guarantee, but for a `--config-dir` that was never created at all (not just
    /// a missing `.conf` file inside an existing dir) — the realistic shape of "never ran `spur net
    /// init` here".
    #[test]
    fn remove_peer_durable_treats_missing_config_dir_as_nothing_to_persist() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("never-created-subdir").join("spur0.conf");
        let err = format!(
            "{:#}",
            remove_peer_durable("spur0", &config_path, "peerA=").unwrap_err()
        );
        // The persist half must fully succeed (no lock/read/directory error); the only failure
        // allowed here is `remove_peer`'s live `wg` call, which errors because the `wg` binary
        // isn't available in this test environment — not because persistence choked on a missing dir.
        assert!(
            err.contains("wg set peer remove") || err.contains("failed to run"),
            "expected only the live wg step to fail, got: {err}"
        );
        assert!(dir.path().join("never-created-subdir").is_dir());
    }

    /// `add_peer_durable`/`remove_peer_durable`/`apply_mesh_durable` all run their live `wg` call
    /// INSIDE the `with_config_lock` closure now (not after it returns), so two concurrent CLI
    /// invocations can't have their live and persisted state land in different orders. This proves
    /// the underlying primitive those callers depend on: two threads racing for the same lock path
    /// never interleave their critical sections.
    #[test]
    fn with_config_lock_serializes_concurrent_critical_sections() {
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur0.conf");
        let events: Arc<Mutex<Vec<(u32, &'static str)>>> = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..4)
            .map(|id| {
                let path = path.clone();
                let events = events.clone();
                std::thread::spawn(move || {
                    with_config_lock(&path, || {
                        events.lock().unwrap().push((id, "enter"));
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        events.lock().unwrap().push((id, "exit"));
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let events = events.lock().unwrap();
        let mut open: Option<u32> = None;
        for (id, kind) in events.iter() {
            match *kind {
                "enter" => {
                    assert!(
                        open.is_none(),
                        "thread {id} entered while {open:?} was still inside"
                    );
                    open = Some(*id);
                }
                "exit" => assert_eq!(open, Some(*id)),
                _ => unreachable!(),
            }
            if *kind == "exit" {
                open = None;
            }
        }
    }

    #[test]
    fn read_from_round_trips_write_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur0.conf");
        let config = WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: Some(51820),
            peers: vec![WgPeer {
                public_key: "peerA=".into(),
                allowed_ips: "10.44.0.2/32".into(),
                endpoint: Some("203.0.113.1:51820".into()),
                persistent_keepalive: Some(25),
            }],
        };
        config.write_to(&path).unwrap();
        let read_back = WgConfig::read_from(&path).unwrap();
        assert_eq!(read_back.peers.len(), 1);
        assert_eq!(read_back.peers[0].public_key, "peerA=");
    }

    #[test]
    fn write_to_overwrites_atomically_with_no_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur0.conf");
        let base = WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: None,
            peers: vec![],
        };
        base.write_to(&path).unwrap();
        let mut updated = base;
        updated.upsert_peer(WgPeer {
            public_key: "peerA=".into(),
            allowed_ips: "10.44.0.2/32".into(),
            endpoint: None,
            persistent_keepalive: None,
        });
        updated.write_to(&path).unwrap();

        assert_eq!(WgConfig::read_from(&path).unwrap().peers.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "no temp file should survive a successful write"
        );
    }

    /// The actual regression this fixes: a peer added on the running interface (`wg set`, e.g. via
    /// `spur net add-peer`) must also land in the config file, so an interface reload (`wg-quick`
    /// restart, host reboot) rebuilds it from disk instead of silently dropping it. Exercises the
    /// same read-modify-write path `add_peer_durable`/`remove_peer_durable` use, without requiring
    /// the `wg` binary the live half of those functions shells out to.
    #[test]
    fn added_peer_persists_across_a_simulated_config_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur0.conf");
        WgConfig {
            private_key: "key=".into(),
            address: "10.44.0.1/16".into(),
            listen_port: Some(51820),
            peers: vec![],
        }
        .write_to(&path)
        .unwrap();

        let mut config = WgConfig::read_from(&path).unwrap();
        config.upsert_peer(WgPeer {
            public_key: "peerWorker=".into(),
            allowed_ips: "10.44.0.2/32".into(),
            endpoint: Some("203.0.113.9:51820".into()),
            persistent_keepalive: Some(25),
        });
        config.write_to(&path).unwrap();

        // Simulate the interface reload that a reboot or `wg-quick` restart triggers: reload
        // strictly from the persisted file, with no memory of the live `wg set` call.
        let reloaded = WgConfig::read_from(&path).unwrap();
        assert_eq!(reloaded.peers.len(), 1, "peer must survive a config reload");
        assert_eq!(reloaded.peers[0].public_key, "peerWorker=");
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

    // Canned `wg show <iface> endpoints` stdout modeled on real testbed output
    // (tab-separated `<pubkey>\t<endpoint>` per line), with keys/addresses replaced by
    // placeholders and the documentation endpoint range (203.0.113.0/24, TEST-NET-3).
    #[test]
    fn test_parse_peer_endpoints_happy_path() {
        let stdout = "peerAAA=\t203.0.113.1:51820\n\
                      peerBBB=\t203.0.113.2:51820\n\
                      peerCCC=\t203.0.113.3:51820\n";
        let map = parse_peer_endpoints(stdout);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get("peerAAA=").map(String::as_str),
            Some("203.0.113.1:51820")
        );
        assert_eq!(
            map.get("peerCCC=").map(String::as_str),
            Some("203.0.113.3:51820")
        );
    }

    #[test]
    fn test_parse_peer_endpoints_skips_none_and_malformed() {
        // A peer with no established endpoint reports `(none)`; blank lines and lines
        // without a tab separator must be ignored, not panic or produce empty entries.
        let stdout = "peerAAA=\t203.0.113.1:51820\n\
                      peerBBB=\t(none)\n\
                      \n\
                      malformed-no-tab\n\
                      peerCCC=\t\n";
        let map = parse_peer_endpoints(stdout);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("peerAAA=").map(String::as_str),
            Some("203.0.113.1:51820")
        );
        assert!(!map.contains_key("peerBBB="));
        assert!(!map.contains_key("peerCCC="));
    }

    #[test]
    fn test_parse_peer_endpoints_empty() {
        assert!(parse_peer_endpoints("").is_empty());
    }
}
