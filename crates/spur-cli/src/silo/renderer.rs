// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Rendering cluster-forge's charts inside the cluster, so the node needs no Helm of its own.
//!
//! The renderer is a short-lived pod running ArgoCD's own image. That image is chosen for three
//! reasons, in order of weight:
//!
//! 1. It carries the same Helm that ArgoCD renders with. Once the parent Application adopts ArgoCD,
//!    OpenBao and Gitea, ArgoCD reconciles those same charts forever after using its bundled Helm.
//!    Rendering the bootstrap with any other Helm means each component is rendered twice by two
//!    different versions, and the two can disagree.
//! 2. The cluster pulls it anyway, for ArgoCD itself, so the bootstrap adds no image dependency.
//! 3. It comes from the chart's own `appVersion`, so there is no second version to keep in step.
//!
//! The image has no `kubectl`, which suits the split here: the pod renders and nothing else, and
//! the node applies the result. The pod gets no ServiceAccount token and no cluster rights.

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::helm::Template;

/// Where the pod keeps the chart it is asked to render. `/home/argocd` is the image's WORKDIR and
/// is group-writable for the non-root user it runs as.
const POD_WORK_DIR: &str = "/home/argocd/render";

/// How long the pod waits to be told what to render before it gives up. Long enough for a slow
/// image pull plus every render, short enough that a SPUR process killed mid-install leaves nothing
/// running for an hour.
const POD_IDLE_SECONDS: u32 = 1800;

const READY_TIMEOUT: &str = "300s";

pub struct Renderer {
    pod: String,
    namespace: String,
}

impl Renderer {
    /// Start the renderer and wait for it to be ready to accept a chart.
    pub async fn start(image: &str, namespace: &str) -> Result<Self> {
        let pod = "spur-silo-renderer".to_string();
        // A leftover pod from a killed run holds the old image, so replace it rather than adopt it.
        let _ = super::kube::delete_ignore_missing(&["pod", &pod, "-n", namespace]).await;

        eprintln!("Starting the chart renderer on {image} ...");
        let manifest = pod_manifest(&pod, namespace, image)?;
        super::kube::apply(manifest.as_bytes(), "the renderer pod").await?;
        super::kube::wait_condition("Ready", &format!("pod/{pod}"), namespace, READY_TIMEOUT)
            .await
            .context(
                "the chart renderer never became ready — check that the node can pull ArgoCD's \
                 image and that a pod can be scheduled",
            )?;
        Ok(Self {
            pod,
            namespace: namespace.to_string(),
        })
    }

    /// Copy a chart directory into the pod under `name` and return the path it landed at.
    ///
    /// The caller names the destination because cluster-forge keeps every chart under a version
    /// directory: `openbao-config/0.1.0` and `openbao-init-job/0.1.0` are different charts with the
    /// same directory name, and uploading both under it would leave one overwriting the other.
    ///
    /// The tree goes in as a tar stream on stdin rather than through `kubectl cp`, so the transfer
    /// is one process with an exit code SPUR can act on.
    pub async fn upload_chart(&self, chart: &Path, name: &str) -> Result<String> {
        let mut archive = tar::Builder::new(Vec::new());
        archive
            .append_dir_all(name, chart)
            .with_context(|| format!("could not archive {}", chart.display()))?;
        let tarball = archive
            .into_inner()
            .context("could not finish the chart archive")?;

        self.exec_stdin(
            &[
                "sh",
                "-c",
                &format!("mkdir -p {POD_WORK_DIR} && tar xf - -C {POD_WORK_DIR}"),
            ],
            &tarball,
            "the chart",
        )
        .await?;
        Ok(format!("{POD_WORK_DIR}/{name}"))
    }

    /// Write an assembled values document into the pod and return the path it landed at.
    pub async fn upload_values(&self, name: &str, values: &serde_yaml::Value) -> Result<String> {
        let body = serde_yaml::to_string(values).context("could not serialise the values")?;
        let path = format!("{POD_WORK_DIR}/{name}");
        self.exec_stdin(
            &[
                "sh",
                "-c",
                &format!("mkdir -p {POD_WORK_DIR} && cat > {path}"),
            ],
            body.as_bytes(),
            "the values",
        )
        .await?;
        Ok(path)
    }

