// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bootstrapping ArgoCD, the first stage of the cluster-forge install.
//!
//! ArgoCD comes first because everything after it is delivered as an ArgoCD Application. It is
//! installed here from the chart cluster-forge vendors, and then adopts itself: the parent
//! Application created at the end of the install manages this same release from then on.

use std::path::Path;

use anyhow::Result;
use serde_yaml::Value;

use super::{helm, kube, renderer::Renderer, values};

pub const NAMESPACE: &str = "argocd";

const APP: &str = "argocd";

/// The workloads that must be up before anything is handed to ArgoCD to reconcile.
const WORKLOADS: &[(&str, &str)] = &[
    ("statefulset", "argocd-application-controller"),
    ("deploy", "argocd-applicationset-controller"),
    ("deploy", "argocd-redis"),
    ("deploy", "argocd-repo-server"),
];

/// The chart cluster-forge pins for ArgoCD. Resolved before the install runs, because the renderer
/// takes its image from this same chart.
pub fn chart_dir(checkout: &Path, root: &Value) -> Result<std::path::PathBuf> {
    let version = values::app_chart_version(root, APP)?;
    Ok(checkout.join("sources").join(APP).join(version))
}

/// Install ArgoCD and wait for it to serve.
pub async fn install(
    renderer: &Renderer,
    chart: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    domain: &str,
) -> Result<()> {
    if installed().await {
        eprintln!("ArgoCD is already installed");
        return Ok(());
    }
    let assembled = assemble_values(root, size_overlay, domain);

    eprintln!("Installing ArgoCD from {} ...", chart.display());
    let chart_path = renderer.upload_chart(chart, APP).await?;
    let values_path = renderer
        .upload_values("argocd-bootstrap-values.yaml", &assembled)
        .await?;
    let manifest = renderer
        .render(
            &helm::Template::new(APP, chart_path)
                .namespace(NAMESPACE)
                .values(values_path),
        )
        .await?;
    kube::apply_server_side(&manifest, "the ArgoCD manifests").await?;

    for (kind, name) in WORKLOADS {
        kube::wait_rollout(kind, name, NAMESPACE).await?;
    }
    eprintln!("ArgoCD is ready");
    Ok(())
}

/// The application controller is the last of ArgoCD's parts to come up, so its presence is what
/// says this stage finished rather than died half way.
async fn installed() -> bool {
    kube::exists(&[
        "get",
        "statefulset",
        "argocd-application-controller",
        "-n",
        NAMESPACE,
    ])
    .await
}

/// Build ArgoCD's bootstrap values: the domain it serves on, overlaid with the values cluster-forge
/// pins for it, then with the size-specific overlay.
///
/// The domain goes in first and cluster-forge's own `global` block goes over it. cluster-forge
/// ships `.apps.argocd.valuesObject.global.domain` as an explicit null, marked "to be filled by
/// cluster-forge app", so the bootstrap domain is cleared again — deliberately, because the parent
/// Application sets the real one when ArgoCD adopts itself.
fn assemble_values(root: &Value, size_overlay: Option<&Value>, domain: &str) -> Value {
    let bootstrap = serde_yaml::from_str(&format!("global:\n  domain: argocd.{domain}\n"))
        .unwrap_or(Value::Null);
    let mut assembled = match values::app_values(root, APP) {
        Some(pinned) => values::merge(bootstrap, pinned),
        None => bootstrap,
    };
    if let Some(overlay) = size_overlay.and_then(|doc| values::app_values(doc, APP)) {
        assembled = values::merge(assembled, overlay);
    }
    assembled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).expect("valid yaml")
    }

    /// The shape cluster-forge actually ships: an `.apps.argocd` with a versioned path, a
    /// `valuesObject` that re-declares `global.domain` as null, and a size overlay that trims
    /// replicas.
    fn root_values() -> Value {
        yaml("apps:\n  argocd:\n    path: argocd/8.3.5\n    valuesObject:\n      global:\n        domain: null\n      controller:\n        replicas: 2\n      server:\n        replicas: 2\n")
    }

    #[test]
    fn the_chart_comes_from_the_version_cluster_forge_pins() {
        assert_eq!(
            chart_dir(Path::new("/srv/cluster-forge"), &root_values()).expect("a chart"),
            Path::new("/srv/cluster-forge/sources/argocd/8.3.5")
        );
    }

    #[test]
    fn the_size_overlay_wins_over_the_pinned_values() {
        let overlay =
            yaml("apps:\n  argocd:\n    valuesObject:\n      controller:\n        replicas: 1\n");
        let assembled = assemble_values(&root_values(), Some(&overlay), "example.com");
        assert_eq!(
            values::at(&assembled, &["controller", "replicas"]).and_then(Value::as_u64),
            Some(1)
        );
        // A key the overlay does not mention survives.
        assert_eq!(
            values::at(&assembled, &["server", "replicas"]).and_then(Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn cluster_forge_clears_the_bootstrap_domain_for_its_own_app_to_set() {
        let assembled = assemble_values(&root_values(), None, "example.com");
        assert!(values::at(&assembled, &["global", "domain"])
            .expect("global.domain present")
            .is_null());
    }

    #[test]
    fn the_bootstrap_domain_survives_when_cluster_forge_does_not_restate_it() {
        // Guards the merge, not cluster-forge: if the null were ever dropped upstream, the domain
        // computed here has to reach the chart rather than vanish.
        let root = yaml("apps:\n  argocd:\n    path: argocd/8.3.5\n    valuesObject:\n      server:\n        replicas: 2\n");
        let assembled = assemble_values(&root, None, "example.com");
        assert_eq!(
            values::at(&assembled, &["global", "domain"]).and_then(Value::as_str),
            Some("argocd.example.com")
        );
    }

    #[test]
    fn waits_for_the_application_controller_it_probes_for() {
        // The resume probe and the readiness wait must name the same workload, or a resumed run
        // skips a stage that never finished.
        assert!(WORKLOADS.contains(&("statefulset", "argocd-application-controller")));
    }
}
