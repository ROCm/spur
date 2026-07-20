// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Projects a SPUR account's allocation into the native Kubernetes objects that ENFORCE it:
//! a Namespace (tenancy), a ResourceQuota (hard caps from the account's `grp_tres` allocation),
//! a LimitRange (default requests so unset-request pods can't dodge the quota), and RBAC (a Role
//! + a RoleBinding to the account's members). Pure — no I/O — so the whole mapping is unit-tested
//! here; the quota controller applies what these return and drift-corrects.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    LimitRange, LimitRangeItem, LimitRangeSpec, Namespace, ResourceQuota, ResourceQuotaSpec,
};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use spur_core::accounting::{TresRecord, TresType};
// Namespace + ServiceAccount naming lives in spur-core so spurctld's `kubeconfig --user` path agrees
// with what this reconciler creates.
use spur_core::quota_names::sanitize_dns_label;
pub use spur_core::quota_names::{account_namespace, user_service_account};

/// Value of the `app.kubernetes.io/managed-by` label stamped on every object this reconciler owns.
/// The controller finds + drift-corrects its objects by this label and it encodes the
/// "SPUR-managed" contract (an admin hand-edit is reverted).
pub const MANAGED_BY: &str = "spur-quota";

/// Name of the (single) ResourceQuota / LimitRange / Role per account namespace.
const QUOTA_NAME: &str = "spur-account-quota";
const LIMITS_NAME: &str = "spur-account-defaults";
const ROLE_NAME: &str = "spur-account-editor";
const BINDING_NAME: &str = "spur-account-members";

/// A SPUR account's projected allocation. The controller builds this from `ListAccounts` (the
/// account's `grp_tres` allocation) joined with `ListUsers` (its member users).
#[derive(Debug, Clone)]
pub struct AccountQuota {
    /// SPUR account name (e.g. "physics").
    pub account: String,
    /// The account's resource allocation. Only Cpu/Memory/Gpu map to a ResourceQuota; a 0/unset
    /// dimension is left uncapped (a 0 cap would block every pod).
    pub grp_tres: TresRecord,
    /// Users associated with the account. Each becomes a RoleBinding subject via their per-namespace
    /// ServiceAccount (minted by `spur k8s kubeconfig --user`).
    pub members: Vec<String>,
}

fn managed_labels(account: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            MANAGED_BY.to_string(),
        ),
        (
            "spur.amd.com/account".to_string(),
            sanitize_dns_label(account),
        ),
    ])
}

fn meta(name: &str, namespace: Option<&str>, account: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: namespace.map(str::to_string),
        labels: Some(managed_labels(account)),
        ..Default::default()
    }
}

/// Map the account allocation to ResourceQuota `hard` entries. CPU (cores) and memory (MB) are
/// capped on both requests and limits; GPUs go on `requests.amd.com/gpu` (an extended resource). A
/// dimension left at 0 is omitted (uncapped). Node/Energy/Billing have no pod-level quota analog.
pub fn quota_hard(grp_tres: &TresRecord) -> BTreeMap<String, Quantity> {
    let mut hard = BTreeMap::new();
    let cpu = grp_tres.get(TresType::Cpu);
    if cpu > 0 {
        hard.insert("requests.cpu".into(), Quantity(cpu.to_string()));
        hard.insert("limits.cpu".into(), Quantity(cpu.to_string()));
    }
    let mem_mb = grp_tres.get(TresType::Memory);
    if mem_mb > 0 {
        // TRES mem is base-10 MB; `M` (not `Mi`) keeps the quota equal to the allocation.
        hard.insert("requests.memory".into(), Quantity(format!("{mem_mb}M")));
        hard.insert("limits.memory".into(), Quantity(format!("{mem_mb}M")));
    }
    let gpu = grp_tres.get(TresType::Gpu);
    if gpu > 0 {
        hard.insert("requests.amd.com/gpu".into(), Quantity(gpu.to_string()));
    }
    hard
}

/// The account's Namespace.
pub fn namespace(account: &str) -> Namespace {
    Namespace {
        metadata: meta(&account_namespace(account), None, account),
        ..Default::default()
    }
}

