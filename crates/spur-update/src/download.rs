use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::check::ReleaseInfo;

/// Download a release tarball and verify its SHA256 checksum.
/// Returns the path to the verified tarball file.
pub async fn download_and_verify(release: &ReleaseInfo, tmp_dir: &Path) -> Result<PathBuf> {
    let tarball_url = release
        .tarball_url
        .as_ref()
        .context("no tarball asset found in release")?;

    // Extract filename from URL
    let filename = tarball_url
        .rsplit('/')
        .next()
        .unwrap_or("spur-release.tar.gz");
    let tarball_path = tmp_dir.join(filename);

    info!(url = %tarball_url, "downloading release");

    // Download tarball
    let client = reqwest::Client::builder()
        .user_agent("spur-update")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = client
        .get(tarball_url)
        .send()
        .await
        .context("failed to download tarball")?
        .error_for_status()
        .context("tarball download HTTP error")?;

    let bytes = resp.bytes().await.context("failed to read tarball body")?;

    // Write to file
    let mut file = tokio::fs::File::create(&tarball_path).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    drop(file);

    debug!(
        path = %tarball_path.display(),
        size = bytes.len(),
        "tarball downloaded"
    );

    // Verify SHA256 if checksum URL is available
    if let Some(checksum_url) = &release.checksum_url {
        debug!(url = %checksum_url, "downloading checksum");
        let checksum_resp = client
            .get(checksum_url)
            .send()
            .await
            .context("failed to download checksum")?
            .error_for_status()
            .context("checksum download HTTP error")?
            .text()
            .await
            .context("failed to read checksum body")?;

        // Parse expected hash (format: "hash  filename" or just "hash")
        let expected_hash = checksum_resp
            .split_whitespace()
            .next()
            .context("empty checksum file")?
            .to_lowercase();

        // Compute actual hash
        let actual_hash = {
            let data = std::fs::read(&tarball_path)?;
            let digest = Sha256::digest(&data);
            format!("{:x}", digest)
        };

        if actual_hash != expected_hash {
            // Delete the bad tarball
            let _ = std::fs::remove_file(&tarball_path);
            bail!(
                "SHA256 checksum mismatch: expected {}, got {}",
                expected_hash,
                actual_hash
            );
        }

        info!("SHA256 checksum verified");
    } else {
        debug!("no checksum asset — skipping verification");
    }

    Ok(tarball_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_verification_logic() {
        let data = b"test data for hashing";
        let digest = Sha256::digest(data);
        let hex = format!("{:x}", digest);
        // Just verify the hash is 64 hex chars
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
