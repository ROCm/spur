use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod, ServiceAccount};
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::Client;
use kube::CustomResourceExt;
use spur_k8s::crd::SpurJob;
use tracing::info;

const APPLY_MANAGER: &str = "spur-k8s-tests";

fn deploy_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("deploy/k8s")
}

/// Patch `namespace: spur` and `.spur.svc.cluster.local` DNS names in a
/// parsed YAML value when using a dynamic namespace.
fn patch_namespace_in_value(value: &mut serde_json::Value, namespace: &str) {
    if namespace == "spur" {
        return;
    }
    let json_str = serde_json::to_string(value).unwrap_or_default();
    let patched = json_str
        .replace(
            "\"namespace\":\"spur\"",
            &format!("\"namespace\":\"{namespace}\""),
        )
        .replace(
            ".spur.svc.cluster.local",
            &format!(".{namespace}.svc.cluster.local"),
        );
    if let Ok(v) = serde_json::from_str(&patched) {
        *value = v;
    }
}

pub struct SuiteContext {
    client: Client,
    pub namespace: String,
}

impl SuiteContext {
    pub async fn setup() -> Result<Self> {
        let client = Client::try_default()
            .await
            .context("failed to create K8s client — is KUBECONFIG set?")?;

        let namespace = super::spur_namespace();
        let ctx = Self { client, namespace };

        ctx.apply_namespace().await?;
        ctx.apply_crd().await?;
        ctx.apply_rbac().await?;

        Ok(ctx)
    }

    pub async fn teardown(&self) -> Result<()> {
        let dp = DeleteParams::default();

        // CRD is cluster-scoped
        let crds: Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
            Api::all(self.client.clone());
        let _ = crds.delete("spurjobs.spur.ai", &dp).await;

        // ClusterRole and ClusterRoleBinding are cluster-scoped
        let cr_api: Api<ClusterRole> = Api::all(self.client.clone());
        let _ = cr_api.delete("spur-operator", &dp).await;

        let crb_api: Api<ClusterRoleBinding> = Api::all(self.client.clone());
        let _ = crb_api.delete("spur-operator", &dp).await;

        // Deleting the namespace cascades all namespaced resources
        let ns_api: Api<Namespace> = Api::all(self.client.clone());
        let _ = ns_api.delete(&self.namespace, &dp).await;

        info!(namespace = %self.namespace, "suite teardown complete");
        Ok(())
    }

    async fn apply_namespace(&self) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        let ns: Namespace = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": self.namespace,
                "labels": {
                    "app.kubernetes.io/part-of": "spur"
                }
            }
        }))?;
        api.patch(
            &self.namespace,
            &PatchParams::apply(APPLY_MANAGER).force(),
            &Patch::Apply(ns),
        )
        .await
        .context("failed to apply namespace")?;
        info!(namespace = %self.namespace, "namespace ensured");
        Ok(())
    }

    async fn apply_crd(&self) -> Result<()> {
        let crd = SpurJob::crd();
        let crds: Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
            Api::all(self.client.clone());
        crds.patch(
            "spurjobs.spur.ai",
            &PatchParams::apply(APPLY_MANAGER).force(),
            &Patch::Apply(crd),
        )
        .await
        .context("failed to apply SpurJob CRD")?;
        info!("SpurJob CRD applied");
        Ok(())
    }

    async fn apply_rbac(&self) -> Result<()> {
        let yaml = std::fs::read_to_string(deploy_root().join("rbac.yaml"))
            .context("failed to read rbac.yaml")?;

        for doc in yaml.split("\n---") {
            let doc = doc.trim();
            if doc.is_empty() {
                continue;
            }
            let mut value: serde_json::Value =
                serde_yaml::from_str(doc).context("failed to parse rbac.yaml document")?;
            patch_namespace_in_value(&mut value, &self.namespace);

            let kind = value["kind"].as_str().unwrap_or("").to_string();
            let name = value["metadata"]["name"].as_str().unwrap_or("").to_string();

            match kind.as_str() {
                "ServiceAccount" => {
                    let api: Api<ServiceAccount> =
                        Api::namespaced(self.client.clone(), &self.namespace);
                    let sa: ServiceAccount = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(sa),
                    )
                    .await?;
                }
                "ClusterRole" => {
                    let api: Api<ClusterRole> = Api::all(self.client.clone());
                    let cr: ClusterRole = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(cr),
                    )
                    .await?;
                }
                "ClusterRoleBinding" => {
                    let api: Api<ClusterRoleBinding> = Api::all(self.client.clone());
                    let crb: ClusterRoleBinding = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(crb),
                    )
                    .await?;
                }
                _ => bail!("unexpected kind in rbac.yaml: {kind}"),
            }
        }
        info!("RBAC applied");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FixtureConfig {
    pub replicas: u32,
    pub image: String,
    pub config_toml: String,
}

