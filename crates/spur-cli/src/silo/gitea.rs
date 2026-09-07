// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bootstrapping Gitea, the last stage the install performs by hand.
//!
//! Gitea holds the repository every later ArgoCD Application reads from, cluster-forge's own
//! `clusterForge.repoUrl` included. That makes it the one component ArgoCD cannot deliver: it would
//! have to clone from Gitea to learn how to install Gitea. So this stage installs it, and its init
//! job then mirrors cluster-forge into it and writes the values the parent Application reads.
//!
//! The init job needs OpenBao, so this stage runs after it: the job reads the OpenBao root token,
//! waits on the OpenBao service, and pulls its own user password from an OpenBao path.

use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml::Value;

use super::{helm, kube, renderer::Renderer, values};

pub const NAMESPACE: &str = "cf-gitea";

const APP: &str = "gitea";

const INIT_JOB_CHART: &str = "gitea-init-job";
const INIT_JOB_VERSION: &str = "0.1.0";
const INIT_JOB: &str = "gitea-init-job";
const INIT_JOB_TIMEOUT: &str = "600s";

/// The Secret the Gitea chart reads to create its admin user, and the user it creates.
const ADMIN_SECRET: &str = "gitea-admin-credentials";
const ADMIN_USER: &str = "silogen-admin";

/// The ConfigMap the init job reads the assembled cluster-forge values out of. The key repeats the
/// name because that is the key the job asks for.
const VALUES_CONFIGMAP: &str = "initial-cf-values";

pub struct Options<'a> {
    pub domain: &'a str,
    pub size: &'a str,
    /// What ArgoCD records as the target revision, so the cluster reports the version it runs.
    pub revision: &'a str,
}

/// Install Gitea, then run the job that fills it.
pub async fn install(
    renderer: &Renderer,
    checkout: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    o: &Options<'_>,
) -> Result<()> {
    ensure_admin_credentials().await?;
    apply_cluster_values(root, size_overlay, o).await?;
    install_server(renderer, checkout, root, size_overlay, o.domain).await?;
    run_init_job(renderer, checkout, o).await
}

/// Put the admin password in place before the chart, which reads this Secret to create the user.
///
/// An existing Secret is left alone. Gitea creates the account once, from whatever password was
/// there at the time, so writing a new one later leaves the Secret disagreeing with the account it
/// is supposed to describe.
async fn ensure_admin_credentials() -> Result<()> {
    if kube::exists(&["get", "secret", ADMIN_SECRET, "-n", NAMESPACE]).await {
        eprintln!("Gitea already has admin credentials");
        return Ok(());
    }
    let password = random_hex(16)?;
    kube::apply_secret(
        ADMIN_SECRET,
        NAMESPACE,
        &[("username", ADMIN_USER), ("password", &password)],
    )
    .await?;
    eprintln!("Generated the Gitea admin password for {ADMIN_USER}");
    Ok(())
}

/// Hand the init job the values it writes into the cluster-values repository.
async fn apply_cluster_values(
    root: &Value,
    size_overlay: Option<&Value>,
    o: &Options<'_>,
) -> Result<()> {
    let document = cluster_values(root, size_overlay, o)?;
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": VALUES_CONFIGMAP, "namespace": NAMESPACE },
        "data": { VALUES_CONFIGMAP: document },
    });
    let body = serde_yaml::to_string(&manifest)
        .context("could not build the cluster-forge values ConfigMap")?;
    kube::apply(body.as_bytes(), VALUES_CONFIGMAP).await
}

/// The whole cluster-forge values document, with the three placeholders it ships filled in.
///
/// This is the entire file rather than one application's section: the init job commits it as the
/// cluster-values repository, and every later Application reads its own values out of it.
fn cluster_values(root: &Value, size_overlay: Option<&Value>, o: &Options<'_>) -> Result<String> {
    let mut doc = root.clone();
    values::set_at(
        &mut doc,
        &["global", "domain"],
        Value::String(o.domain.to_string()),
    );
    values::set_at(
        &mut doc,
        &["global", "clusterSize"],
        Value::String(size_file(o.size)),
    );
    values::set_at(
        &mut doc,
        &["clusterForge", "targetRevision"],
        Value::String(o.revision.to_string()),
    );
    // The overlay goes on after the placeholders, so a size that sets one of them wins.
    if let Some(overlay) = size_overlay {
        doc = values::merge(doc, overlay.clone());
    }
    serde_yaml::to_string(&doc).context("could not serialise the cluster-forge values")
}

async fn install_server(
    renderer: &Renderer,
    checkout: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    domain: &str,
) -> Result<()> {
    let version = values::app_chart_version(root, APP)?;
    let chart = checkout.join("sources").join(APP).join(version);
    eprintln!("Installing Gitea from {} ...", chart.display());

    let chart_path = renderer.upload_chart(&chart, APP).await?;
    let values_path = renderer
        .upload_values(
            "gitea-bootstrap-values.yaml",
            &values::assemble(root, size_overlay, APP),
        )
        .await?;
    let manifest = renderer
        .render(
            &helm::Template::new(APP, chart_path)
                .namespace(NAMESPACE)
                .values(values_path)
                .set("gitea.config.server.ROOT_URL", &root_url(domain)),
        )
        .await?;
    kube::apply_server_side(&manifest, "the Gitea manifests").await?;
    kube::wait_rollout("deploy", APP, NAMESPACE).await
}

