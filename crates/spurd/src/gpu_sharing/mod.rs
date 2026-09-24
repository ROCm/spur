// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GPU-level sharing with the managed Kubernetes on this node. Inactive, with
//! no Kubernetes client, unless the controller marks the node as shared.
//!
//! Kubernetes allocation is the ledger of record: this module reads which
//! GPUs Kubernetes allocated (holds) and reports them on the heartbeat.

pub mod holds;
pub mod kubelet_links;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::api::resource::v1::{ResourceClaim, ResourceSlice};
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{watcher, WatchStreamExt};
use kube::Api;
use spur_core::resource::ResourceSet;
use spur_devices::cdi::{discover_sharing_identities, SharingIdentity};
use spur_proto::proto::GpuHoldReport;
use tokio::sync::{Notify, OnceCell};
use tokio_stream::StreamExt;
use tracing::{info, warn};

use holds::{K8sView, DRA_DRIVER};
pub use kubelet_links::KubeletLinks;

/// Node label the GPU operator's `DeviceConfig`s select on.
pub const SHARING_LABEL: &str = "spur.amd.com/gpu-sharing";

const CONVERGE_INTERVAL: Duration = Duration::from_secs(30);

/// The Kubernetes Node name of the host `hostname`. k0s starts the kubelet
/// without `--hostname-override`, and the kubelet lowercases the hostname.
pub fn k8s_node_name(hostname: &str) -> String {
    hostname.to_lowercase()
}

/// Shared handle for this node's GPU sharing state.
pub struct GpuSharing {
    node_name: String,
    k0s: Option<Arc<crate::cluster::K0sAgent>>,
    links: KubeletLinks,
    /// The controller's flag; `None` until the first heartbeat response.
    desired: Mutex<Option<bool>>,
    client: OnceCell<kube::Client>,
    /// Why the whole node cannot share now, e.g. a foreign kubelet path.
    problem: Mutex<Option<String>>,
    /// Label value last written to the Node.
    labeled: Mutex<Option<bool>>,
    watch: Mutex<Option<Watch>>,
    identities: Mutex<(u64, HashMap<u64, SharingIdentity>)>,
    changed: Notify,
    converge: Notify,
}

