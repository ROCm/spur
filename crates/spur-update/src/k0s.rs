// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Download + install a pinned k0s release binary from GitHub so `spur k8s up` is self-contained
//! on bare-metal nodes.
//!
//! k0s ships one raw binary per arch (`k0s-<version>-<arch>`, e.g. `k0s-v1.36.2+k0s.0-amd64`) plus a
//! combined `sha256sums.txt` (lines are `<hex>  <name>` or `<hex> *<name>` in binary mode). This
//! fetches the binary for the host arch, verifies its SHA-256 against that manifest, and installs
//! it atomically (write temp -> chmod 0755 -> rename over dest). Version-agnostic: the caller
//! supplies the tag; the pin (`spur_core::k0s::K0S_PINNED_VERSION`) lives in spur-core.
//!
//! Only SHA-256 is verified (matching the rest of `spur-update`); the `.sig` cosign signatures k0s
//! also publishes are not checked.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// GitHub repo k0s releases come from.
const K0S_REPO: &str = "k0sproject/k0s";
const GITHUB_API: &str = "https://api.github.com";
/// Generous ceiling for the ~260 MB binary download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// k0s asset arch suffix for the host (`amd64`/`arm64`).
fn host_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => bail!("no k0s build for host arch {other} (supported: x86_64, aarch64)"),
    }
}

/// The k0s asset filename for `version` on the host arch, e.g. `k0s-v1.36.2+k0s.0-amd64`.
pub fn asset_name(version: &str) -> Result<String> {
    Ok(format!("k0s-{version}-{}", host_arch()?))
}

fn http_client(user_agent: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

/// Resolve `"latest"` to the newest k0s release tag via the GitHub API.
pub async fn resolve_latest_version() -> Result<String> {
    let url = format!("{GITHUB_API}/repos/{K0S_REPO}/releases/latest");
    let resp = http_client("spur")?
        .get(&url)
        .send()
        .await
        .context("GitHub API request for the latest k0s release failed")?
        .error_for_status()
        .context("GitHub API returned an error for the latest k0s release")?;
    let json: serde_json::Value = resp.json().await.context("parse GitHub release JSON")?;
    let tag = json
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("latest k0s release JSON has no tag_name")?;
    if tag.is_empty() {
        bail!("GitHub returned an empty latest k0s tag");
    }
    Ok(tag.to_string())
}

/// Summary of a completed install.
#[derive(Debug, Clone)]
pub struct InstalledK0s {
    pub version: String,
    pub path: PathBuf,
    pub sha256: String,
}

/// The SHA-256 hex of `asset` from a `sha256sums.txt` body. Lines are `<hex>  <name>` or, in
/// sha256sum binary mode, `<hex> *<name>`.
fn expected_hash(sums: &str, asset: &str) -> Result<String> {
    for line in sums.lines() {
        let mut it = line.split_whitespace();
        let (Some(hash), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            return Ok(hash.to_lowercase());
        }
    }
    bail!("no sha256 entry for {asset} in sha256sums.txt");
}

fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Download k0s `version` for the host arch, verify its SHA-256, and install it to `dest`
/// atomically (temp file -> `chmod 0755` -> rename over `dest`). `version` may be `"latest"`.
pub async fn install_k0s(version: &str, dest: &Path) -> Result<InstalledK0s> {
    let version = if version == "latest" {
        resolve_latest_version().await?
    } else {
        version.to_string()
    };
    let asset = asset_name(&version)?;
    let base = format!("https://github.com/{K0S_REPO}/releases/download/{version}");
    let client = http_client("spur-update")?;

    // Checksum manifest first, so a bad/nonexistent version fails before the big download.
    let sums = client
        .get(format!("{base}/sha256sums.txt"))
        .send()
        .await
        .context("download k0s sha256sums.txt")?
        .error_for_status()
        .with_context(|| format!("sha256sums.txt not found for k0s {version}"))?
        .text()
        .await
        .context("read k0s sha256sums.txt")?;
    let want = expected_hash(&sums, &asset)?;

    let bytes = client
        .get(format!("{base}/{asset}"))
        .send()
        .await
        .with_context(|| format!("download {asset}"))?
        .error_for_status()
        .with_context(|| format!("{asset} not found (unknown k0s version {version}?)"))?
        .bytes()
        .await
        .context("read k0s binary body")?;
    let got = sha256_hex(&bytes);
    if got != want {
        bail!("k0s {asset} sha256 mismatch: expected {want}, got {got}");
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    // Temp sibling on the same filesystem so the final rename is atomic.
    let tmp = dest.with_extension("k0s-download");
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .await
            .context("chmod k0s 0755")?;
    }
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("install k0s to {}", dest.display()))?;

    Ok(InstalledK0s {
        version,
        path: dest.to_path_buf(),
        sha256: got,
    })
}

/// Ensure a k0s binary exists at `dest`; install `version` if it is missing. Returns `Some(info)`
/// when it installed, `None` when k0s was already present (no network use).
pub async fn ensure_k0s(version: &str, dest: &Path) -> Result<Option<InstalledK0s>> {
    if dest.exists() {
        return Ok(None);
    }
    Ok(Some(install_k0s(version, dest).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_is_k0s_version_arch() {
        let n = asset_name("v1.36.2+k0s.0").unwrap();
        assert!(n.starts_with("k0s-v1.36.2+k0s.0-"), "got {n}");
        assert!(n.ends_with("amd64") || n.ends_with("arm64"), "got {n}");
    }

    #[test]
    fn expected_hash_binary_mode() {
        let sums = "8b5d985f *k0s-v1.36.2+k0s.0-amd64\nb00b425a *k0s-v1.36.2+k0s.0-arm64\n";
        assert_eq!(
            expected_hash(sums, "k0s-v1.36.2+k0s.0-amd64").unwrap(),
            "8b5d985f"
        );
        assert_eq!(
            expected_hash(sums, "k0s-v1.36.2+k0s.0-arm64").unwrap(),
            "b00b425a"
        );
    }

    #[test]
    fn expected_hash_two_space_mode_and_lowercases() {
        let sums = "ABC123  k0s-v1-amd64\n";
        assert_eq!(expected_hash(sums, "k0s-v1-amd64").unwrap(), "abc123");
    }

    #[test]
    fn expected_hash_missing_asset_errors() {
        let sums = "abc  k0s-v1-amd64\n";
        assert!(expected_hash(sums, "k0s-v1-arm64").is_err());
    }
}