/// Run the job that mirrors cluster-forge into Gitea and writes the cluster-values repository.
async fn run_init_job(renderer: &Renderer, checkout: &Path, o: &Options<'_>) -> Result<()> {
    if kube::job_succeeded(INIT_JOB, NAMESPACE).await {
        eprintln!("The Gitea init job has already run");
        return Ok(());
    }
    // A Job's pod template is immutable, so an unfinished one from an earlier attempt cannot be
    // applied over. Remove it and start again.
    kube::delete_ignore_missing(&["job", INIT_JOB, "-n", NAMESPACE]).await?;

    let chart = checkout
        .join("sources")
        .join(INIT_JOB_CHART)
        .join(INIT_JOB_VERSION);
    let chart_path = renderer.upload_chart(&chart, INIT_JOB_CHART).await?;

    eprintln!("Filling Gitea ...");
    let manifest = renderer
        .render(
            &helm::Template::new("gitea-init", chart_path)
                .set("clusterSize", &size_file(o.size))
                .set("domain", o.domain)
                .set("targetRevision", o.revision),
        )
        .await?;
    kube::apply(&manifest, "the Gitea init job").await?;
    kube::wait_condition(
        "complete",
        &format!("job/{INIT_JOB}"),
        NAMESPACE,
        INIT_JOB_TIMEOUT,
    )
    .await
    .inspect_err(|_| kube::print_job_logs(INIT_JOB, NAMESPACE))?;
    eprintln!("Gitea holds the cluster-forge sources");
    Ok(())
}

fn size_file(size: &str) -> String {
    format!("values_{size}.yaml")
}

fn root_url(domain: &str) -> String {
    format!("https://gitea.{domain}/")
}

/// A hex string of `bytes` random bytes, read straight from the kernel.
///
/// `/dev/urandom` keeps this free of a random-number dependency, and the password never leaves this
/// process except inside a manifest on a pipe.
fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .context("could not open /dev/urandom to generate the Gitea admin password")?
        .read_exact(&mut buf)
        .context("could not read /dev/urandom to generate the Gitea admin password")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).expect("valid yaml")
    }

    /// The shape cluster-forge ships: three placeholders as explicit nulls, and a versioned path.
    fn root_values() -> Value {
        yaml("global:\n  clusterSize: null\n  domain: null\nclusterForge:\n  repoUrl: http://gitea-http.cf-gitea.svc:3000/cluster-org/cluster-forge.git\n  targetRevision: null\napps:\n  gitea:\n    path: gitea/12.3.0\n    valuesObject:\n      persistence:\n        enabled: true\n")
    }

    fn options() -> Options<'static> {
        Options {
            domain: "cf.example.com",
            size: "medium",
            revision: "v2.2.2",
        }
    }

    fn built(size_overlay: Option<&Value>) -> Value {
        let rendered =
            cluster_values(&root_values(), size_overlay, &options()).expect("a document");
        serde_yaml::from_str(&rendered).expect("valid yaml")
    }

    #[test]
    fn fills_the_three_placeholders_cluster_forge_ships() {
        let doc = built(None);
        assert_eq!(
            values::at(&doc, &["global", "domain"]).and_then(Value::as_str),
            Some("cf.example.com")
        );
        assert_eq!(
            values::at(&doc, &["global", "clusterSize"]).and_then(Value::as_str),
            Some("values_medium.yaml")
        );
        assert_eq!(
            values::at(&doc, &["clusterForge", "targetRevision"]).and_then(Value::as_str),
            Some("v2.2.2")
        );
    }

    #[test]
    fn keeps_the_rest_of_the_document() {
        // The init job commits this whole file as the cluster-values repository, so every later
        // application reads its own values out of it. Dropping a key breaks an application that
        // this stage never mentions.
        let doc = built(None);
        assert_eq!(
            values::at(&doc, &["clusterForge", "repoUrl"]).and_then(Value::as_str),
            Some("http://gitea-http.cf-gitea.svc:3000/cluster-org/cluster-forge.git")
        );
        assert!(values::at(&doc, &["apps", "gitea", "valuesObject"]).is_some());
    }

    #[test]
    fn the_size_overlay_goes_on_after_the_placeholders() {
        // Order matters: filling after the merge would overwrite a size that sets one of them.
        let overlay = yaml("global:\n  domain: size.example.com\napps:\n  gitea:\n    valuesObject:\n      persistence:\n        enabled: false\n");
        let doc = built(Some(&overlay));
        assert_eq!(
            values::at(&doc, &["global", "domain"]).and_then(Value::as_str),
            Some("size.example.com")
        );
        assert_eq!(
            values::at(
                &doc,
                &["apps", "gitea", "valuesObject", "persistence", "enabled"]
            )
            .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn a_placeholder_the_document_drops_is_still_written() {
        let doc = cluster_values(&yaml("apps: {}\n"), None, &options()).expect("a document");
        let parsed: Value = serde_yaml::from_str(&doc).expect("valid yaml");
        assert_eq!(
            values::at(&parsed, &["global", "domain"]).and_then(Value::as_str),
            Some("cf.example.com")
        );
    }

    #[test]
    fn the_root_url_names_the_gitea_host_and_ends_in_a_slash() {
        // The chart writes this into Gitea's config verbatim, and Gitea builds its clone URLs from
        // it, so a missing slash reaches every repository address the init job registers.
        assert_eq!(root_url("cf.example.com"), "https://gitea.cf.example.com/");
    }

    #[test]
    fn the_size_file_matches_what_cluster_forge_ships() {
        assert_eq!(size_file("medium"), "values_medium.yaml");
        assert_eq!(size_file("small"), "values_small.yaml");
    }

    #[test]
    fn a_generated_password_is_hex_of_the_asked_length() {
        let password = random_hex(16).expect("random bytes");
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(password, random_hex(16).expect("random bytes"));
    }
}
