// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The placeholder pod and `ResourceClaim` that make Kubernetes allocate the
//! GPUs of a Spur job on a shared node, so pods cannot get them.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    Container, Namespace, Pod, PodResourceClaim, PodSpec, ResourceClaim as ResourceClaimRef,
    ResourceRequirements,
};
use k8s_openapi::api::resource::v1::{
    CELDeviceSelector, DeviceClaim, DeviceRequest, DeviceSelector, ExactDeviceRequest,
    ResourceClaim, ResourceClaimSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use spur_proto::proto::{AllocatedDevice, DeviceAllocations, ResourceAllocations};
use tokio::time::Instant;
use tracing::{debug, warn};

pub const NAMESPACE: &str = "spur-system";
pub const DRA_DRIVER: &str = "gpu.amd.com";
pub const DEVICE_CLASS: &str = "gpu.amd.com";
/// The sandbox image k0s pins (`KubePauseContainerImage` in k0s v1.36.2), so
/// every k0s node already has it locally.
pub const PAUSE_IMAGE: &str = "quay.io/k0sproject/pause:3.10.2-0";

pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "spurd";
pub const LABEL_JOB_ID: &str = "spur.amd.com/job-id";
pub const LABEL_RUN_ATTEMPT: &str = "spur.amd.com/run-attempt";
pub const LABEL_NODE: &str = "spur.amd.com/node";
pub const LABEL_USER: &str = "spur.amd.com/user";
pub const LABEL_ACCOUNT: &str = "spur.amd.com/account";

const POD_CLAIM_NAME: &str = "gpus";
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
// ponytail: polling, switch to a watch if the API server load matters.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One GPU as the placeholder sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedGpu {
    pub stable_id: u64,
    /// DRA device name, `gpu-<card>-<renderD>`.
    pub dra_device: String,
    /// `0000:bb:dd.f`, function forced to 0 for a partition.
    pub selector_bdf: String,
    pub parent_key: u64,
}

#[derive(Debug, Clone)]
pub struct PlaceholderSpec {
    pub job_id: u32,
    pub run_attempt: u32,
    pub user: String,
    pub account: String,
    /// Kubernetes Node name of this node.
    pub node_name: String,
    pub pause_image: String,
    pub gpus: Vec<RequestedGpu>,
}

impl PlaceholderSpec {
    pub fn name(&self) -> String {
        placeholder_name(self.job_id, self.run_attempt, &self.node_name)
    }
}

/// The node is part of the name because a multi-node job has one placeholder
/// per node in the same namespace.
pub fn placeholder_name(job_id: u32, run_attempt: u32, node_name: &str) -> String {
    format!("spur-job-{job_id}-{run_attempt}-{node_name}")
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("Kubernetes cannot allocate the GPUs: {0}")]
    Unschedulable(String),
    #[error("Kubernetes did not allocate the GPUs before the launch deadline")]
    Deadline,
    #[error("Kubernetes rejected the placeholder: {0}")]
    Rejected(kube::Error),
    #[error(transparent)]
    Mapping(#[from] MappingError),
}

impl From<AcquireError> for tonic::Status {
    fn from(e: AcquireError) -> Self {
        tonic::Status::resource_exhausted(e.to_string())
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    #[error("claim holds device {device} of driver {driver} pool {pool}, not of this node")]
    ForeignDevice {
        driver: String,
        pool: String,
        device: String,
    },
    #[error("claim holds device {0}, which this node does not know")]
    UnknownDevice(String),
    #[error("claim holds {allocated:?}, which does not match the requested {requested:?}")]
    Mismatch {
        requested: Vec<String>,
        allocated: Vec<String>,
    },
}

/// Result of [`ensure_present`] for a running job.
#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    /// The claim holds exactly the job's devices.
    Held,
    /// The claim exists but is not allocated yet.
    Pending,
    /// Kubernetes gave the job's devices, or some of them, to something else.
    Conflict(String),
}

fn labels(spec: &PlaceholderSpec) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
        (LABEL_JOB_ID.to_string(), spec.job_id.to_string()),
        (LABEL_RUN_ATTEMPT.to_string(), spec.run_attempt.to_string()),
        (LABEL_NODE.to_string(), spec.node_name.clone()),
    ]);
    // ponytail: a user or account that is not a valid label value is left out
    // rather than failing the launch; the job id label identifies the job.
    for (key, value) in [(LABEL_USER, &spec.user), (LABEL_ACCOUNT, &spec.account)] {
        if is_label_value(value) {
            labels.insert(key.to_string(), value.clone());
        }
    }
    labels
}