struct Watch {
    claims: Store<ResourceClaim>,
    slices: Store<ResourceSlice>,
    healthy: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GpuSharing {
    pub fn new(hostname: &str, k0s: Arc<crate::cluster::K0sAgent>) -> Arc<Self> {
        Arc::new(Self::build(
            hostname,
            Some(k0s),
            KubeletLinks::system(),
            None,
        ))
    }

    fn build(
        hostname: &str,
        k0s: Option<Arc<crate::cluster::K0sAgent>>,
        links: KubeletLinks,
        client: Option<kube::Client>,
    ) -> Self {
        Self {
            node_name: k8s_node_name(hostname),
            k0s,
            links,
            desired: Mutex::new(None),
            client: OnceCell::new_with(client),
            problem: Mutex::new(None),
            labeled: Mutex::new(None),
            watch: Mutex::new(None),
            identities: Mutex::new((0, HashMap::new())),
            changed: Notify::new(),
            converge: Notify::new(),
        }
    }

    /// This node's Kubernetes Node name, also the pool name of its DRA devices.
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// Whether the controller marks this node as shared.
    pub fn is_shared(&self) -> bool {
        *lock(&self.desired) == Some(true)
    }

    /// Admin client for the managed k0s, built on first use. The client keeps
    /// kube's defaults (30 s connect, 295 s read), so a slow API server is
    /// waited for, not treated as lost. A failed build is retried on the next call.
    pub async fn client(&self) -> anyhow::Result<kube::Client> {
        self.client
            .get_or_try_init(|| async {
                let k0s = self.k0s.as_ref().context("no k0s agent on this node")?;
                let yaml = k0s.admin_kubeconfig().await?;
                let kubeconfig = kube::config::Kubeconfig::from_yaml(&yaml)?;
                let config = kube::Config::from_custom_kubeconfig(
                    kubeconfig,
                    &kube::config::KubeConfigOptions::default(),
                )
                .await?;
                Ok(kube::Client::try_from(config)?)
            })
            .await
            .cloned()
    }

    /// Sharing identity of the GPU with `stable_id`, rediscovered when unknown.
    pub fn identity(&self, stable_id: u64) -> Option<SharingIdentity> {
        let mut cache = lock(&self.identities);
        if !cache.1.contains_key(&stable_id) {
            cache.1 = discover_identities();
        }
        cache.1.get(&stable_id).cloned()
    }

    /// Ask for one extra heartbeat now because the hold state changed.
    pub fn notify_change(&self) {
        self.changed.notify_one();
    }

    /// Completes when a hold change asks for an extra heartbeat.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    /// Converge to the controller's flag from a heartbeat response.
    pub fn set_desired(&self, shared: bool) {
        let previous = lock(&self.desired).replace(shared);
        if previous != Some(shared) {
            info!(shared, node = %self.node_name, "GPU sharing flag changed");
            self.converge.notify_one();
        }
    }

    /// Placeholder pods of Spur jobs still on this node. Opt-out keeps the
    /// kubelet links until this is zero.
    // ponytail: stub until placeholder pods exist; they replace it with a real count.
    fn live_placeholders(&self) -> usize {
        0
    }

    pub async fn converge_loop(self: Arc<Self>) {
        loop {
            self.converge_once().await;
            tokio::select! {
                _ = tokio::time::sleep(CONVERGE_INTERVAL) => {}
                _ = self.converge.notified() => {}
            }
        }
    }

    async fn converge_once(self: &Arc<Self>) {
        let desired = *lock(&self.desired);
        match desired {
            None => {}
            Some(true) => self.opt_in().await,
            Some(false) => self.opt_out().await,
        }
    }

    async fn opt_in(self: &Arc<Self>) {
        if let Err(reason) = self.links.ensure() {
            self.set_problem(Some(reason));
            return;
        }
        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                self.set_problem(Some(format!("no Kubernetes API access: {e:#}")));
                return;
            }
        };
        self.set_problem(None);
        if let Err(e) = self.set_label(&client, true).await {
            warn!(error = %e, "cannot label the Node as shared; retrying");
        }
        self.ensure_watch(client);
    }

    async fn opt_out(self: &Arc<Self>) {
        lock(&self.watch).take();
        self.set_problem(None);
        let was_shared = self.links.any_ours() || *lock(&self.labeled) == Some(true);
        if !was_shared {
            return;
        }
        if *lock(&self.labeled) != Some(false) {
            let result = match self.client().await {
                Ok(client) => self.set_label(&client, false).await.map_err(Into::into),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!(error = %e, "cannot label the Node as not shared; retrying");
                return;
            }
        }
        if self.live_placeholders() > 0 {
            return;
        }
        match self.links.remove_ours() {
            Ok(()) => info!("GPU sharing off; kubelet plugin links removed"),
            Err(e) => warn!(error = %e, "cannot remove the kubelet plugin links"),
        }
    }

    async fn set_label(&self, client: &kube::Client, shared: bool) -> kube::Result<()> {
        if *lock(&self.labeled) == Some(shared) {
            return Ok(());
        }
        let patch =
            serde_json::json!({"metadata": {"labels": {SHARING_LABEL: shared.to_string()}}});
        Api::<Node>::all(client.clone())
            .patch(
                &self.node_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
        *lock(&self.labeled) = Some(shared);
        info!(node = %self.node_name, shared, "labelled Node {SHARING_LABEL}");
        Ok(())
    }

    fn set_problem(&self, problem: Option<String>) {
        let mut current = lock(&self.problem);
        if *current != problem {
            if let Some(reason) = &problem {
                warn!(reason, "node cannot share its GPUs");
            }
            *current = problem;
            self.changed.notify_one();
        }
    }

    fn ensure_watch(self: &Arc<Self>, client: kube::Client) {
        let mut slot = lock(&self.watch);
        if slot.as_ref().is_some_and(|w| !w.task.is_finished()) {
            return;
        }
        let (claims, claims_writer) = reflector::store();
        let (slices, slices_writer) = reflector::store();
        let healthy = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(Arc::clone(self).run_watch(
            client,
            claims_writer,
            slices_writer,
            healthy.clone(),
        ));
        *slot = Some(Watch {
            claims,
            slices,
            healthy,
            task,
        });
    }

    /// Mirror this node's claims and slices. A change to what they say about
    /// the node's GPUs asks for an extra heartbeat; a failing watch stops the
    /// report, so the controller sees the node as stale.
    async fn run_watch(
        self: Arc<Self>,
        client: kube::Client,
        mut claims_writer: reflector::store::Writer<ResourceClaim>,
        mut slices_writer: reflector::store::Writer<ResourceSlice>,
        healthy: Arc<AtomicBool>,
    ) {
        enum Event {
            Claim(Result<watcher::Event<ResourceClaim>, watcher::Error>),
            Slice(Result<watcher::Event<ResourceSlice>, watcher::Error>),
        }
        // ponytail: claims have no node field selector, so every claim in the cluster is watched.
        let claims = watcher(Api::all(client.clone()), watcher::Config::default())
            .default_backoff()
            .map(Event::Claim);
        let slice_fields = format!("spec.nodeName={},spec.driver={DRA_DRIVER}", self.node_name);
        let slices = watcher(
            Api::all(client),
            watcher::Config::default().fields(&slice_fields),
        )
        .default_backoff()
        .map(Event::Slice);
        let mut events = std::pin::pin!(claims.merge(slices));
        let (mut claims_synced, mut slices_synced) = (false, false);
        let mut last_view: Option<K8sView> = None;
        while let Some(event) = events.next().await {
            match event {
                Event::Claim(Ok(e)) => {
                    claims_synced |= matches!(e, watcher::Event::InitDone);
                    claims_writer.apply_watcher_event(&e);
                }
                Event::Slice(Ok(e)) => {
                    slices_synced |= matches!(e, watcher::Event::InitDone);
                    slices_writer.apply_watcher_event(&e);
                }
                Event::Claim(Err(e)) | Event::Slice(Err(e)) => {
                    warn!(error = %e, "Kubernetes watch failed; holds not reported until it recovers");
                    healthy.store(false, Ordering::Relaxed);
                    continue;
                }
            }
            let ready = claims_synced && slices_synced;
            healthy.store(ready, Ordering::Relaxed);
            let view = self.view();
            if ready && view != last_view {
                last_view = view;
                self.notify_change();
            }
        }
    }

    fn view(&self) -> Option<K8sView> {
        let watch = lock(&self.watch);
        let watch = watch.as_ref()?;
        if !watch.healthy.load(Ordering::Relaxed) {
            return None;
        }
        let claims = watch.claims.state();
        let slices = watch.slices.state();
        Some(K8sView::from_objects(
            claims.iter().map(Arc::as_ref),
            slices.iter().map(Arc::as_ref),
            &self.node_name,
        ))
    }

    /// Hold report for the heartbeat: `None` when the node is not shared or
    /// Kubernetes cannot be read, so the controller treats every GPU as held.
    pub fn heartbeat_report(
        &self,
        inventory: &ResourceSet,
        job_gpus: &HashMap<u32, Vec<u64>>,
    ) -> Option<GpuHoldReport> {
        if !self.is_shared() {
            return None;
        }
        if let Some(reason) = lock(&self.problem).clone() {
            return Some(GpuHoldReport {
                generation: inventory.generation,
                gpus: Vec::new(),
                unshareable_reason: reason,
            });
        }
        let view = self.view()?;
        let identities = {
            let mut cache = lock(&self.identities);
            if cache.0 != inventory.generation || cache.1.is_empty() {
                *cache = (inventory.generation, discover_identities());
            }
            cache.1.clone()
        };
        let stable_ids: Vec<u64> = inventory.gpus.iter().map(|g| g.stable_id).collect();
        Some(holds::build_report(
            inventory.generation,
            &stable_ids,
            &identities,
            &view,
            job_gpus,
            &self.node_name,
        ))
    }
}