impl FixtureConfig {
    pub fn single_node() -> Self {
        let image = std::env::var("SPUR_CI_IMAGE").unwrap_or_else(|_| "spur:ci".to_string());
        Self {
            replicas: 1,
            image,
            config_toml: r#"
cluster_name = "k8s-ci"

[scheduler]
interval_secs = 1
plugin = "backfill"

[[partitions]]
name = "default"
state = "UP"
default = true
nodes = "ALL"
max_time = "1h"
default_time = "10m"
"#
            .to_string(),
        }
    }

    pub fn raft_ha() -> Self {
        let image = std::env::var("SPUR_CI_IMAGE").unwrap_or_else(|_| "spur:ci".to_string());
        Self {
            replicas: 3,
            image,
            config_toml: r#"
cluster_name = "k8s-ci-raft"

[controller]
peers = [
  "spurctld-0.spurctld.spur.svc.cluster.local:6821",
  "spurctld-1.spurctld.spur.svc.cluster.local:6821",
  "spurctld-2.spurctld.spur.svc.cluster.local:6821",
]

[scheduler]
interval_secs = 1
plugin = "backfill"

[[partitions]]
name = "default"
state = "UP"
default = true
nodes = "ALL"
max_time = "1h"
default_time = "10m"
"#
            .to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// ClusterFixture — deploys controller + operator into an existing namespace
// ---------------------------------------------------------------------------

pub struct ClusterFixture {
    client: Client,
    config: FixtureConfig,
    namespace: String,
}

impl ClusterFixture {
    /// Deploy Spur controller + operator. Requires SuiteContext to be set up first.
    pub async fn deploy(suite: &SuiteContext, config: FixtureConfig) -> Result<Self> {
        let client = Client::try_default()
            .await
            .context("failed to create K8s client")?;

        let fixture = Self {
            client,
            config,
            namespace: suite.namespace.clone(),
        };

        // Clean up any existing deployment to avoid stale Raft state on PVCs
        fixture.teardown_workloads().await?;
        tokio::time::sleep(Duration::from_secs(5)).await;

        fixture.apply_configmap().await?;
        fixture.apply_controller().await?;
        fixture.apply_operator().await?;
        fixture.wait_ready().await?;

        Ok(fixture)
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub async fn fresh_client(&self) -> Result<Client> {
        Client::try_default()
            .await
            .context("failed to create fresh K8s client")
    }

    pub fn spurjob_api(&self, namespace: &str) -> Api<SpurJob> {
        Api::namespaced(self.client.clone(), namespace)
    }

    pub fn pod_api(&self, namespace: &str) -> Api<Pod> {
        Api::namespaced(self.client.clone(), namespace)
    }

    pub fn config(&self) -> &FixtureConfig {
        &self.config
    }

    async fn apply_configmap(&self) -> Result<()> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let config_toml = self.config.config_toml.replace(
            ".spur.svc.cluster.local",
            &format!(".{}.svc.cluster.local", self.namespace),
        );
        let cm: ConfigMap = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "spur-config",
                "namespace": &self.namespace
            },
            "data": {
                "spur.conf": config_toml
            }
        }))?;
        api.patch(
            "spur-config",
            &PatchParams::apply(APPLY_MANAGER).force(),
            &Patch::Apply(cm),
        )
        .await
        .context("failed to apply ConfigMap")?;
        info!("ConfigMap spur-config applied");
        Ok(())
    }

    async fn apply_controller(&self) -> Result<()> {
        let yaml = std::fs::read_to_string(deploy_root().join("spurctld.yaml"))
            .context("failed to read spurctld.yaml")?;

        for doc in yaml.split("\n---") {
            let doc = doc.trim();
            if doc.is_empty() {
                continue;
            }
            let patched = doc.replace("spur:latest", &self.config.image).replace(
                "replicas: 3",
                &format!("replicas: {}", self.config.replicas),
            );

            let mut value: serde_json::Value =
                serde_yaml::from_str(&patched).context("failed to parse spurctld.yaml")?;
            patch_namespace_in_value(&mut value, &self.namespace);
            let kind = value["kind"].as_str().unwrap_or("").to_string();
            let name = value["metadata"]["name"].as_str().unwrap_or("").to_string();

            match kind.as_str() {
                "Service" => {
                    let api: Api<k8s_openapi::api::core::v1::Service> =
                        Api::namespaced(self.client.clone(), &self.namespace);
                    let svc: k8s_openapi::api::core::v1::Service = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(svc),
                    )
                    .await?;
                }
                "StatefulSet" => {
                    let api: Api<StatefulSet> =
                        Api::namespaced(self.client.clone(), &self.namespace);
                    let ss: StatefulSet = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(ss),
                    )
                    .await?;
                }
                _ => bail!("unexpected kind in spurctld.yaml: {kind}"),
            }
        }
        info!(
            replicas = self.config.replicas,
            image = %self.config.image,
            "controller StatefulSet applied"
        );
        Ok(())
    }

    async fn apply_operator(&self) -> Result<()> {
        let yaml = std::fs::read_to_string(deploy_root().join("operator.yaml"))
            .context("failed to read operator.yaml")?;

        for doc in yaml.split("\n---") {
            let doc = doc.trim();
            if doc.is_empty() {
                continue;
            }
            let patched = doc.replace("spur:latest", &self.config.image);
            let mut value: serde_json::Value =
                serde_yaml::from_str(&patched).context("failed to parse operator.yaml")?;
            patch_namespace_in_value(&mut value, &self.namespace);
            let kind = value["kind"].as_str().unwrap_or("").to_string();
            let name = value["metadata"]["name"].as_str().unwrap_or("").to_string();

            match kind.as_str() {
                "Service" => {
                    let api: Api<k8s_openapi::api::core::v1::Service> =
                        Api::namespaced(self.client.clone(), &self.namespace);
                    let svc: k8s_openapi::api::core::v1::Service = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(svc),
                    )
                    .await?;
                }
                "Deployment" => {
                    let api: Api<Deployment> =
                        Api::namespaced(self.client.clone(), &self.namespace);
                    let dep: Deployment = serde_json::from_value(value)?;
                    api.patch(
                        &name,
                        &PatchParams::apply(APPLY_MANAGER).force(),
                        &Patch::Apply(dep),
                    )
                    .await?;
                }
                _ => bail!("unexpected kind in operator.yaml: {kind}"),
            }
        }
        info!(image = %self.config.image, "operator Deployment applied");
        Ok(())
    }

    async fn wait_ready(&self) -> Result<()> {
        let timeout = Duration::from_secs(120);
        let start = std::time::Instant::now();

        let dep_api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        loop {
            if start.elapsed() > timeout {
                bail!("timeout waiting for operator deployment to be ready");
            }
            if let Ok(dep) = dep_api.get("spur-k8s-operator").await {
                let available = dep
                    .status
                    .as_ref()
                    .and_then(|s| s.available_replicas)
                    .unwrap_or(0);
                if available >= 1 {
                    info!("operator deployment ready");
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let expected = self.config.replicas as usize;
        loop {
            if start.elapsed() > timeout {
                bail!("timeout waiting for {expected} controller pod(s) to be ready");
            }
            let pods = pod_api
                .list(&ListParams::default().labels("app=spurctld"))
                .await?;
            let ready_count = pods
                .items
                .iter()
                .filter(|p| {
                    p.status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .map(|conds| {
                            conds
                                .iter()
                                .any(|c| c.type_ == "Ready" && c.status == "True")
                        })
                        .unwrap_or(false)
                })
                .count();
            if ready_count >= expected {
                info!(ready_count, "controller pod(s) ready");
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        Ok(())
    }

    /// Remove controller, operator, PVCs, and ConfigMap.
    pub async fn teardown_workloads(&self) -> Result<()> {
        let dp = DeleteParams::default();

        let dep_api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = dep_api.delete("spur-k8s-operator", &dp).await;

        let svc_api: Api<k8s_openapi::api::core::v1::Service> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let _ = svc_api.delete("spur-k8s-operator", &dp).await;

        let ss_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = ss_api.delete("spurctld", &dp).await;
        let _ = svc_api.delete("spurctld", &dp).await;

        let pvc_api: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        for i in 0..3 {
            let _ = pvc_api.delete(&format!("spool-spurctld-{i}"), &dp).await;
        }

        let cm_api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = cm_api.delete("spur-config", &dp).await;

        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(60) {
                break;
            }
            let pods = pod_api
                .list(&ListParams::default().labels("app=spurctld"))
                .await?;
            if pods.items.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        info!("workload teardown complete");
        Ok(())
    }

    pub async fn reconfigure(&mut self, new_config: FixtureConfig) -> Result<()> {
        self.teardown_workloads().await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        self.config = new_config;
        self.apply_configmap().await?;
        self.apply_controller().await?;
        self.apply_operator().await?;
        self.wait_ready().await?;
        Ok(())
    }
}
