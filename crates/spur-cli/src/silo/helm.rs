// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Describing a `helm template` invocation.
//!
//! Nothing here runs Helm. The renderer runs it inside the cluster, so every path in a `Template`
//! is a path in the renderer pod rather than on the node.

use std::path::PathBuf;

/// The Kubernetes version the charts render against.
///
/// The deployer this replaces never set one, so every chart rendered at Helm's own default of
/// 1.33 even on a newer cluster. Keeping that exact value keeps a chart's `.Capabilities` guards
/// resolving the way the released platform stack was built and tested against. Raising it is a
/// deliberate change, not a tidy-up.
pub const KUBE_VERSION: &str = "1.33";

/// A `helm template` invocation. Built up so the argument vector can be asserted in a test —
/// a wrong `--set` renders a chart that applies cleanly and behaves differently.
pub struct Template {
    release_name: String,
    chart: PathBuf,
    namespace: Option<String>,
    values: Vec<PathBuf>,
    sets: Vec<(String, String)>,
    show_only: Option<String>,
}

impl Template {
    pub fn new(release_name: &str, chart: impl Into<PathBuf>) -> Self {
        Self {
            release_name: release_name.to_string(),
            chart: chart.into(),
            namespace: None,
            values: Vec::new(),
            sets: Vec::new(),
            show_only: None,
        }
    }

    pub fn namespace(mut self, namespace: &str) -> Self {
        self.namespace = Some(namespace.to_string());
        self
    }

    pub fn values(mut self, path: impl Into<PathBuf>) -> Self {
        self.values.push(path.into());
        self
    }

    pub fn set(mut self, key: &str, value: &str) -> Self {
        self.sets.push((key.to_string(), value.to_string()));
        self
    }

    pub fn show_only(mut self, template: &str) -> Self {
        self.show_only = Some(template.to_string());
        self
    }

    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "template".to_string(),
            "--release-name".to_string(),
            self.release_name.clone(),
            self.chart.display().to_string(),
        ];
        if let Some(namespace) = &self.namespace {
            args.push("--namespace".to_string());
            args.push(namespace.clone());
        }
        if let Some(template) = &self.show_only {
            args.push("--show-only".to_string());
            args.push(template.clone());
        }
        for path in &self.values {
            args.push("--values".to_string());
            args.push(path.display().to_string());
        }
        for (key, value) in &self.sets {
            args.push("--set".to_string());
            args.push(format!("{key}={value}"));
        }
        // A chart's `templates/tests/` holds Pods that Helm only runs on `helm test`, and
        // `helm template` renders them like any other resource. Applying one creates a Pod that
        // does nothing, and a Pod is immutable, so the next run cannot apply over it and the whole
        // stage fails. Both the OpenBao and the Gitea chart ship one.
        args.push("--skip-tests".to_string());
        args.push(format!("--kube-version={KUBE_VERSION}"));
        args
    }

    /// The chart this renders, for an error message that names it.
    pub fn chart_path(&self) -> String {
        self.chart.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_chart_into_a_namespace_with_values() {
        let args = Template::new("argocd", "/srv/cluster-forge/sources/argocd/8.3.5")
            .namespace("argocd")
            .values("/tmp/argocd.yaml")
            .args();
        assert_eq!(
            args,
            vec![
                "template",
                "--release-name",
                "argocd",
                "/srv/cluster-forge/sources/argocd/8.3.5",
                "--namespace",
                "argocd",
                "--values",
                "/tmp/argocd.yaml",
                "--skip-tests",
                "--kube-version=1.33",
            ]
        );
    }

    #[test]
    fn a_set_becomes_one_key_equals_value_argument() {
        let args = Template::new("openbao", "/chart")
            .set("ui.enabled", "true")
            .args();
        assert!(args.windows(2).any(|w| w == ["--set", "ui.enabled=true"]));
    }

    #[test]
    fn show_only_selects_a_single_template() {
        let args = Template::new("cluster-forge", "/chart")
            .show_only("templates/cluster-forge.yaml")
            .args();
        assert!(args
            .windows(2)
            .any(|w| w == ["--show-only", "templates/cluster-forge.yaml"]));
    }

    #[test]
    fn every_render_skips_the_charts_test_hooks() {
        // A rendered test Pod gets applied like any other resource, and a Pod is immutable, so it
        // blocks every later run of the stage that created it.
        assert!(Template::new("x", "/chart")
            .args()
            .contains(&"--skip-tests".to_string()));
    }

    #[test]
    fn every_render_pins_the_kube_version() {
        // A chart that resolves .Capabilities differently renders different manifests, so this is
        // not cosmetic.
        assert!(Template::new("x", "/chart")
            .args()
            .contains(&"--kube-version=1.33".to_string()));
    }
}