fn discover_identities() -> HashMap<u64, SharingIdentity> {
    discover_sharing_identities()
        .into_iter()
        .map(|i| (i.stable_id, i))
        .collect()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remove this node's kubelet plugin links, for `spur k8s down --reset`.
pub fn remove_kubelet_links() {
    if let Err(e) = KubeletLinks::system().remove_ours() {
        warn!(error = %e, "cannot remove the kubelet plugin links");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeApiServer;
    use super::*;
    use http::StatusCode;

    fn sharing(server: &FakeApiServer, root: &std::path::Path) -> Arc<GpuSharing> {
        Arc::new(GpuSharing::build(
            "GPU-Node-1",
            None,
            KubeletLinks::under(root),
            Some(server.client()),
        ))
    }

    fn node_ok() -> FakeApiServer {
        FakeApiServer::answering(
            StatusCode::OK,
            &serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "gpu-node-1"}}),
        )
    }

    #[test]
    fn node_name_is_the_lowercase_hostname() {
        assert_eq!(k8s_node_name("GPU-Node-1"), "gpu-node-1");
    }

    #[tokio::test]
    async fn label_patch_is_a_merge_patch_on_the_own_node() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());

        sharing.set_label(&server.client(), true).await.unwrap();
        sharing.set_label(&server.client(), true).await.unwrap();
        sharing.set_label(&server.client(), false).await.unwrap();

        let seen = server.requests();
        assert_eq!(seen.len(), 2, "an unchanged label is not patched again");
        assert_eq!(seen[0].method, http::Method::PATCH);
        assert_eq!(seen[0].path, "/api/v1/nodes/gpu-node-1");
        assert_eq!(seen[0].content_type, "application/merge-patch+json");
        assert_eq!(
            seen[0].body,
            serde_json::json!({"metadata": {"labels": {SHARING_LABEL: "true"}}})
        );
        assert_eq!(
            seen[1].body,
            serde_json::json!({"metadata": {"labels": {SHARING_LABEL: "false"}}})
        );
    }

    #[tokio::test]
    async fn opt_in_links_labels_and_reports_nothing_before_the_watch_syncs() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(true);

        sharing.converge_once().await;

        assert!(KubeletLinks::under(root.path()).any_ours());
        assert_eq!(*lock(&sharing.labeled), Some(true));
        assert!(sharing
            .heartbeat_report(&ResourceSet::default(), &HashMap::new())
            .is_none());
    }

    #[tokio::test]
    async fn foreign_kubelet_path_is_reported_as_unshareable() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("var/lib/kubelet/plugins")).unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(true);

        sharing.converge_once().await;

        let inventory = ResourceSet {
            generation: 5,
            ..Default::default()
        };
        let report = sharing
            .heartbeat_report(&inventory, &HashMap::new())
            .unwrap();
        assert_eq!(report.generation, 5);
        assert!(report
            .unshareable_reason
            .contains("var/lib/kubelet/plugins exists"));
        assert!(
            server.requests().is_empty(),
            "no API call before the links are in place"
        );
    }

    #[tokio::test]
    async fn opt_out_labels_false_and_removes_own_links() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(true);
        sharing.converge_once().await;

        sharing.set_desired(false);
        sharing.converge_once().await;

        assert!(!KubeletLinks::under(root.path()).any_ours());
        assert_eq!(*lock(&sharing.labeled), Some(false));
        assert!(lock(&sharing.watch).is_none());
        assert!(sharing
            .heartbeat_report(&ResourceSet::default(), &HashMap::new())
            .is_none());
    }

    #[tokio::test]
    async fn node_never_shared_stays_inactive() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(false);

        sharing.converge_once().await;

        assert!(server.requests().is_empty());
        assert!(!root.path().join("var/lib/kubelet").exists());
    }

    #[tokio::test]
    async fn unknown_flag_changes_nothing() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        KubeletLinks::under(root.path()).ensure().unwrap();
        let sharing = sharing(&server, root.path());

        sharing.converge_once().await;

        assert!(KubeletLinks::under(root.path()).any_ours());
        assert!(server.requests().is_empty());
    }
}
