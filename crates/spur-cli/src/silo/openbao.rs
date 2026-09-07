// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bootstrapping OpenBao, the second stage of the cluster-forge install.
//!
//! OpenBao comes before Gitea because Gitea's init job cannot run without it: that job reads the
//! root token out of the `openbao-keys` Secret, waits on the OpenBao service, and pulls its own
//! user password from a KV path. All three are products of this stage. Nothing in this stage
//! refers to Gitea, so the order is forced rather than chosen.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::Value;

use super::{helm, kube, renderer::Renderer, values};

pub const NAMESPACE: &str = "cf-openbao";

const APP: &str = "openbao";
const CONFIG_APP: &str = "openbao-config";

/// The init job is not an ArgoCD application, so cluster-forge's values pin no version for it.
const INIT_JOB_CHART: &str = "openbao-init-job";
const INIT_JOB_VERSION: &str = "0.1.0";
const INIT_JOB: &str = "openbao-init-job";

/// The two ConfigMaps the init job reads, and the names they take while ArgoCD does not yet own
/// them. Both charts are applied again later by ArgoCD under their real names, so the init-time
/// copies are renamed to keep the two from colliding.
const INIT_CONFIGMAPS: &[(&str, &str, &str)] = &[
    (
        "templates/openbao-secret-manager-cm.yaml",
        "openbao-secret-manager-scripts",
        "openbao-secret-manager-scripts-init",
    ),
    (
        "templates/openbao-secret-definitions.yaml",
        "openbao-secrets-config",
        "openbao-secrets-init-config",
    ),
];

const SERVER_POD: &str = "openbao-0";
const SERVER_TIMEOUT: &str = "300s";
const INIT_JOB_TIMEOUT: &str = "300s";

/// A database user whose password has to exist as a Kubernetes Secret before CNPG ever sees the
/// Cluster resource that names it.
struct CnpgUser {
    namespace: &'static str,
    secret: &'static str,
    username: &'static str,
    bao_path: &'static str,
}

/// CNPG reconciles its managed roles once, when the instance manager starts, and does not retry.
/// A role whose Secret has not appeared yet therefore keeps no password, and nothing fails until a
/// client tries to connect much later. Creating the Secrets here, while the OpenBao root token is
/// in hand, means they are already there when ArgoCD applies the Cluster.
const CNPG_USERS: &[CnpgUser] = &[
    CnpgUser {
        namespace: "airm",
        secret: "airm-cnpg-user",
        username: "airm_user",
        bao_path: "secrets/airm-cnpg-user-password",
    },
    CnpgUser {
        namespace: "aiwb",
        secret: "aiwb-cnpg-user",
        username: "aiwb_user",
        bao_path: "secrets/aiwb-cnpg-user-password",
    },
];

/// Install OpenBao, initialise it, and seed the secrets the rest of the install depends on.
pub async fn install(
    renderer: &Renderer,
    checkout: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    domain: &str,
) -> Result<()> {
    let server_values = values::assemble(root, size_overlay, APP);
    install_server(renderer, checkout, root, &server_values).await?;
    apply_init_configmaps(renderer, checkout, root, size_overlay, domain).await?;
    run_init_job(renderer, checkout, &server_values, domain).await?;
    preseed_cnpg_users().await
}

async fn install_server(
    renderer: &Renderer,
    checkout: &Path,
    root: &Value,
    server_values: &Value,
) -> Result<()> {
    let chart = chart_dir(checkout, root, APP)?;
    eprintln!("Installing OpenBao from {} ...", chart.display());

    let chart_path = renderer.upload_chart(&chart, APP).await?;
    let values_path = renderer
        .upload_values("openbao-bootstrap-values.yaml", server_values)
        .await?;
    let manifest = renderer
        .render(
            &helm::Template::new(APP, chart_path)
                .namespace(NAMESPACE)
                .values(values_path)
                .set("ui.enabled", "true"),
        )
        .await?;
    kube::apply_server_side(&manifest, "the OpenBao manifests").await?;

    // Not `Ready`: OpenBao serves sealed, and the init job below is what unseals it.
    kube::wait_jsonpath(
        "{.status.phase}",
        "Running",
        &format!("pod/{SERVER_POD}"),
        NAMESPACE,
        SERVER_TIMEOUT,
    )
    .await
}