/// The account's ResourceQuota (hard caps from its allocation).
pub fn resource_quota(aq: &AccountQuota) -> ResourceQuota {
    let ns = account_namespace(&aq.account);
    ResourceQuota {
        metadata: meta(QUOTA_NAME, Some(&ns), &aq.account),
        spec: Some(ResourceQuotaSpec {
            hard: Some(quota_hard(&aq.grp_tres)),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A LimitRange giving every container a default *request*, so a pod that omits requests still
/// counts against the ResourceQuota (otherwise unset-request pods dodge the cap). Deliberately no
/// default *limit*: forcing an arbitrary small limit would reject ordinary larger pods that omit
/// limits — the account's ResourceQuota is what actually bounds usage.
pub fn limit_range(account: &str) -> LimitRange {
    let ns = account_namespace(account);
    let default_request = BTreeMap::from([
        ("cpu".to_string(), Quantity("100m".into())),
        ("memory".to_string(), Quantity("128Mi".into())),
    ]);
    LimitRange {
        metadata: meta(LIMITS_NAME, Some(&ns), account),
        spec: Some(LimitRangeSpec {
            limits: vec![LimitRangeItem {
                type_: "Container".to_string(),
                default_request: Some(default_request),
                ..Default::default()
            }],
        }),
    }
}

/// A namespace-scoped Role granting the account's members ordinary workload management (no cluster
/// resources, no quota/RBAC self-editing — those stay SPUR-managed).
pub fn role(account: &str) -> Role {
    let ns = account_namespace(account);
    let rule = |api_groups: &[&str], resources: &[&str], verbs: &[&str]| PolicyRule {
        api_groups: Some(api_groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.iter().map(|s| s.to_string()).collect()),
        verbs: verbs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let rw = &["get", "list", "watch", "create", "update", "patch", "delete"];
    Role {
        metadata: meta(ROLE_NAME, Some(&ns), account),
        rules: Some(vec![
            rule(
                &[""],
                &[
                    "pods",
                    "pods/log",
                    "pods/exec",
                    "pods/attach",
                    "pods/portforward",
                    "services",
                    "configmaps",
                    "secrets",
                    "persistentvolumeclaims",
                ],
                rw,
            ),
            rule(&[""], &["events"], &["get", "list", "watch"]),
            rule(&["batch"], &["jobs", "cronjobs"], rw),
            rule(
                &["apps"],
                &["deployments", "replicasets", "statefulsets", "daemonsets"],
                rw,
            ),
        ]),
    }
}

/// The RoleBinding granting the account Role to each member's per-namespace ServiceAccount.
pub fn role_binding(aq: &AccountQuota) -> RoleBinding {
    let ns = account_namespace(&aq.account);
    let subjects: Vec<Subject> = aq
        .members
        .iter()
        .map(|user| Subject {
            kind: "ServiceAccount".to_string(),
            name: user_service_account(user),
            namespace: Some(ns.clone()),
            api_group: None,
        })
        .collect();
    RoleBinding {
        metadata: meta(BINDING_NAME, Some(&ns), &aq.account),
        role_ref: RoleRef {
            api_group: Some("rbac.authorization.k8s.io".to_string()),
            kind: "Role".to_string(),
            name: ROLE_NAME.to_string(),
        },
        subjects: Some(subjects),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tres(cpu: u64, mem_mb: u64, gpu: u64) -> TresRecord {
        let mut t = TresRecord::new();
        if cpu > 0 {
            t.set(TresType::Cpu, cpu);
        }
        if mem_mb > 0 {
            t.set(TresType::Memory, mem_mb);
        }
        if gpu > 0 {
            t.set(TresType::Gpu, gpu);
        }
        t
    }

    #[test]
    fn quota_hard_maps_cpu_mem_gpu() {
        let h = quota_hard(&tres(16, 32768, 8));
        assert_eq!(h["requests.cpu"].0, "16");
        assert_eq!(h["limits.cpu"].0, "16");
        assert_eq!(h["requests.memory"].0, "32768M");
        assert_eq!(h["limits.memory"].0, "32768M");
        assert_eq!(h["requests.amd.com/gpu"].0, "8");
    }

    #[test]
    fn quota_hard_omits_zero_dimensions() {
        // GPU-only allocation: cpu/mem uncapped (no key), gpu capped.
        let h = quota_hard(&tres(0, 0, 4));
        assert!(!h.contains_key("requests.cpu"));
        assert!(!h.contains_key("requests.memory"));
        assert_eq!(h["requests.amd.com/gpu"].0, "4");
        // Node/Energy/Billing never map even when set.
        let mut t = TresRecord::new();
        t.set(TresType::Node, 3);
        t.set(TresType::Billing, 100);
        assert!(quota_hard(&t).is_empty());
    }

    #[test]
    fn resource_quota_carries_hard_caps_and_managed_label() {
        let aq = AccountQuota {
            account: "physics".into(),
            grp_tres: tres(16, 1024, 2),
            members: vec![],
        };
        let rq = resource_quota(&aq);
        assert_eq!(rq.metadata.namespace.as_deref(), Some("spur-acct-physics"));
        assert_eq!(rq.metadata.name.as_deref(), Some(QUOTA_NAME));
        assert_eq!(
            rq.metadata.labels.as_ref().unwrap()["app.kubernetes.io/managed-by"],
            MANAGED_BY
        );
        let hard = rq.spec.unwrap().hard.unwrap();
        assert_eq!(hard["requests.amd.com/gpu"].0, "2");
    }

    #[test]
    fn role_binding_has_a_service_account_subject_per_member() {
        let aq = AccountQuota {
            account: "physics".into(),
            grp_tres: tres(1, 0, 0),
            members: vec!["alice".into(), "bob".into()],
        };
        let rb = role_binding(&aq);
        let subs = rb.subjects.unwrap();
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|s| s.kind == "ServiceAccount"
            && s.namespace.as_deref() == Some("spur-acct-physics")));
        assert_eq!(subs[0].name, "spur-user-alice");
        assert_eq!(rb.role_ref.name, ROLE_NAME);
        assert_eq!(rb.role_ref.kind, "Role");
    }

    #[test]
    fn limit_range_defaults_requests_so_pods_count_against_quota() {
        let lr = limit_range("physics");
        let item = &lr.spec.unwrap().limits[0];
        assert_eq!(item.type_, "Container");
        assert_eq!(item.default_request.as_ref().unwrap()["cpu"].0, "100m");
        assert_eq!(item.default_request.as_ref().unwrap()["memory"].0, "128Mi");
        // No default limit: it would reject ordinary pods that omit limits.
        assert!(item.default.is_none());
    }
}