    /// Render a chart and return the manifest stream.
    pub async fn render(&self, template: &Template) -> Result<Vec<u8>> {
        let mut args = vec!["helm".to_string()];
        args.extend(template.args());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();

        let out = self.exec(&argv).await?;
        if !out.status.success() {
            bail!(
                "could not render {}: {}",
                template.chart_path(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if out.stdout.is_empty() {
            bail!("rendering {} produced no manifests", template.chart_path());
        }
        Ok(out.stdout)
    }

    /// Remove the pod. Called on the way out of an install, successful or not.
    pub async fn stop(&self) {
        let _ =
            super::kube::delete_ignore_missing(&["pod", &self.pod, "-n", &self.namespace]).await;
    }

    async fn exec(&self, argv: &[&str]) -> Result<std::process::Output> {
        let mut cmd = crate::silo::kube::kubectl();
        cmd.args(["exec", &self.pod, "-n", &self.namespace, "--"]);
        cmd.args(argv);
        cmd.output()
            .await
            .context("could not run a command in the chart renderer")
    }

    async fn exec_stdin(&self, argv: &[&str], stdin: &[u8], what: &str) -> Result<()> {
        let mut args = vec!["exec", "-i", &self.pod, "-n", &self.namespace, "--"];
        args.extend_from_slice(argv);
        let out = super::kube::run_with_stdin(&args, stdin)
            .await
            .with_context(|| format!("could not send {what} to the chart renderer"))?;
        if !out.status.success() {
            bail!(
                "could not send {what} to the chart renderer: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// The renderer pod: no ServiceAccount token, no privileges, and nothing to do but wait. It only
/// ever runs `helm template`, which reads local files and never contacts the API server.
///
/// Helm keeps its cache, config and data under `$HOME` when nothing else says otherwise, and the
/// image sets no `HOME`. The three `HELM_*` variables point it at the writable home instead, so a
/// render cannot fail on a directory it may not create.
fn pod_manifest(name: &str, namespace: &str, image: &str) -> Result<String> {
    let home = "/home/argocd";
    let pod = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "spur" },
        },
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "containers": [{
                "name": "renderer",
                "image": image,
                "command": ["sleep", POD_IDLE_SECONDS.to_string()],
                "env": [
                    { "name": "HOME", "value": home },
                    { "name": "HELM_CACHE_HOME", "value": format!("{home}/.cache/helm") },
                    { "name": "HELM_CONFIG_HOME", "value": format!("{home}/.config/helm") },
                    { "name": "HELM_DATA_HOME", "value": format!("{home}/.local/share/helm") },
                ],
                "resources": { "requests": { "cpu": "50m", "memory": "128Mi" } },
            }],
        },
    });
    serde_yaml::to_string(&pod).context("could not build the renderer pod manifest")
}

/// The image to render with, read out of the ArgoCD chart itself.
///
/// The chart's `appVersion` is the ArgoCD release it installs, and its `global.image.repository`
/// is where that release comes from. Taking both from the chart means the renderer holds the exact
/// Helm that this cluster's ArgoCD will reconcile with, with no second version to keep in step.
pub fn image_from_chart(chart: &Path) -> Result<String> {
    let meta = super::values::read_file(&chart.join("Chart.yaml"))?;
    let app_version = super::values::at(&meta, &["appVersion"])
        .and_then(serde_yaml::Value::as_str)
        .with_context(|| format!("{}/Chart.yaml names no appVersion", chart.display()))?;
    let chart_values = super::values::read_file(&chart.join("values.yaml"))?;
    Ok(image_for(&chart_values, app_version))
}

fn image_for(chart_values: &serde_yaml::Value, app_version: &str) -> String {
    let repository = super::values::at(chart_values, &["global", "image", "repository"])
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or("quay.io/argoproj/argocd");
    format!("{repository}:{app_version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).expect("valid yaml")
    }

    #[test]
    fn the_image_comes_from_the_chart_so_helm_matches_what_argocd_will_use() {
        let chart =
            yaml("global:\n  image:\n    repository: quay.io/argoproj/argocd\n    tag: \"\"\n");
        assert_eq!(
            image_for(&chart, "v3.1.4"),
            "quay.io/argoproj/argocd:v3.1.4"
        );
    }

    #[test]
    fn a_chart_that_names_no_repository_falls_back_to_upstream() {
        assert_eq!(
            image_for(&yaml("{}"), "v3.1.4"),
            "quay.io/argoproj/argocd:v3.1.4"
        );
    }

    fn manifest() -> serde_yaml::Value {
        let rendered = pod_manifest(
            "spur-silo-renderer",
            "argocd",
            "quay.io/argoproj/argocd:v3.1.4",
        )
        .expect("a manifest");
        serde_yaml::from_str(&rendered).expect("valid yaml")
    }

    #[test]
    fn the_pod_carries_no_service_account_token() {
        // The renderer only runs `helm template`, which never contacts the API server. Handing it
        // a token would give a chart-rendering sandbox cluster credentials for no reason.
        let pod = manifest();
        assert_eq!(
            super::super::values::at(&pod, &["spec", "automountServiceAccountToken"])
                .and_then(serde_yaml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            super::super::values::at(&pod, &["spec", "restartPolicy"])
                .and_then(serde_yaml::Value::as_str),
            Some("Never")
        );
    }

    #[test]
    fn the_pod_points_helm_at_a_home_it_can_write() {
        // The image sets no HOME, and Helm creates its cache and config under whatever HOME the
        // runtime supplies. An unwritable one fails every render.
        let pod = manifest();
        let env = super::super::values::at(&pod, &["spec", "containers"])
            .and_then(|c| c.as_sequence())
            .and_then(|c| c.first())
            .and_then(|c| c.get("env"))
            .and_then(|e| e.as_sequence())
            .expect("an env block");
        let named = |key: &str| {
            env.iter()
                .find(|e| e.get("name").and_then(serde_yaml::Value::as_str) == Some(key))
                .and_then(|e| e.get("value"))
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_string)
        };
        assert_eq!(named("HOME").as_deref(), Some("/home/argocd"));
        for key in ["HELM_CACHE_HOME", "HELM_CONFIG_HOME", "HELM_DATA_HOME"] {
            assert!(
                named(key).is_some_and(|v| v.starts_with("/home/argocd/")),
                "{key} must sit under the writable home"
            );
        }
    }

    #[test]
    fn the_command_overrides_the_images_entrypoint_with_a_plain_wait() {
        // The image entrypoint is tini, and a renderer that ran ArgoCD itself would need cluster
        // credentials. The pod must do nothing until it is told to render.
        let pod = manifest();
        let command = super::super::values::at(&pod, &["spec", "containers"])
            .and_then(|c| c.as_sequence())
            .and_then(|c| c.first())
            .and_then(|c| c.get("command"))
            .and_then(|c| c.as_sequence())
            .expect("a command");
        assert_eq!(
            command.first().and_then(serde_yaml::Value::as_str),
            Some("sleep")
        );
    }
}
