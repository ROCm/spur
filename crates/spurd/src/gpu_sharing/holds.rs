// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Who holds each GPU of a shared node, computed from the node's
//! `ResourceClaim`s and `ResourceSlice`s. Pure: no API access here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use k8s_openapi::api::resource::v1::{ResourceClaim, ResourceSlice};
use spur_devices::cdi::SharingIdentity;
use spur_proto::proto::{GpuHold, GpuHoldReport, GpuHoldState};

pub const DRA_DRIVER: &str = "gpu.amd.com";
/// Claim label, set by the bridge, that names the Spur job a pod's GPUs belong to.
pub const JOB_ID_LABEL: &str = "spur.amd.com/job-id";

/// The allocated claim that names one DRA device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimHolder {
    pub namespace: String,
    pub claim: String,
    /// First pod the claim is reserved for; empty before the scheduler reserves it.
    pub pod: String,
    /// The Spur job of the claim's `spur.amd.com/job-id` label.
    pub owner_job: Option<u32>,
}

/// What Kubernetes says about this node's GPUs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct K8sView {
    /// Device names the DRA driver publishes for this node; `None` without a slice.
    pub published: Option<BTreeSet<String>>,
    /// Allocated device name -> the claim holding it.
    pub holders: BTreeMap<String, ClaimHolder>,
}

impl K8sView {
    pub fn from_objects<'a>(
        claims: impl IntoIterator<Item = &'a ResourceClaim>,
        slices: impl IntoIterator<Item = &'a ResourceSlice>,
        node: &str,
    ) -> Self {
        let mut published: Option<BTreeSet<String>> = None;
        for slice in slices {
            let spec = &slice.spec;
            if spec.driver != DRA_DRIVER || spec.node_name.as_deref() != Some(node) {
                continue;
            }
            let names = published.get_or_insert_with(BTreeSet::new);
            names.extend(spec.devices.iter().flatten().map(|d| d.name.clone()));
        }

        let mut holders = BTreeMap::new();
        for claim in claims {
            let Some(holder) = claim_holder(claim) else {
                continue;
            };
            for device in allocated_devices(claim, node) {
                holders.insert(device, holder.clone());
            }
        }
        Self { published, holders }
    }
}

fn claim_holder(claim: &ResourceClaim) -> Option<ClaimHolder> {
    let owner_job = claim
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(JOB_ID_LABEL))
        .and_then(|v| v.parse().ok());
    let pod = claim
        .status
        .as_ref()?
        .reserved_for
        .iter()
        .flatten()
        .find(|r| r.resource == "pods")
        .map(|r| r.name.clone())
        .unwrap_or_default();
    Some(ClaimHolder {
        namespace: claim.metadata.namespace.clone().unwrap_or_default(),
        claim: claim.metadata.name.clone().unwrap_or_default(),
        pod,
        owner_job,
    })
}

/// Devices of `gpu.amd.com` on `node` the claim is allocated. A hold is
/// effective from allocation, before any pod starts.
fn allocated_devices(claim: &ResourceClaim, node: &str) -> Vec<String> {
    claim
        .status
        .as_ref()
        .and_then(|s| s.allocation.as_ref())
        .and_then(|a| a.devices.as_ref())
        .and_then(|d| d.results.as_ref())
        .into_iter()
        .flatten()
        .filter(|r| r.driver == DRA_DRIVER && r.pool == node)
        .map(|r| r.device.clone())
        .collect()
}

/// The full hold state of `inventory` (the reported GPU stable_ids), as the
/// heartbeat carries it. `job_gpus` maps each running Spur job to its GPUs.
pub fn build_report(
    generation: u64,
    inventory: &[u64],
    identities: &HashMap<u64, SharingIdentity>,
    view: &K8sView,
    job_gpus: &HashMap<u32, Vec<u64>>,
    node: &str,
) -> GpuHoldReport {
    let Some(published) = &view.published else {
        return GpuHoldReport {
            generation,
            gpus: Vec::new(),
            unshareable_reason: format!("no ResourceSlice from {DRA_DRIVER} for node {node}"),
        };
    };
    let job_on = |sid: u64| {
        job_gpus
            .iter()
            .find(|(_, gpus)| gpus.contains(&sid))
            .map(|(&job, _)| job)
    };
    let gpus = inventory
        .iter()
        .map(|&sid| gpu_hold(sid, identities.get(&sid), published, view, job_on(sid)))
        .collect();
    GpuHoldReport {
        generation,
        gpus,
        unshareable_reason: String::new(),
    }
}

