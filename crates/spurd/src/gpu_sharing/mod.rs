// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GPU-level sharing with the managed Kubernetes on this node. Inactive, with
//! no Kubernetes client, unless the controller marks the node as shared.
//!
//! Kubernetes allocation is the ledger of record: this module reads which
//! GPUs Kubernetes allocated (holds) and reports them on the heartbeat.

pub mod credential;
pub mod holds;
pub mod kubelet_links;
pub mod placeholder;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

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
use tokio::sync::Notify;
use tokio_stream::StreamExt;
use tracing::{info, warn};

use crate::cluster::ClusterRole;
use crate::reporter::NodeReporter;
use holds::{K8sView, DRA_DRIVER};
pub use kubelet_links::KubeletLinks;
pub use spur_core::k0s::k8s_node_name;

/// Node label the GPU operator's `DeviceConfig`s select on.
pub const SHARING_LABEL: &str = "spur.amd.com/gpu-sharing";

const CONVERGE_INTERVAL: Duration = Duration::from_secs(30);
/// A worker's token is bound and expires (see [`credential::TOKEN_DURATION`]).
const CLIENT_MAX_AGE: Duration = Duration::from_secs(12 * 3600);

fn client_expired(built: Instant, now: Instant) -> bool {
    now.saturating_duration_since(built) >= CLIENT_MAX_AGE
}

/// Only a node with a kubelet and GPUs has a Node the GPU operator selects on.
fn carries_label(role: Option<ClusterRole>, has_gpus: bool) -> bool {
    has_gpus && matches!(role, Some(ClusterRole::Worker | ClusterRole::Single))
}

fn is_unauthorized(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(s) if s.code == 401)
}

fn watch_unauthorized(e: &watcher::Error) -> bool {
    match e {
        watcher::Error::InitialListFailed(e)
        | watcher::Error::WatchStartFailed(e)
        | watcher::Error::WatchFailed(e) => is_unauthorized(e),
        watcher::Error::WatchError(s) => s.code == 401,
        watcher::Error::NoResourceVersion => false,
    }
}

/// Shared handle for this node's GPU sharing state.
pub struct GpuSharing {
    node_name: String,
    k0s: Option<Arc<crate::cluster::K0sAgent>>,
    /// Asks the controller for a worker's kubeconfig and knows the GPU inventory.
    reporter: Weak<NodeReporter>,
    links: KubeletLinks,
    /// The controller's flag; `None` until the first heartbeat response.
    desired: Mutex<Option<bool>>,
    client: Mutex<Option<(kube::Client, Instant)>>,
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
    pub fn new(
        hostname: &str,
        k0s: Arc<crate::cluster::K0sAgent>,
        reporter: &Arc<NodeReporter>,
    ) -> Arc<Self> {
        Arc::new(Self::build(
            hostname,
            Some(k0s),
            Arc::downgrade(reporter),
            KubeletLinks::system(),
            None,
        ))
    }

