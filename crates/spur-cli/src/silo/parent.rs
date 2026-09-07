// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The parent ArgoCD Application, the last thing the bootstrap creates.
//!
//! Everything up to here was done by hand. This Application takes over: it owns every cluster-forge
//! application, the three bootstrapped components included, so ArgoCD reconciles ArgoCD, OpenBao and
//! Gitea from now on. Creating it is what ends the bootstrap and starts the platform stack.
//!
//! Nothing waits on it. ArgoCD syncs on its own schedule, and the applications arrive over several
//! minutes.

use std::path::Path;

use anyhow::Result;

use super::{gitea, helm, kube, renderer::Renderer};

/// The chart directory that renders the Application, and the one template wanted out of it.
const CHART: &str = "root";
const TEMPLATE: &str = "templates/cluster-forge.yaml";

/// The two repositories the Gitea init job filled, at their in-cluster addresses.
const GITEA_FORGE_REPO: &str = "http://gitea-http.cf-gitea.svc:3000/cluster-org/cluster-forge.git";
const GITEA_VALUES_REPO: &str =
    "http://gitea-http.cf-gitea.svc:3000/cluster-org/cluster-values.git";

/// The size that reads its sources straight from upstream instead of from Gitea.
const DIRECT_SIZE: &str = "small";

/// Where the parent Application reads cluster-forge from, and the cluster-values repository it
/// overlays, when there is one.
struct Sources {
    repo_url: &'static str,
    external_values: Option<&'static str>,
}

/// `small` points ArgoCD at upstream and overlays nothing, so it carries no cluster-values
/// repository and cannot disable an application. Every other size reads both repositories out of
/// Gitea, which is why the Gitea stage has to run first.
fn sources(size: &str) -> Sources {
    if size == DIRECT_SIZE {
        return Sources {
            repo_url: super::release::REPO,
            external_values: None,
        };
    }
    Sources {
        repo_url: GITEA_FORGE_REPO,
        external_values: Some(GITEA_VALUES_REPO),
    }
}

/// Create the parent Application and hand the cluster over to ArgoCD.
pub async fn install(
    renderer: &Renderer,
    checkout: &Path,
    has_size_overlay: bool,
    o: &gitea::Options<'_>,
) -> Result<()> {
    let sources = sources(o.size);
    let chart_path = renderer.upload_chart(&checkout.join(CHART), CHART).await?;

    let mut template = helm::Template::new("cluster-forge", &chart_path)
        .namespace(argocd_namespace())
        .show_only(TEMPLATE)
        .values(format!("{chart_path}/values.yaml"))
        .set("global.domain", o.domain)
        .set("global.clusterSize", &size_file(o.size))
        .set("clusterForge.targetRevision", o.revision)
        .set("clusterForge.repoUrl", sources.repo_url)
        // The root chart renders this Application only when told to, because the same chart is what
        // ArgoCD later renders to manage it.
        .set("bootstrap.renderSelfReference", "true");

    // The size overlay goes on as a second values file, after the chart's own.
    if has_size_overlay {
        template = template.values(format!("{chart_path}/{}", size_file(o.size)));
    }
    template = match sources.external_values {
        Some(repo) => template
            .set("externalValues.enabled", "true")
            .set("externalValues.repoUrl", repo),
        None => template.set("externalValues.enabled", "false"),
    };

    eprintln!("Handing the cluster to ArgoCD ...");
    let manifest = renderer.render(&template).await?;
    kube::apply_server_side(&manifest, "the cluster-forge Application").await?;
    eprintln!("ArgoCD owns the platform stack from now on");
    Ok(())
}

fn argocd_namespace() -> &'static str {
    super::argocd::NAMESPACE
}

fn size_file(size: &str) -> String {
    format!("values_{size}.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_but_small_reads_both_repositories_out_of_gitea() {
        for size in ["medium", "large"] {
            let s = sources(size);
            assert_eq!(s.repo_url, GITEA_FORGE_REPO);
            assert_eq!(s.external_values, Some(GITEA_VALUES_REPO));
        }
    }

    #[test]
    fn small_reads_upstream_and_overlays_nothing() {
        // The cluster-values overlay is what lets a size disable an application, so small cannot.
        let s = sources("small");
        assert_eq!(s.repo_url, super::super::release::REPO);
        assert_eq!(s.external_values, None);
    }

    #[test]
    fn the_gitea_addresses_are_in_cluster_service_names() {
        // The Application is reconciled by the ArgoCD repo-server inside the cluster, so an
        // address that only resolves outside it would leave every application unable to sync.
        for url in [GITEA_FORGE_REPO, GITEA_VALUES_REPO] {
            assert!(url.contains("gitea-http.cf-gitea.svc"));
            assert!(url.ends_with(".git"));
        }
    }
}