fn is_label_value(v: &str) -> bool {
    let alnum = |c: Option<char>| c.is_none_or(|c| c.is_ascii_alphanumeric());
    v.len() <= 63
        && alnum(v.chars().next())
        && alnum(v.chars().last())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn metadata(spec: &PlaceholderSpec) -> ObjectMeta {
    ObjectMeta {
        name: Some(spec.name()),
        namespace: Some(NAMESPACE.to_string()),
        labels: Some(labels(spec)),
        ..Default::default()
    }
}

/// One request per distinct selector BDF: a whole GPU gets `count` 1, a
/// partitioned parent gets `count` k and kube-scheduler picks the siblings.
pub fn build_claim(spec: &PlaceholderSpec) -> ResourceClaim {
    let mut groups: Vec<(&str, i64)> = Vec::new();
    for gpu in &spec.gpus {
        match groups.iter_mut().find(|(bdf, _)| *bdf == gpu.selector_bdf) {
            Some((_, count)) => *count += 1,
            None => groups.push((&gpu.selector_bdf, 1)),
        }
    }
    let requests = groups
        .into_iter()
        .enumerate()
        .map(|(i, (bdf, count))| DeviceRequest {
            name: format!("g{i}"),
            exactly: Some(ExactDeviceRequest {
                device_class_name: DEVICE_CLASS.to_string(),
                allocation_mode: Some("ExactCount".to_string()),
                count: Some(count),
                selectors: Some(vec![DeviceSelector {
                    cel: Some(CELDeviceSelector {
                        expression: format!(
                            "device.attributes[\"resource.kubernetes.io\"].pciBusID == \"{bdf}\""
                        ),
                    }),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    ResourceClaim {
        metadata: metadata(spec),
        spec: ResourceClaimSpec {
            devices: Some(DeviceClaim {
                requests: Some(requests),
                ..Default::default()
            }),
        },
        status: None,
    }
}

/// The pod goes through kube-scheduler (a node selector, never `nodeName`),
/// because only the scheduler allocates a claim.
pub fn build_pod(spec: &PlaceholderSpec) -> Pod {
    Pod {
        metadata: metadata(spec),
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "pause".to_string(),
                image: Some(spec.pause_image.clone()),
                image_pull_policy: Some("IfNotPresent".to_string()),
                resources: Some(ResourceRequirements {
                    claims: Some(vec![ResourceClaimRef {
                        name: POD_CLAIM_NAME.to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            resource_claims: Some(vec![PodResourceClaim {
                name: POD_CLAIM_NAME.to_string(),
                resource_claim_name: Some(spec.name()),
                ..Default::default()
            }]),
            node_selector: Some(BTreeMap::from([(
                "kubernetes.io/hostname".to_string(),
                spec.node_name.clone(),
            )])),
            restart_policy: Some("Always".to_string()),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            ..Default::default()
        }),
        status: None,
    }
}

fn is_code(e: &kube::Error, code: u16) -> bool {
    matches!(e, kube::Error::Api(s) if s.code == code)
}

/// Bad Request and Invalid do not change on retry; everything else can.
/// Unauthorized does not change with the same client, so the caller must
/// build a new client before it tries again.
fn is_permanent(e: &kube::Error) -> bool {
    is_code(e, 400) || is_code(e, 401) || is_code(e, 422)
}

fn exists_ok(r: Result<impl Sized, kube::Error>) -> Result<(), kube::Error> {
    match r {
        Err(e) if !is_code(&e, 409) => Err(e),
        _ => Ok(()),
    }
}

fn not_found_ok(r: Result<impl Sized, kube::Error>) -> Result<(), kube::Error> {
    match r {
        Err(e) if !is_code(&e, 404) => Err(e),
        _ => Ok(()),
    }
}

/// Retries one API call until it answers or the deadline passes. A call that
/// is slow or fails for a reason that can change is never a loss.
async fn retry<T, F, Fut>(deadline: Instant, what: &str, mut call: F) -> Result<T, AcquireError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, kube::Error>>,
{
    loop {
        let call_deadline = deadline.min(Instant::now() + CALL_TIMEOUT);
        match tokio::time::timeout_at(call_deadline, call()).await {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) if is_permanent(&e) => return Err(AcquireError::Rejected(e)),
            Ok(Err(e)) => warn!(what, error = %e, "Kubernetes API call failed, retrying"),
            Err(_) => warn!(what, "Kubernetes API call timed out, retrying"),
        }
        if Instant::now() + RETRY_BACKOFF >= deadline {
            return Err(AcquireError::Deadline);
        }
        tokio::time::sleep(RETRY_BACKOFF).await;
    }
}

pub async fn ensure_namespace(client: &Client) -> Result<(), kube::Error> {
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(NAMESPACE.to_string()),
            labels: Some(BTreeMap::from([(
                LABEL_MANAGED_BY.to_string(),
                MANAGED_BY.to_string(),
            )])),
            ..Default::default()
        },
        ..Default::default()
    };
    exists_ok(
        Api::<Namespace>::all(client.clone())
            .create(&PostParams::default(), &ns)
            .await,
    )
}

fn pod_condition(pod: &Pod, kind: &str) -> Option<(String, Option<String>, Option<String>)> {
    pod.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == kind)
        .map(|c| (c.status.clone(), c.reason.clone(), c.message.clone()))
}

fn is_scheduled(pod: &Pod) -> bool {
    matches!(pod_condition(pod, "PodScheduled"), Some((s, _, _)) if s == "True")
}

fn unschedulable_message(pod: &Pod) -> Option<String> {
    match pod_condition(pod, "PodScheduled")? {
        (s, Some(r), msg) if s == "False" && r == "Unschedulable" => Some(msg.unwrap_or(r)),
        _ => None,
    }
}

/// The allocated device names, in result order. Every result must be a
/// device of this node's `gpu.amd.com` pool; `None` while not allocated.
pub fn allocated_devices(
    claim: &ResourceClaim,
    node_name: &str,
) -> Result<Option<Vec<String>>, MappingError> {
    let Some(allocation) = claim.status.as_ref().and_then(|s| s.allocation.as_ref()) else {
        return Ok(None);
    };
    let results = allocation
        .devices
        .as_ref()
        .and_then(|d| d.results.as_ref())
        .map(Vec::as_slice)
        .unwrap_or_default();
    results
        .iter()
        .map(|r| {
            if r.driver == DRA_DRIVER && r.pool == node_name {
                Ok(r.device.clone())
            } else {
                Err(MappingError::ForeignDevice {
                    driver: r.driver.clone(),
                    pool: r.pool.clone(),
                    device: r.device.clone(),
                })
            }
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

/// Creates the claim and the placeholder pod and waits until Kubernetes has
/// allocated the claim and scheduled the pod. Returns the allocated DRA
/// device names. On any error the placeholder is deleted again.
pub async fn acquire(
    client: &Client,
    spec: &PlaceholderSpec,
    deadline: Instant,
) -> Result<Vec<String>, AcquireError> {
    let result = create_and_wait(client, spec, deadline).await;
    if let Err(e) = &result {
        warn!(job_id = spec.job_id, error = %e, "giving up the GPU placeholder");
        if let Err(e) = release_once(client, &spec.name()).await {
            // The orphan reconcile deletes it later.
            warn!(job_id = spec.job_id, error = %e, "placeholder delete failed");
        }
    }
    result
}

async fn create_and_wait(
    client: &Client,
    spec: &PlaceholderSpec,
    deadline: Instant,
) -> Result<Vec<String>, AcquireError> {
    let claims: Api<ResourceClaim> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let name = spec.name();
    let (claim, pod) = (build_claim(spec), build_pod(spec));
    let pp = PostParams::default();

    retry(deadline, "create namespace", || ensure_namespace(client)).await?;
    retry(deadline, "create claim", || async {
        exists_ok(claims.create(&pp, &claim).await)
    })
    .await?;
    retry(deadline, "create pod", || async {
        exists_ok(pods.create(&pp, &pod).await)
    })
    .await?;

    loop {
        let pod = retry(deadline, "get pod", || pods.get(&name)).await?;
        if let Some(msg) = unschedulable_message(&pod) {
            return Err(AcquireError::Unschedulable(msg));
        }
        if is_scheduled(&pod) {
            let claim = retry(deadline, "get claim", || claims.get(&name)).await?;
            if let Some(devices) = allocated_devices(&claim, &spec.node_name)? {
                debug!(job_id = spec.job_id, ?devices, "GPU placeholder allocated");
                return Ok(devices);
            }
        }
        if Instant::now() + POLL_INTERVAL >= deadline {
            return Err(AcquireError::Deadline);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn release_once(client: &Client, name: &str) -> Result<(), kube::Error> {
    let dp = DeleteParams::default();
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let claims: Api<ResourceClaim> = Api::namespaced(client.clone(), NAMESPACE);
    let pod = tokio::time::timeout(CALL_TIMEOUT, pods.delete(name, &dp));
    let claim = tokio::time::timeout(CALL_TIMEOUT, claims.delete(name, &dp));
    let (pod, claim) = tokio::join!(pod, claim);
    let timed_out = |what| kube::Error::Service(format!("delete {what} {name} timed out").into());
    not_found_ok(pod.map_err(|_| timed_out("pod"))?)?;
    not_found_ok(claim.map_err(|_| timed_out("claim"))?)
}

/// Deletes the placeholder of a job attempt on this node. A missing pod or
/// claim is not an error.
pub async fn release(
    client: &Client,
    job_id: u32,
    run_attempt: u32,
    node_name: &str,
) -> Result<(), kube::Error> {
    release_once(client, &placeholder_name(job_id, run_attempt, node_name)).await
}

fn job_of(meta: &ObjectMeta) -> Option<(u32, u32)> {
    let labels = meta.labels.as_ref()?;
    Some((
        labels.get(LABEL_JOB_ID)?.parse().ok()?,
        labels.get(LABEL_RUN_ATTEMPT)?.parse().ok()?,
    ))
}

/// Deletes the placeholders of this node whose (job id, run attempt) is not
/// live. `live` must also hold the attempts that are still launching. It is
/// read after the listing, so a placeholder a launch creates meanwhile is kept.
/// Returns the deleted names.
pub async fn reconcile_orphans(
    client: &Client,
    node_name: &str,
    live: impl FnOnce() -> HashSet<(u32, u32)>,
) -> Result<Vec<String>, kube::Error> {
    let lp = ListParams::default().labels(&format!(
        "{LABEL_MANAGED_BY}={MANAGED_BY},{LABEL_NODE}={node_name}"
    ));
    let pods = Api::<Pod>::namespaced(client.clone(), NAMESPACE)
        .list(&lp)
        .await?;
    let claims = Api::<ResourceClaim>::namespaced(client.clone(), NAMESPACE)
        .list(&lp)
        .await?;
    let live = live();
    let orphans: HashSet<String> = pods
        .items
        .iter()
        .map(|p| &p.metadata)
        .chain(claims.items.iter().map(|c| &c.metadata))
        .filter(|m| job_of(m).is_none_or(|job| !live.contains(&job)))
        .filter_map(|m| m.name.clone())
        .collect();
    let mut deleted: Vec<String> = orphans.into_iter().collect();
    deleted.sort();
    for name in &deleted {
        release_once(client, name).await?;
    }
    Ok(deleted)
}

/// Recreates a deleted claim or pod of a running job. `spec.gpus` must be the
/// devices the job runs on. One attempt per call; the caller calls again.
pub async fn ensure_present(
    client: &Client,
    spec: &PlaceholderSpec,
) -> Result<Presence, kube::Error> {
    let claims: Api<ResourceClaim> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let name = spec.name();
    let pp = PostParams::default();

    let claim = match claims.get_opt(&name).await? {
        Some(c) => c,
        None => {
            warn!(job_id = spec.job_id, "recreating the deleted GPU claim");
            exists_ok(claims.create(&pp, &build_claim(spec)).await)?;
            return Ok(Presence::Pending);
        }
    };
    let pod = match pods.get_opt(&name).await? {
        Some(p) => Some(p),
        None => {
            warn!(
                job_id = spec.job_id,
                "recreating the deleted GPU placeholder pod"
            );
            exists_ok(pods.create(&pp, &build_pod(spec)).await)?;
            None
        }
    };
    let allocated = match allocated_devices(&claim, &spec.node_name) {
        Ok(a) => a,
        Err(e) => return Ok(Presence::Conflict(e.to_string())),
    };
    let Some(allocated) = allocated else {
        return Ok(match pod.as_ref().and_then(unschedulable_message) {
            Some(msg) => Presence::Conflict(msg),
            None => Presence::Pending,
        });
    };
    let want: HashSet<&str> = spec.gpus.iter().map(|g| g.dra_device.as_str()).collect();
    let got: HashSet<&str> = allocated.iter().map(String::as_str).collect();
    if want == got {
        Ok(Presence::Held)
    } else {
        Ok(Presence::Conflict(format!(
            "claim holds {allocated:?}, the job runs on {want:?}"
        )))
    }
}

/// Maps the allocated device names back to devices of this node, in the
/// order of `spec.gpus`. A requested device that Kubernetes allocated keeps
/// its slot; another slot takes an allocated sibling with the same parent.
pub fn map_allocation(
    spec: &PlaceholderSpec,
    allocated: &[String],
    lookup: impl Fn(&str) -> Option<RequestedGpu>,
) -> Result<Vec<RequestedGpu>, MappingError> {
    let mut pool: Vec<Option<RequestedGpu>> = allocated
        .iter()
        .map(|name| {
            lookup(name)
                .map(Some)
                .ok_or_else(|| MappingError::UnknownDevice(name.clone()))
        })
        .collect::<Result<_, _>>()?;
    let mut take = |pred: &dyn Fn(&RequestedGpu) -> bool| {
        pool.iter_mut()
            .find(|slot| slot.as_ref().is_some_and(pred))
            .and_then(Option::take)
    };
    let mut slots: Vec<Option<RequestedGpu>> = spec
        .gpus
        .iter()
        .map(|want| take(&|got: &RequestedGpu| got.stable_id == want.stable_id))
        .collect();
    for (slot, want) in slots.iter_mut().zip(&spec.gpus) {
        if slot.is_none() {
            *slot = take(&|got: &RequestedGpu| got.parent_key == want.parent_key);
        }
    }
    let mismatch = || MappingError::Mismatch {
        requested: spec.gpus.iter().map(|g| g.dra_device.clone()).collect(),
        allocated: allocated.to_vec(),
    };
    if pool.iter().any(Option::is_some) {
        return Err(mismatch());
    }
    slots.into_iter().map(|s| s.ok_or_else(mismatch)).collect()
}

/// The `LaunchJobResponse.substituted_alloc` value: `None` when the chosen
/// devices are the requested ones, else `original` with its GPUs replaced.
pub fn substituted_alloc(
    original: &ResourceAllocations,
    chosen: &[RequestedGpu],
) -> Option<ResourceAllocations> {
    let requested: HashSet<u64> = original
        .devices
        .get("gpu")
        .map(|d| d.devices.iter().map(|dev| dev.device_id).collect())
        .unwrap_or_default();
    let chosen_ids: HashSet<u64> = chosen.iter().map(|g| g.stable_id).collect();
    if requested == chosen_ids {
        return None;
    }
    let mut alloc = original.clone();
    alloc.devices.insert(
        "gpu".to_string(),
        DeviceAllocations {
            devices: chosen
                .iter()
                .map(|g| AllocatedDevice {
                    device_id: g.stable_id,
                    count: 1,
                })
                .collect(),
        },
    );
    Some(alloc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use http::header::CONTENT_TYPE;
    use http::{Method, Request, Response, StatusCode};
    use kube::client::Body;
    use serde_json::{json, Value};

    const NODE: &str = "node-a";

    #[derive(Debug, Clone)]
    struct Seen {
        method: Method,
        path: String,
        query: String,
        body: Value,
    }

    enum Reply {
        Json(StatusCode, Value),
        Hang,
    }

    type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

    /// An API server whose answer to each request comes from a test closure,
    /// so a test can script sequences, errors and calls that never answer.
    #[derive(Clone)]
    struct ScriptedApi {
        handler: Arc<Handler>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl ScriptedApi {
        fn new(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
            Self {
                handler: Arc::new(handler),
                seen: Arc::default(),
            }
        }

        fn client(&self) -> Client {
            Client::new(self.clone(), "default")
        }

        fn calls(&self, method: Method, path: &str) -> Vec<Seen> {
            self.seen
                .lock()
                .expect("request log is not poisoned")
                .iter()
                .filter(|s| s.method == method && s.path == path)
                .cloned()
                .collect()
        }
    }

    impl tower::Service<Request<Body>> for ScriptedApi {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Infallible>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            let this = self.clone();
            Box::pin(async move {
                let (parts, body) = req.into_parts();
                let bytes = body.collect_bytes().await.expect("request body reads");
                let seen = Seen {
                    method: parts.method,
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or_default().replace("%2F", "/"),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                };
                this.seen
                    .lock()
                    .expect("request log is not poisoned")
                    .push(seen.clone());
                match (this.handler)(&seen) {
                    Reply::Json(status, v) => Ok(Response::builder()
                        .status(status)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(serde_json::to_vec(&v).expect("json")))
                        .expect("valid response")),
                    Reply::Hang => std::future::pending().await,
                }
            })
        }
    }

    fn ok(v: Value) -> Reply {
        Reply::Json(StatusCode::OK, v)
    }

    fn api_error(code: u16, reason: &str) -> Reply {
        Reply::Json(
            StatusCode::from_u16(code).expect("status code"),
            json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
                   "message": reason, "reason": reason, "code": code}),
        )
    }

    fn deleted() -> Reply {
        ok(json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success"}))
    }

    fn gpu(stable_id: u64, card: u32, render: u32, bdf: &str) -> RequestedGpu {
        RequestedGpu {
            stable_id,
            dra_device: format!("gpu-{card}-{render}"),
            selector_bdf: bdf.to_string(),
            parent_key: stable_id >> 11,
        }
    }

    fn spx_a() -> RequestedGpu {
        gpu(0x19 << 11, 1, 128, "0000:19:00.0")
    }

    fn spx_b() -> RequestedGpu {
        gpu(0x39 << 11, 2, 136, "0000:39:00.0")
    }

    fn cpx(k: u64) -> RequestedGpu {
        gpu(
            (0x59 << 11) | k,
            3 + k as u32,
            144 + k as u32,
            "0000:59:00.0",
        )
    }

    fn spec(gpus: Vec<RequestedGpu>) -> PlaceholderSpec {
        PlaceholderSpec {
            job_id: 42,
            run_attempt: 1,
            user: "alice".into(),
            account: "research".into(),
            node_name: NODE.into(),
            pause_image: PAUSE_IMAGE.into(),
            gpus,
        }
    }

    const NAME: &str = "spur-job-42-1-node-a";
    const NS_PATH: &str = "/api/v1/namespaces";
    const PODS: &str = "/api/v1/namespaces/spur-system/pods";
    const POD: &str = "/api/v1/namespaces/spur-system/pods/spur-job-42-1-node-a";
    const CLAIMS: &str = "/apis/resource.k8s.io/v1/namespaces/spur-system/resourceclaims";
    const CLAIM: &str =
        "/apis/resource.k8s.io/v1/namespaces/spur-system/resourceclaims/spur-job-42-1-node-a";

    fn meta(name: &str, job: u32, attempt: u32) -> Value {
        json!({"name": name, "namespace": NAMESPACE, "labels": {
            LABEL_MANAGED_BY: MANAGED_BY, LABEL_NODE: NODE,
            LABEL_JOB_ID: job.to_string(), LABEL_RUN_ATTEMPT: attempt.to_string()}})
    }

    fn pod_json(condition: Option<(&str, &str)>) -> Value {
        let conditions: Vec<Value> = condition
            .map(|(status, reason)| {
                json!({"type": "PodScheduled", "status": status, "reason": reason,
                       "message": "0/1 nodes are available: 1 cannot allocate all claims."})
            })
            .into_iter()
            .collect();
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": meta(NAME, 42, 1),
               "status": {"conditions": conditions}})
    }

    fn claim_json(results: &[(&str, &str, &str)]) -> Value {
        let mut claim = json!({"apiVersion": "resource.k8s.io/v1", "kind": "ResourceClaim",
                               "metadata": meta(NAME, 42, 1), "spec": {}});
        if !results.is_empty() {
            let results: Vec<Value> = results
                .iter()
                .map(|(driver, pool, device)| {
                    json!({"request": "g0", "driver": driver, "pool": pool, "device": device})
                })
                .collect();
            claim["status"] = json!({"allocation": {"devices": {"results": results}}});
        }
        claim
    }

    fn list_json(kind: &str, api_version: &str, names: &[(&str, u32, u32)]) -> Value {
        let items: Vec<Value> = names
            .iter()
            .map(|(n, job, attempt)| json!({"metadata": meta(n, *job, *attempt), "spec": {}}))
            .collect();
        json!({"apiVersion": api_version, "kind": kind, "metadata": {}, "items": items})
    }

    fn deadline(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn claim_has_one_request_per_whole_gpu_and_per_partitioned_parent() {
        let claim = serde_json::to_value(build_claim(&spec(vec![spx_b(), cpx(0), cpx(2)])))
            .expect("claim serializes");

        assert_eq!(claim["metadata"]["name"], NAME);
        assert_eq!(claim["metadata"]["namespace"], "spur-system");
        assert_eq!(claim["metadata"]["labels"][LABEL_JOB_ID], "42");
        assert_eq!(claim["metadata"]["labels"][LABEL_USER], "alice");
        assert_eq!(claim["metadata"]["labels"][LABEL_ACCOUNT], "research");
        assert_eq!(claim["metadata"]["labels"][LABEL_MANAGED_BY], "spurd");
        let requests = claim["spec"]["devices"]["requests"]
            .as_array()
            .expect("requests");
        assert_eq!(requests.len(), 2);
        let expected = [("g0", "0000:39:00.0", 1), ("g1", "0000:59:00.0", 2)];
        for (req, (name, bdf, count)) in requests.iter().zip(expected) {
            assert_eq!(req["name"], name);
            let exactly = &req["exactly"];
            assert_eq!(exactly["deviceClassName"], "gpu.amd.com");
            assert_eq!(exactly["allocationMode"], "ExactCount");
            assert_eq!(exactly["count"], count);
            assert_eq!(
                exactly["selectors"][0]["cel"]["expression"],
                format!("device.attributes[\"resource.kubernetes.io\"].pciBusID == \"{bdf}\"")
            );
        }
    }

    #[test]
    fn pod_pins_the_node_by_selector_and_references_the_claim() {
        let pod = serde_json::to_value(build_pod(&spec(vec![spx_a()]))).expect("pod serializes");

        let pod_spec = &pod["spec"];
        assert!(pod_spec.get("nodeName").is_none());
        assert_eq!(pod_spec["nodeSelector"]["kubernetes.io/hostname"], NODE);
        assert_eq!(pod_spec["resourceClaims"][0]["resourceClaimName"], NAME);
        assert_eq!(pod_spec["restartPolicy"], "Always");
        assert!(pod_spec.get("tolerations").is_none());
        let container = &pod_spec["containers"][0];
        assert_eq!(container["image"], PAUSE_IMAGE);
        assert_eq!(
            container["resources"],
            json!({"claims": [{"name": "gpus"}]})
        );
        assert_eq!(pod_spec["resourceClaims"][0]["name"], "gpus");
        assert_eq!(pod["metadata"]["labels"][LABEL_NODE], NODE);
    }

    #[test]
    fn invalid_label_values_are_left_out() {
        let mut s = spec(vec![spx_a()]);
        s.user = "alice@example.com".into();
        s.account = String::new();

        let labels = labels(&s);

        assert!(!labels.contains_key(LABEL_USER));
        assert_eq!(labels.get(LABEL_ACCOUNT).map(String::as_str), Some(""));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waits_until_scheduled_and_allocated() {
        let pod_gets = AtomicUsize::new(0);
        let api = ScriptedApi::new(move |s| match (s.method.as_str(), s.path.as_str()) {
            ("POST", NS_PATH) => api_error(409, "AlreadyExists"),
            ("POST", CLAIMS) | ("POST", PODS) => ok(s.body.clone()),
            ("GET", POD) if pod_gets.fetch_add(1, Ordering::SeqCst) == 0 => ok(pod_json(None)),
            ("GET", POD) => ok(pod_json(Some(("True", "")))),
            ("GET", CLAIM) => ok(claim_json(&[("gpu.amd.com", NODE, "gpu-1-128")])),
            _ => api_error(500, "unexpected"),
        });

        let devices = acquire(&api.client(), &spec(vec![spx_a()]), deadline(300))
            .await
            .expect("acquired");

        assert_eq!(devices, vec!["gpu-1-128"]);
        let claim_post = &api.calls(Method::POST, CLAIMS)[0].body;
        assert_eq!(
            claim_post["spec"]["devices"]["requests"][0]["exactly"]["count"],
            1
        );
        let pod_post = &api.calls(Method::POST, PODS)[0].body;
        assert_eq!(
            pod_post["spec"]["resourceClaims"][0]["resourceClaimName"],
            NAME
        );
        assert_eq!(
            pod_post["spec"]["nodeSelector"]["kubernetes.io/hostname"],
            NODE
        );
        assert!(pod_post["spec"].get("nodeName").is_none());
        assert_eq!(api.calls(Method::GET, POD).len(), 2);
        assert!(api.calls(Method::DELETE, POD).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn unschedulable_pod_ends_the_wait_and_deletes_the_placeholder() {
        let api = ScriptedApi::new(|s| match (s.method.as_str(), s.path.as_str()) {
            ("POST", _) => api_error(409, "AlreadyExists"),
            ("GET", POD) => ok(pod_json(Some(("False", "Unschedulable")))),
            ("DELETE", _) => deleted(),
            _ => api_error(500, "unexpected"),
        });
        let start = Instant::now();

        let err = acquire(&api.client(), &spec(vec![spx_a()]), deadline(300))
            .await
            .expect_err("unschedulable");

        assert!(matches!(err, AcquireError::Unschedulable(_)), "{err:?}");
        assert!(Instant::now() - start < Duration::from_secs(1));
        assert_eq!(api.calls(Method::DELETE, POD).len(), 1);
        assert_eq!(api.calls(Method::DELETE, CLAIM).len(), 1);
        assert_eq!(
            tonic::Status::from(err).code(),
            tonic::Code::ResourceExhausted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_and_failing_api_calls_are_retried() {
        let claim_posts = AtomicUsize::new(0);
        let pod_gets = AtomicUsize::new(0);
        let api = ScriptedApi::new(move |s| match (s.method.as_str(), s.path.as_str()) {
            ("POST", NS_PATH) => ok(json!({"apiVersion": "v1", "kind": "Namespace",
                                           "metadata": {"name": NAMESPACE}})),
            ("POST", CLAIMS) if claim_posts.fetch_add(1, Ordering::SeqCst) == 0 => {
                api_error(500, "InternalError")
            }
            ("POST", CLAIMS) | ("POST", PODS) => ok(s.body.clone()),
            ("GET", POD) if pod_gets.fetch_add(1, Ordering::SeqCst) == 0 => Reply::Hang,
            ("GET", POD) => ok(pod_json(Some(("True", "")))),
            ("GET", CLAIM) => ok(claim_json(&[("gpu.amd.com", NODE, "gpu-1-128")])),
            _ => api_error(500, "unexpected"),
        });

        let devices = acquire(&api.client(), &spec(vec![spx_a()]), deadline(300))
            .await
            .expect("acquired after retries");

        assert_eq!(devices, vec!["gpu-1-128"]);
        assert_eq!(api.calls(Method::POST, CLAIMS).len(), 2);
        assert_eq!(api.calls(Method::GET, POD).len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_gives_up_and_deletes_the_placeholder() {
        let api = ScriptedApi::new(|s| match (s.method.as_str(), s.path.as_str()) {
            ("POST", _) => api_error(409, "AlreadyExists"),
            ("GET", POD) => ok(pod_json(None)),
            ("DELETE", _) => api_error(404, "NotFound"),
            _ => api_error(500, "unexpected"),
        });

        let err = acquire(&api.client(), &spec(vec![spx_a()]), deadline(5))
            .await
            .expect_err("deadline");

        assert!(matches!(err, AcquireError::Deadline), "{err:?}");
        assert!(api.calls(Method::GET, POD).len() >= 5);
        assert_eq!(api.calls(Method::DELETE, POD).len(), 1);
        assert_eq!(api.calls(Method::DELETE, CLAIM).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_or_unauthorized_placeholder_is_rejected_without_retry() {
        for (code, reason) in [(422, "Invalid"), (401, "Unauthorized")] {
            let api = ScriptedApi::new(move |s| match (s.method.as_str(), s.path.as_str()) {
                ("POST", CLAIMS) => api_error(code, reason),
                ("POST", _) => api_error(409, "AlreadyExists"),
                ("DELETE", _) => api_error(404, "NotFound"),
                _ => api_error(500, "unexpected"),
            });

            let err = acquire(&api.client(), &spec(vec![spx_a()]), deadline(300))
                .await
                .expect_err("rejected");

            assert!(matches!(err, AcquireError::Rejected(_)), "{err:?}");
            assert_eq!(api.calls(Method::POST, CLAIMS).len(), 1);
            assert_eq!(api.calls(Method::DELETE, CLAIM).len(), 1);
        }
    }

    #[test]
    fn allocated_devices_rejects_a_device_of_another_driver_or_node() {
        let parse = |v: Value| serde_json::from_value::<ResourceClaim>(v).expect("claim");

        assert_eq!(allocated_devices(&parse(claim_json(&[])), NODE), Ok(None));
        assert_eq!(
            allocated_devices(
                &parse(claim_json(&[("gpu.amd.com", NODE, "gpu-1-128")])),
                NODE
            ),
            Ok(Some(vec!["gpu-1-128".to_string()]))
        );
        for (driver, pool) in [("gpu.nvidia.com", NODE), ("gpu.amd.com", "node-b")] {
            let claim = parse(claim_json(&[(driver, pool, "gpu-1-128")]));
            assert!(matches!(
                allocated_devices(&claim, NODE),
                Err(MappingError::ForeignDevice { .. })
            ));
        }
    }

    #[tokio::test]
    async fn reconcile_orphans_deletes_only_attempts_that_are_not_live() {
        let api = ScriptedApi::new(|s| match (s.method.as_str(), s.path.as_str()) {
            ("GET", PODS) => ok(list_json(
                "PodList",
                "v1",
                &[("spur-job-1-0-node-a", 1, 0), ("spur-job-2-0-node-a", 2, 0)],
            )),
            ("GET", CLAIMS) => ok(list_json(
                "ResourceClaimList",
                "resource.k8s.io/v1",
                &[("spur-job-2-0-node-a", 2, 0), ("spur-job-3-0-node-a", 3, 0)],
            )),
            ("DELETE", _) => deleted(),
            _ => api_error(500, "unexpected"),
        });

        let deleted = reconcile_orphans(&api.client(), NODE, || HashSet::from([(1, 0)]))
            .await
            .expect("reconciled");

        assert_eq!(deleted, vec!["spur-job-2-0-node-a", "spur-job-3-0-node-a"]);
        let pods_list = &api.calls(Method::GET, PODS)[0];
        assert!(
            pods_list
                .query
                .contains("app.kubernetes.io/managed-by%3Dspurd%2Cspur.amd.com/node%3Dnode-a"),
            "{}",
            pods_list.query
        );
        assert_eq!(
            api.calls(Method::DELETE, &format!("{PODS}/spur-job-2-0-node-a"))
                .len(),
            1
        );
        assert_eq!(
            api.calls(Method::DELETE, &format!("{CLAIMS}/spur-job-3-0-node-a"))
                .len(),
            1
        );
        assert!(api
            .calls(Method::DELETE, &format!("{PODS}/spur-job-1-0-node-a"))
            .is_empty());
    }

    #[tokio::test]
    async fn release_tolerates_an_already_deleted_placeholder() {
        let api = ScriptedApi::new(|_| api_error(404, "NotFound"));

        release(&api.client(), 42, 1, NODE).await.expect("released");

        assert_eq!(api.calls(Method::DELETE, POD).len(), 1);
        assert_eq!(api.calls(Method::DELETE, CLAIM).len(), 1);
    }

    #[tokio::test]
    async fn ensure_present_recreates_a_deleted_pod_and_reports_the_hold() {
        let api = ScriptedApi::new(|s| match (s.method.as_str(), s.path.as_str()) {
            ("GET", CLAIM) => ok(claim_json(&[("gpu.amd.com", NODE, "gpu-1-128")])),
            ("GET", POD) => api_error(404, "NotFound"),
            ("POST", PODS) => ok(s.body.clone()),
            _ => api_error(500, "unexpected"),
        });

        let held = ensure_present(&api.client(), &spec(vec![spx_a()])).await;
        let lost = ensure_present(&api.client(), &spec(vec![spx_b()])).await;

        assert_eq!(held.expect("checked"), Presence::Held);
        assert!(matches!(lost.expect("checked"), Presence::Conflict(_)));
        assert_eq!(api.calls(Method::POST, PODS).len(), 2);
    }

    #[tokio::test]
    async fn ensure_present_recreates_a_deleted_claim() {
        let api = ScriptedApi::new(|s| match (s.method.as_str(), s.path.as_str()) {
            ("GET", CLAIM) => api_error(404, "NotFound"),
            ("POST", CLAIMS) => ok(s.body.clone()),
            _ => api_error(500, "unexpected"),
        });

        let presence = ensure_present(&api.client(), &spec(vec![spx_a()]))
            .await
            .expect("checked");

        assert_eq!(presence, Presence::Pending);
        assert_eq!(api.calls(Method::POST, CLAIMS).len(), 1);
    }

    fn inventory() -> Vec<RequestedGpu> {
        vec![spx_a(), spx_b(), cpx(0), cpx(1), cpx(2), cpx(3)]
    }

    fn lookup(name: &str) -> Option<RequestedGpu> {
        inventory().into_iter().find(|g| g.dra_device == name)
    }

    fn names(gpus: &[RequestedGpu]) -> Vec<String> {
        gpus.iter().map(|g| g.dra_device.clone()).collect()
    }

    #[test]
    fn spx_allocation_must_equal_the_request() {
        let s = spec(vec![spx_a(), spx_b()]);

        let exact = map_allocation(&s, &names(&[spx_b(), spx_a()]), lookup);
        let other = map_allocation(&s, &names(&[spx_a(), cpx(0)]), lookup);

        assert_eq!(exact, Ok(vec![spx_a(), spx_b()]));
        assert!(matches!(other, Err(MappingError::Mismatch { .. })));
    }

    #[test]
    fn cpx_allocation_may_pick_other_siblings_of_the_parent() {
        let s = spec(vec![cpx(0), cpx(1)]);

        let chosen = map_allocation(&s, &names(&[cpx(3), cpx(0)]), lookup).expect("mapped");

        assert_eq!(chosen, vec![cpx(0), cpx(3)]);
    }

    #[test]
    fn allocation_with_an_unknown_or_extra_device_is_rejected() {
        let s = spec(vec![cpx(0)]);

        let unknown = map_allocation(&s, &["gpu-9-200".to_string()], lookup);
        let extra = map_allocation(&s, &names(&[cpx(0), cpx(1)]), lookup);

        assert_eq!(
            unknown,
            Err(MappingError::UnknownDevice("gpu-9-200".into()))
        );
        assert!(matches!(extra, Err(MappingError::Mismatch { .. })));
    }

    fn alloc_of(gpus: &[RequestedGpu]) -> ResourceAllocations {
        let devices = gpus
            .iter()
            .map(|g| AllocatedDevice {
                device_id: g.stable_id,
                count: 1,
            })
            .collect();
        ResourceAllocations {
            cpus: 4,
            memory_mb: 1024,
            devices: [("gpu".to_string(), DeviceAllocations { devices })].into(),
            generation: 7,
        }
    }

    #[test]
    fn substituted_alloc_is_set_only_when_the_devices_differ() {
        let original = alloc_of(&[cpx(0), cpx(1)]);

        let same = substituted_alloc(&original, &[cpx(1), cpx(0)]);
        let moved = substituted_alloc(&original, &[cpx(0), cpx(3)]);

        assert_eq!(same, None);
        assert_eq!(moved, Some(alloc_of(&[cpx(0), cpx(3)])));
    }
}