    fn build(
        hostname: &str,
        k0s: Option<Arc<crate::cluster::K0sAgent>>,
        reporter: Weak<NodeReporter>,
        links: KubeletLinks,
        client: Option<kube::Client>,
    ) -> Self {
        Self {
            node_name: k8s_node_name(hostname),
            k0s,
            reporter,
            links,
            desired: Mutex::new(None),
            client: Mutex::new(client.map(|c| (c, Instant::now()))),
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

    /// Client for the managed k0s, built on first use and rebuilt after
    /// [`CLIENT_MAX_AGE`] or a 401. The client keeps kube's defaults (30 s
    /// connect, 295 s read), so a slow API server is waited for, not treated
    /// as lost. A failed build is retried on the next call.
    pub async fn client(&self) -> anyhow::Result<kube::Client> {
        let cached = lock(&self.client).clone();
        if let Some((client, built)) = cached {
            if !client_expired(built, Instant::now()) {
                return Ok(client);
            }
        }
        let yaml = self.kubeconfig().await?;
        let kubeconfig = kube::config::Kubeconfig::from_yaml(&yaml)?;
        let config = kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &kube::config::KubeConfigOptions::default(),
        )
        .await?;
        let client = kube::Client::try_from(config)?;
        *lock(&self.client) = Some((client.clone(), Instant::now()));
        Ok(client)
    }

    /// A control-plane node reads its admin kubeconfig. A worker has none, so
    /// it gets a scoped one through the controller.
    async fn kubeconfig(&self) -> anyhow::Result<String> {
        let k0s = self.k0s.as_ref().context("no k0s agent on this node")?;
        if k0s.role().await != Some(ClusterRole::Worker) {
            return k0s.admin_kubeconfig().await;
        }
        self.reporter
            .upgrade()
            .context("no controller connection")?
            .gpu_sharing_kubeconfig()
            .await
    }

    /// Drop the client when the API server rejects its token, so the next
    /// call builds a new one.
    pub fn forget_client_if_unauthorized(&self, e: &kube::Error) {
        if is_unauthorized(e) && lock(&self.client).take().is_some() {
            info!("Kubernetes rejected the token; rebuilding the client");
        }
    }

    async fn carries_label(&self) -> bool {
        let Some(k0s) = &self.k0s else {
            return false;
        };
        let has_gpus = self.reporter.upgrade().is_some_and(|r| r.has_gpus());
        carries_label(k0s.role().await, has_gpus)
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
        let labelled = self.carries_label().await;
        self.converge_as(labelled).await;
    }

    async fn converge_as(self: &Arc<Self>, carries_label: bool) {
        let desired = *lock(&self.desired);
        match desired {
            None => {}
            Some(true) => self.opt_in().await,
            Some(false) => self.opt_out(carries_label).await,
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
            self.forget_client_if_unauthorized(&e);
            warn!(error = %e, "cannot label the Node as shared; retrying");
        }
        self.ensure_watch(client);
    }

    /// Every enrolled GPU node that is not shared carries the label `false`,
    /// because the GPU operator's selectors match equality only.
    async fn opt_out(self: &Arc<Self>, carries_label: bool) {
        lock(&self.watch).take();
        self.set_problem(None);
        let was_shared = self.links.any_ours() || *lock(&self.labeled) == Some(true);
        if (carries_label || was_shared) && *lock(&self.labeled) != Some(false) {
            let result = match self.client().await {
                Ok(client) => self.set_label(&client, false).await.map_err(|e| {
                    self.forget_client_if_unauthorized(&e);
                    e.into()
                }),
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!(error = %e, "cannot label the Node as not shared; retrying");
                return;
            }
        }
        if !self.links.any_ours() || self.live_placeholders() > 0 {
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
                    if watch_unauthorized(&e) {
                        // Restarted with a new client by the next converge.
                        lock(&self.client).take();
                        return;
                    }
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
            Weak::new(),
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
    fn client_is_rebuilt_after_twelve_hours() {
        let built = Instant::now();

        assert!(!client_expired(built, built));
        assert!(!client_expired(
            built,
            built + Duration::from_secs(12 * 3600 - 1)
        ));
        assert!(client_expired(
            built,
            built + Duration::from_secs(12 * 3600)
        ));
        assert!(!client_expired(built + Duration::from_secs(1), built));
    }

    #[test]
    fn only_enrolled_gpu_nodes_with_a_kubelet_carry_the_label() {
        assert!(carries_label(Some(ClusterRole::Worker), true));
        assert!(carries_label(Some(ClusterRole::Single), true));
        assert!(!carries_label(Some(ClusterRole::Controller), true));
        assert!(!carries_label(None, true));
        assert!(!carries_label(Some(ClusterRole::Worker), false));
    }

    #[tokio::test]
    async fn rejected_token_drops_the_client() {
        let server = FakeApiServer::answering(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                                "reason": "Unauthorized", "code": 401}),
        );
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(false);

        sharing.converge_as(true).await;

        assert_eq!(server.requests().len(), 1);
        assert!(lock(&sharing.client).is_none());
        assert_eq!(*lock(&sharing.labeled), None);
    }

    #[tokio::test]
    async fn non_shared_enrolled_gpu_node_is_labelled_false_without_watches() {
        let server = node_ok();
        let root = tempfile::tempdir().unwrap();
        let sharing = sharing(&server, root.path());
        sharing.set_desired(false);

        sharing.converge_as(true).await;
        sharing.converge_as(true).await;

        let seen = server.requests();
        assert_eq!(seen.len(), 1, "the label is written once");
        assert_eq!(seen[0].method, http::Method::PATCH);
        assert_eq!(
            seen[0].body,
            serde_json::json!({"metadata": {"labels": {SHARING_LABEL: "false"}}})
        );
        assert!(lock(&sharing.watch).is_none());
        assert!(!root.path().join("var/lib/kubelet").exists());
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