/// Put the init-time copies of the secret-manager scripts and the secret definitions in place.
async fn apply_init_configmaps(
    renderer: &Renderer,
    checkout: &Path,
    root: &Value,
    size_overlay: Option<&Value>,
    domain: &str,
) -> Result<()> {
    let chart = chart_dir(checkout, root, CONFIG_APP)?;
    let chart_path = renderer.upload_chart(&chart, CONFIG_APP).await?;
    let config_values = values::assemble(root, size_overlay, CONFIG_APP);
    let values_path = renderer
        .upload_values("openbao-config-values.yaml", &config_values)
        .await?;

    for (template, from, to) in INIT_CONFIGMAPS {
        let manifest = renderer
            .render(
                &helm::Template::new("openbao-config-init", &chart_path)
                    .namespace(NAMESPACE)
                    .values(&values_path)
                    .set("domain", domain)
                    .show_only(template),
            )
            .await?;
        let renamed = rename_resource(&manifest, from, to)?;
        kube::apply_server_side(&renamed, to).await?;
    }
    Ok(())
}

/// Run the job that initialises OpenBao, unseals it, and writes every seeded secret.
async fn run_init_job(
    renderer: &Renderer,
    checkout: &Path,
    server_values: &Value,
    domain: &str,
) -> Result<()> {
    if kube::job_succeeded(INIT_JOB, NAMESPACE).await {
        eprintln!("The OpenBao init job has already run");
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
    let values_path = renderer
        .upload_values("openbao-init-values.yaml", server_values)
        .await?;

    eprintln!("Initialising OpenBao ...");
    let manifest = renderer
        .render(
            &helm::Template::new("openbao-init", chart_path)
                .values(values_path)
                .set("domain", domain),
        )
        .await?;
    kube::apply(&manifest, "the OpenBao init job").await?;
    kube::wait_condition(
        "complete",
        &format!("job/{INIT_JOB}"),
        NAMESPACE,
        INIT_JOB_TIMEOUT,
    )
    .await?;
    eprintln!("OpenBao is initialised");
    Ok(())
}

async fn preseed_cnpg_users() -> Result<()> {
    let token = kube::secret_field("openbao-keys", NAMESPACE, "root_token").await?;
    for user in CNPG_USERS {
        let password = read_bao_secret(&token, user.bao_path).await?;
        kube::ensure_namespace(user.namespace).await?;
        kube::apply_secret(
            user.secret,
            user.namespace,
            &[("username", user.username), ("password", &password)],
        )
        .await?;
        eprintln!("Pre-seeded {} in {}", user.secret, user.namespace);
    }
    Ok(())
}

/// Read one KV value out of OpenBao.
///
/// The token goes in on stdin and the path goes in as `$1`, so neither the token nor the value it
/// unlocks ever appears in an argument list that another process on the node could read.
async fn read_bao_secret(token: &str, path: &str) -> Result<String> {
    let script = "read -r BAO_TOKEN; export BAO_TOKEN; exec bao kv get -field=value \"$1\"";
    let out = kube::run_with_stdin(
        &[
            "exec", "-i", SERVER_POD, "-n", NAMESPACE, "--", "sh", "-c", script, "sh", path,
        ],
        format!("{token}\n").as_bytes(),
    )
    .await
    .with_context(|| format!("could not read {path} from OpenBao"))?;
    if !out.status.success() {
        bail!(
            "could not read {path} from OpenBao: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        bail!("OpenBao holds no value at {path}");
    }
    Ok(value)
}

fn chart_dir(checkout: &Path, root: &Value, app: &str) -> Result<std::path::PathBuf> {
    let version = values::app_chart_version(root, app)?;
    Ok(checkout.join("sources").join(app).join(version))
}

/// Rename the single resource in a rendered manifest.
///
/// The deployer this replaces ran `sed` over the rendered text. Parsing the document and setting
/// `metadata.name` cannot match the name where it appears as something other than the resource's
/// own, which `sed` did whenever a chart mentioned the name twice.
fn rename_resource(manifest: &[u8], from: &str, to: &str) -> Result<Vec<u8>> {
    let mut doc: Value =
        serde_yaml::from_slice(manifest).context("could not parse the rendered manifest")?;
    let name = doc
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str);
    match name {
        Some(name) if name == from => {}
        Some(name) => bail!("expected the rendered resource to be named {from}, found {name}"),
        None => bail!("the rendered resource has no metadata.name"),
    }
    doc["metadata"]["name"] = Value::String(to.to_string());
    serde_yaml::to_string(&doc)
        .map(String::into_bytes)
        .context("could not serialise the renamed resource")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).expect("valid yaml")
    }

    fn root_values() -> Value {
        yaml("apps:\n  openbao:\n    path: openbao/0.18.2\n    valuesObject:\n      server:\n        ha:\n          enabled: false\n      ui:\n        enabled: true\n  openbao-config:\n    path: openbao-config/0.1.0\n    valuesObject:\n      minio:\n        apiAccessKey: api\n")
    }

    #[test]
    fn each_chart_comes_from_the_version_cluster_forge_pins() {
        let checkout = Path::new("/srv/cluster-forge");
        assert_eq!(
            chart_dir(checkout, &root_values(), APP).expect("a chart"),
            Path::new("/srv/cluster-forge/sources/openbao/0.18.2")
        );
        assert_eq!(
            chart_dir(checkout, &root_values(), CONFIG_APP).expect("a chart"),
            Path::new("/srv/cluster-forge/sources/openbao-config/0.1.0")
        );
    }

    #[test]
    fn the_size_overlay_wins_over_the_pinned_values() {
        let overlay =
            yaml("apps:\n  openbao:\n    valuesObject:\n      server:\n        ha:\n          enabled: true\n");
        let assembled = values::assemble(&root_values(), Some(&overlay), APP);
        assert_eq!(
            values::at(&assembled, &["server", "ha", "enabled"]).and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            values::at(&assembled, &["ui", "enabled"]).and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn an_application_with_no_size_section_keeps_its_base_values() {
        let overlay = yaml("apps:\n  gitea:\n    valuesObject:\n      x: 1\n");
        let assembled = values::assemble(&root_values(), Some(&overlay), CONFIG_APP);
        assert_eq!(
            values::at(&assembled, &["minio", "apiAccessKey"]).and_then(Value::as_str),
            Some("api")
        );
    }

    #[test]
    fn renaming_changes_the_resource_name_and_nothing_else() {
        let rendered = b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: openbao-secrets-config\n  namespace: cf-openbao\ndata:\n  note: openbao-secrets-config is referenced here\n";
        let renamed = rename_resource(
            rendered,
            "openbao-secrets-config",
            "openbao-secrets-init-config",
        )
        .expect("renamed");
        let doc: Value = serde_yaml::from_slice(&renamed).expect("valid yaml");
        assert_eq!(
            values::at(&doc, &["metadata", "name"]).and_then(Value::as_str),
            Some("openbao-secrets-init-config")
        );
        // A textual substitution would have rewritten this too.
        assert_eq!(
            values::at(&doc, &["data", "note"]).and_then(Value::as_str),
            Some("openbao-secrets-config is referenced here")
        );
    }

    #[test]
    fn renaming_refuses_a_manifest_that_is_not_the_expected_resource() {
        // Guards against upstream renaming the ConfigMap: applying an unrenamed copy would collide
        // with the one ArgoCD manages, and the collision only shows up much later.
        let rendered = b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: something-else\n";
        assert!(rename_resource(rendered, "openbao-secrets-config", "x").is_err());
    }

    #[test]
    fn every_cnpg_user_names_a_distinct_secret_and_namespace() {
        // Two entries sharing a namespace and name would leave one password silently overwritten.
        let mut seen = std::collections::HashSet::new();
        for user in CNPG_USERS {
            assert!(seen.insert((user.namespace, user.secret)));
            assert!(user.bao_path.starts_with("secrets/"));
        }
    }
}
