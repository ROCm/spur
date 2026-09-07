// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Installing the cluster-forge platform stack onto a running k0s cluster.
//!
//! cluster-forge delivers itself through ArgoCD, so the install is a bootstrap: put ArgoCD, OpenBao
//! and Gitea on the cluster by hand, then create the one Application that owns everything after
//! that, including the three bootstrapped components themselves.
//!
//! Every stage is idempotent and probes for its own result first, so an install that fails part way
//! resumes instead of repeating stages that take minutes.

pub mod argocd;
pub mod certs;
pub mod cnpg;
pub mod fetch;
pub mod gateway_node;
pub mod gitea;
pub mod helm;
pub mod kube;
pub mod kubeconfig;
pub mod metallb;
pub mod openbao;
pub mod parent;
pub mod preflight;
pub mod release;
pub mod renderer;
pub mod storage;
pub mod values;

use std::path::Path;

use anyhow::Result;
use serde_yaml::Value;

/// Namespaces the bootstrap creates before anything is applied into them.
const NAMESPACES: &[&str] = &[argocd::NAMESPACE, openbao::NAMESPACE, gitea::NAMESPACE];

pub struct Options<'a> {
    pub release: &'a release::Release,
    pub size: &'a str,
    pub domain: &'a str,
}

/// cluster-forge ships a `HelmChartConfig/rke2-coredns` in its gateway configuration. That kind
/// belongs to RKE2's helm-controller, which k0s does not have, so the whole application fails to
/// sync, no Gateway is ever created, and every HTTPRoute behind it stalls. Registering the kind
/// lets the object apply; nothing on k0s reconciles it, so it stays inert.
const HELM_CHART_CONFIG_CRD: &str = "\
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: helmchartconfigs.helm.cattle.io
spec:
  group: helm.cattle.io
  scope: Namespaced
  names:
    kind: HelmChartConfig
    plural: helmchartconfigs
    singular: helmchartconfig
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          x-kubernetes-preserve-unknown-fields: true
";

/// Register the kind before the deployer runs. ArgoCD caches the API resource list at startup, so a
/// CRD added afterwards stays invisible until its controller restarts.
pub async fn ensure_helm_chart_config_crd() -> Result<()> {
    if kube::exists(&["get", "crd", "helmchartconfigs.helm.cattle.io"]).await {
        return Ok(());
    }
    kube::apply_echoing(HELM_CHART_CONFIG_CRD.as_bytes(), "the HelmChartConfig kind").await
}

/// Bootstrap cluster-forge. `work_dir` is this command's private directory.
///
/// The node itself gets nothing installed on it. It needs a checkout, `k0s kubectl`, and a cluster
/// that can pull an image; Helm runs in the cluster, in a pod the install starts and then removes.
pub async fn install(o: &Options<'_>, work_dir: &Path) -> Result<()> {
    let release = o.release;
    let checkout = fetch::fetch(release, work_dir).await?;

    let root = values::read_file(&checkout.join("root").join("values.yaml"))?;
    let size_overlay = read_size_overlay(&checkout, o.size)?;
    let argocd_chart = argocd::chart_dir(&checkout, &root)?;

    for namespace in NAMESPACES {
        kube::ensure_namespace(namespace).await?;
    }

    let image = renderer::image_from_chart(&argocd_chart)?;
    let renderer = renderer::Renderer::start(&image, argocd::NAMESPACE).await?;
    let gitea = gitea::Options {
        domain: o.domain,
        size: o.size,
        revision: &release.revision,
    };
    let result = bootstrap(
        &renderer,
        &checkout,
        &argocd_chart,
        &root,
        size_overlay.as_ref(),
        &gitea,
    )
    .await;
    renderer.stop().await;
    result
}

/// The bootstrap stages, in the only order they work in.
///
/// ArgoCD comes first because everything after it is an ArgoCD Application. OpenBao comes next,
/// because the Gitea init job reads the OpenBao root token and pulls its own password from an
/// OpenBao path. Gitea comes third, and its init job fills the repository the parent Application
/// reads. The parent Application comes last, and hands the cluster to ArgoCD.
async fn bootstrap(
    renderer: &renderer::Renderer,
    checkout: &Path,
    argocd_chart: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    o: &gitea::Options<'_>,
) -> Result<()> {
    argocd::install(renderer, argocd_chart, root, size_overlay, o.domain).await?;
    openbao::install(renderer, checkout, root, size_overlay, o.domain).await?;
    gitea::install(renderer, checkout, root, size_overlay, o).await?;
    parent::install(renderer, checkout, size_overlay.is_some(), o).await
}

/// The per-size overlay, when cluster-forge ships one for this size. A size with no file is not an
/// error: the base values already describe a working stack.
fn read_size_overlay(checkout: &Path, size: &str) -> Result<Option<Value>> {
    let path = checkout.join("root").join(format!("values_{size}.yaml"));
    if !path.is_file() {
        eprintln!("cluster-forge ships no values_{size}.yaml, so the base values stand alone");
        return Ok(None);
    }
    Ok(Some(values::read_file(&path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bootstrapped_component_gets_its_namespace() {
        // Applying into a namespace that does not exist fails the whole stage, so the list has to
        // cover each component the bootstrap installs by hand.
        assert!(NAMESPACES.contains(&"argocd"));
        assert!(NAMESPACES.contains(&"cf-openbao"));
        assert!(NAMESPACES.contains(&"cf-gitea"));
    }
}