fn gpu_hold(
    stable_id: u64,
    identity: Option<&SharingIdentity>,
    published: &BTreeSet<String>,
    view: &K8sView,
    spur_job: Option<u32>,
) -> GpuHold {
    let unshareable = |reason: String, dra_device: String| GpuHold {
        stable_id,
        state: GpuHoldState::GpuHoldUnshareable as i32,
        reason,
        dra_device,
        ..Default::default()
    };
    let Some(identity) = identity else {
        return unshareable("not a KFD device".into(), String::new());
    };
    let Some(device) = identity.dra_device_name() else {
        return unshareable(
            identity.unshareable_reason().unwrap_or_default(),
            String::new(),
        );
    };
    if !published.contains(&device) {
        return unshareable(
            format!("{device} is not in the {DRA_DRIVER} ResourceSlice"),
            device,
        );
    }
    let Some(holder) = view.holders.get(&device) else {
        let state = match spur_job {
            Some(_) => GpuHoldState::GpuHoldSpurJob,
            None => GpuHoldState::GpuHoldFree,
        };
        return GpuHold {
            stable_id,
            state: state as i32,
            job_id: spur_job.unwrap_or_default(),
            dra_device: device,
            ..Default::default()
        };
    };
    let (state, job_id) = match spur_job {
        Some(job) if holder.owner_job != Some(job) => (GpuHoldState::GpuHoldConflict, job),
        _ => (
            GpuHoldState::GpuHoldK8s,
            holder.owner_job.unwrap_or_default(),
        ),
    };
    GpuHold {
        stable_id,
        state: state as i32,
        job_id,
        pod_namespace: holder.namespace.clone(),
        pod_name: holder.pod.clone(),
        claim_name: holder.claim.clone(),
        reason: String::new(),
        dra_device: device,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "gpu-node-1";

    fn slice(node: &str, driver: &str, devices: &[&str]) -> ResourceSlice {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": format!("{node}-{driver}")},
            "spec": {
                "driver": driver,
                "nodeName": node,
                "pool": {"name": node, "generation": 1, "resourceSliceCount": 1},
                "devices": devices.iter().map(|d| serde_json::json!({"name": d})).collect::<Vec<_>>(),
            }
        }))
        .unwrap()
    }

    fn claim(
        namespace: &str,
        name: &str,
        labels: serde_json::Value,
        pool: &str,
        devices: &[&str],
        pod: Option<&str>,
    ) -> ResourceClaim {
        let results: Vec<_> = devices
            .iter()
            .map(|d| serde_json::json!({"request": "gpu", "driver": DRA_DRIVER, "pool": pool, "device": d}))
            .collect();
        let reserved: Vec<_> = pod
            .map(|p| serde_json::json!({"resource": "pods", "name": p, "uid": "u1"}))
            .into_iter()
            .collect();
        serde_json::from_value(serde_json::json!({
            "metadata": {"namespace": namespace, "name": name, "labels": labels},
            "spec": {},
            "status": {"allocation": {"devices": {"results": results}}, "reservedFor": reserved},
        }))
        .unwrap()
    }

    fn identity(stable_id: u64, card: Option<u32>, render_minor: u32) -> SharingIdentity {
        SharingIdentity {
            stable_id,
            render_minor,
            card_id: card,
        }
    }

    fn identities() -> HashMap<u64, SharingIdentity> {
        [
            identity(1, Some(1), 128),
            identity(2, Some(9), 136),
            identity(3, Some(17), 144),
            identity(4, Some(25), 152),
            identity(5, None, 160),
        ]
        .into_iter()
        .map(|i| (i.stable_id, i))
        .collect()
    }

    fn state(report: &GpuHoldReport, sid: u64) -> (GpuHoldState, &GpuHold) {
        let hold = report.gpus.iter().find(|g| g.stable_id == sid).unwrap();
        (GpuHoldState::try_from(hold.state).unwrap(), hold)
    }

    #[test]
    fn holds_classify_k8s_conflict_and_unshareable() {
        let slices = [
            slice(NODE, DRA_DRIVER, &["gpu-1-128", "gpu-9-136", "gpu-17-144"]),
            slice("other-node", DRA_DRIVER, &["gpu-25-152"]),
            slice(NODE, "gpu.nvidia.com", &["gpu-25-152"]),
        ];
        let claims = [
            claim(
                "team",
                "infer",
                serde_json::json!({}),
                NODE,
                &["gpu-9-136"],
                Some("infer-0"),
            ),
            claim(
                "team",
                "serve",
                serde_json::json!({JOB_ID_LABEL: "7"}),
                NODE,
                &["gpu-1-128"],
                Some("serve-0"),
            ),
            claim(
                "team",
                "train",
                serde_json::json!({}),
                NODE,
                &["gpu-17-144"],
                None,
            ),
            claim(
                "team",
                "elsewhere",
                serde_json::json!({}),
                "other-node",
                &["gpu-25-152"],
                None,
            ),
        ];
        let view = K8sView::from_objects(&claims, &slices, NODE);
        let job_gpus = HashMap::from([(7, vec![1]), (9, vec![3])]);

        let report = build_report(
            42,
            &[1, 2, 3, 4, 5, 6],
            &identities(),
            &view,
            &job_gpus,
            NODE,
        );

        assert_eq!(report.generation, 42);
        assert!(report.unshareable_reason.is_empty());
        assert_eq!(report.gpus.len(), 6);

        let (s, g) = state(&report, 1);
        assert_eq!(
            (s, g.job_id, g.claim_name.as_str()),
            (GpuHoldState::GpuHoldK8s, 7, "serve")
        );
        let (s, g) = state(&report, 2);
        assert_eq!((s, g.job_id), (GpuHoldState::GpuHoldK8s, 0));
        assert_eq!(
            (
                g.pod_namespace.as_str(),
                g.pod_name.as_str(),
                g.claim_name.as_str()
            ),
            ("team", "infer-0", "infer")
        );
        assert_eq!(g.dra_device, "gpu-9-136");
        let (s, g) = state(&report, 3);
        assert_eq!(
            (s, g.job_id, g.claim_name.as_str(), g.pod_name.as_str()),
            (GpuHoldState::GpuHoldConflict, 9, "train", "")
        );
        let (s, g) = state(&report, 4);
        assert_eq!(s, GpuHoldState::GpuHoldUnshareable);
        assert_eq!(
            g.reason,
            "gpu-25-152 is not in the gpu.amd.com ResourceSlice"
        );
        let (s, g) = state(&report, 5);
        assert_eq!(
            (s, g.reason.as_str()),
            (
                GpuHoldState::GpuHoldUnshareable,
                "no DRM card for renderD160"
            )
        );
        let (s, g) = state(&report, 6);
        assert_eq!(
            (s, g.reason.as_str()),
            (GpuHoldState::GpuHoldUnshareable, "not a KFD device")
        );
    }

    #[test]
    fn claim_label_names_the_owner_job_of_a_pod_gpu() {
        let slices = [slice(
            NODE,
            DRA_DRIVER,
            &["gpu-1-128", "gpu-9-136", "gpu-17-144", "gpu-25-152"],
        )];
        let owned_by = |name: &str, job: &str, device: &str| {
            claim(
                "team",
                name,
                serde_json::json!({JOB_ID_LABEL: job}),
                NODE,
                &[device],
                Some(name),
            )
        };
        let claims = [
            owned_by("same-job", "7", "gpu-1-128"),
            owned_by("job-gone", "8", "gpu-9-136"),
            owned_by("other-job", "9", "gpu-17-144"),
        ];
        let view = K8sView::from_objects(&claims, &slices, NODE);
        let job_gpus = HashMap::from([(7, vec![1]), (4, vec![3]), (5, vec![4])]);

        let report = build_report(1, &[1, 2, 3, 4], &identities(), &view, &job_gpus, NODE);

        let owner = |sid| {
            let (s, g) = state(&report, sid);
            (s, g.job_id, g.claim_name.clone())
        };
        assert_eq!(owner(1), (GpuHoldState::GpuHoldK8s, 7, "same-job".into()));
        assert_eq!(owner(2), (GpuHoldState::GpuHoldK8s, 8, "job-gone".into()));
        assert_eq!(
            owner(3),
            (GpuHoldState::GpuHoldConflict, 4, "other-job".into())
        );
        assert_eq!(owner(4), (GpuHoldState::GpuHoldSpurJob, 5, String::new()));
    }

    #[test]
    fn free_gpu_has_no_holder() {
        let view = K8sView::from_objects(&[], &[slice(NODE, DRA_DRIVER, &["gpu-1-128"])], NODE);
        let report = build_report(1, &[1], &identities(), &view, &HashMap::new(), NODE);
        let (s, g) = state(&report, 1);
        assert_eq!(
            (s, g.dra_device.as_str()),
            (GpuHoldState::GpuHoldFree, "gpu-1-128")
        );
    }

    #[test]
    fn unallocated_claim_holds_nothing() {
        let mut pending = claim("team", "pending", serde_json::json!({}), NODE, &[], None);
        pending.status = None;
        let view =
            K8sView::from_objects(&[pending], &[slice(NODE, DRA_DRIVER, &["gpu-1-128"])], NODE);
        assert!(view.holders.is_empty());
    }

    #[test]
    fn node_without_driver_slice_is_unshareable() {
        let view = K8sView::from_objects(
            &[],
            &[slice("other-node", DRA_DRIVER, &["gpu-1-128"])],
            NODE,
        );
        let report = build_report(3, &[1, 2], &identities(), &view, &HashMap::new(), NODE);
        assert_eq!(report.generation, 3);
        assert!(report.gpus.is_empty());
        assert_eq!(
            report.unshareable_reason,
            "no ResourceSlice from gpu.amd.com for node gpu-node-1"
        );
    }
}
