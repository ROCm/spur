// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Putting cluster-forge's sources on this node.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::release::{Release, Source, REPO};

/// The directory the sources land in, below the working directory this command owns.
const CHECKOUT_DIR: &str = "cluster-forge";

/// Fetch cluster-forge into `work_dir` and return the directory that holds `root/` and `sources/`.
///
/// A previous checkout is removed first: a clone pinned to another revision would otherwise be
/// reused silently, and a `--force` re-install would deploy whatever the last run left behind.
pub async fn fetch(release: &Release, work_dir: &Path) -> Result<PathBuf> {
    let checkout = work_dir.join(CHECKOUT_DIR);
    if checkout.exists() {
        std::fs::remove_dir_all(&checkout)
            .with_context(|| format!("could not clear {}", checkout.display()))?;
    }
    match &release.source {
        Source::Revision(revision) => clone(revision, &checkout).await?,
        Source::Tarball(url) => unpack(url, work_dir, &checkout).await?,
    }
    if !checkout.join("root").join("values.yaml").is_file() {
        bail!(
            "{} holds no root/values.yaml, so it is not a cluster-forge tree",
            checkout.display()
        );
    }
    Ok(checkout)
}

async fn clone(revision: &str, checkout: &Path) -> Result<()> {
    eprintln!("Cloning cluster-forge {revision} ...");
    let out = tokio::process::Command::new("git")
        .args(["clone", "--branch", revision, "--depth", "1", REPO])
        .arg(checkout)
        .output()
        .await
        .context("could not run git — install it, or pass --release as a release URL")?;
    if !out.status.success() {
        bail!(
            "could not clone cluster-forge {revision}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Unpack a release archive. The archive carries its own `cluster-forge/` top-level directory, so
/// it extracts into the working directory rather than into the checkout path.
async fn unpack(url: &str, work_dir: &Path, checkout: &Path) -> Result<()> {
    eprintln!("Downloading cluster-forge from {url} ...");
    let client = reqwest::Client::builder()
        .user_agent("spur")
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("could not build an HTTP client")?;
    let body = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} returned an error"))?
        .bytes()
        .await
        .with_context(|| format!("could not read the response from {url}"))?;

    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(body));
    tar::Archive::new(decoder)
        .unpack(work_dir)
        .with_context(|| format!("could not unpack the archive from {url}"))?;
    if !checkout.is_dir() {
        bail!("the archive from {url} holds no {} directory", CHECKOUT_DIR);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silo::release;

    /// Reaches github over the network, so it is ignored: `cargo test` stays offline and
    /// deterministic. Run it with `cargo test -p spur-cli -- --ignored` when this module changes,
    /// because nothing else proves the pinned release still has the tree the bootstrap reads.
    #[tokio::test]
    #[ignore]
    async fn clones_the_pinned_release_and_finds_the_charts_the_bootstrap_needs() {
        let work = tempfile::tempdir().expect("a temp dir");
        let pinned = release::resolve("").expect("the pinned release resolves");
        let checkout = fetch(&pinned, work.path())
            .await
            .expect("cluster-forge clones");

        let root = crate::silo::values::read_file(&checkout.join("root").join("values.yaml"))
            .expect("root values parse");
        let version =
            crate::silo::values::app_chart_version(&root, "argocd").expect("argocd is pinned");
        assert!(
            checkout
                .join("sources")
                .join("argocd")
                .join(&version)
                .is_dir(),
            "cluster-forge {} names argocd {version} but ships no chart there",
            pinned.revision
        );
        assert!(checkout.join("root").join("values_medium.yaml").is_file());
    }

    /// A second fetch must not reuse whatever the first left behind, or a re-install would deploy
    /// the previous revision without saying so.
    #[tokio::test]
    #[ignore]
    async fn a_second_fetch_replaces_the_previous_checkout() {
        let work = tempfile::tempdir().expect("a temp dir");
        let pinned = release::resolve("").expect("resolves");
        let checkout = fetch(&pinned, work.path()).await.expect("clones");
        let marker = checkout.join("SPUR_STALE_MARKER");
        std::fs::write(&marker, b"stale").expect("marker written");

        fetch(&pinned, work.path()).await.expect("clones again");
        assert!(!marker.exists(), "the stale checkout survived the re-fetch");
    }
}
