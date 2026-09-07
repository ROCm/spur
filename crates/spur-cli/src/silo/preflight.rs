// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the install checks about the node before it starts.

use anyhow::Result;

use super::release::{Release, Source};

/// Name what looks wrong with the node, then carry on.
///
/// The install assumes the node is configured, so this changes nothing and stops nothing. It is
/// worth the few seconds because each of these fails late and as something else: an unraised
/// inotify limit as a kubelet that quietly stops tracking ConfigMap updates, a full disk as an
/// image pull that never finishes.
pub async fn report_unmet_prerequisites() {
    let missing = crate::prepare_node::unmet_prerequisites().await;
    if missing.is_empty() {
        return;
    }
    eprintln!("This node may not be ready for the platform stack:");
    for item in &missing {
        eprintln!("  - {item}");
    }
    eprintln!("The install continues, because it assumes the node is configured.");
}

/// Stop now for a binary the install cannot do without.
///
/// `prepare-node` does not install these, so they are not reported with the rest: they are the
/// operator's to put there. Checking up front matters because the clone happens minutes in, after
/// the cluster is up and the kubeconfig, StorageClasses and gateway certificate are all in place.
pub fn require_install_binaries(release: &Release) -> Result<()> {
    // The tarball source needs no git at all, so a node without it can still install.
    if !matches!(release.source, Source::Revision(_)) {
        return Ok(());
    }
    if crate::prepare_node::binary_on_path("git") {
        return Ok(());
    }
    anyhow::bail!(
        "`git` is not on PATH, and it is what clones cluster-forge {}. Install git, or pass \
         --release as a release archive URL, which downloads instead.",
        release.revision
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_url_needs_no_git() {
        // The archive downloads over HTTP, so a node without git can still install from one. The
        // revision case is not asserted here: this test host has git, and the answer would be the
        // environment's rather than the code's.
        let release = Release {
            source: Source::Tarball(
                "https://github.com/silogen/cluster-forge/releases/download/v2.2.2/release.tar.gz"
                    .into(),
            ),
            revision: "v2.2.2".into(),
        };
        assert!(require_install_binaries(&release).is_ok());
    }
}
