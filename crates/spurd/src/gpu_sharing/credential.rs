// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Kubernetes identity of a k0s worker's GPU sharing. A worker has no
//! admin kubeconfig, so a control-plane node applies this RBAC and mints a
//! bound token for the worker's ServiceAccount.

use serde_json::{json, Value};

use super::placeholder::{LABEL_MANAGED_BY, MANAGED_BY, NAMESPACE};

/// Lifetime of a minted token. The worker rebuilds its client well before this.
pub const TOKEN_DURATION: &str = "24h";

pub fn service_account_name(node: &str) -> String {
    format!("spurd-gpu-sharing-{node}")
}

/// Namespace, ServiceAccount and minimum RBAC for the GPU sharing of `node`,
/// as one `v1/List` for `kubectl apply`. `node` must be a DNS-1123 label.
pub fn rbac_manifest(node: &str) -> Value {
    let name = service_account_name(node);
    let labels = json!({ LABEL_MANAGED_BY: MANAGED_BY });
    let subjects = json!([{"kind": "ServiceAccount", "name": name, "namespace": NAMESPACE}]);
    let owned = ["create", "get", "list", "watch", "delete", "patch"];
    json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": {"name": NAMESPACE, "labels": {
                    LABEL_MANAGED_BY: MANAGED_BY,
                    // A worker credential may create pods here; baseline keeps them unprivileged.
                    "pod-security.kubernetes.io/enforce": "baseline",
                }},
            },
            {
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {"name": name, "namespace": NAMESPACE, "labels": labels},
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "ClusterRole",
                "metadata": {"name": name, "labels": labels},
                "rules": [
                    {"apiGroups": ["resource.k8s.io"],
                     "resources": ["resourceclaims", "resourceslices"],
                     "verbs": ["get", "list", "watch"]},
                    {"apiGroups": [""], "resources": ["nodes"],
                     "resourceNames": [node], "verbs": ["get", "patch"]},
                    {"apiGroups": [""], "resources": ["namespaces"],
                     "resourceNames": [NAMESPACE], "verbs": ["get"]},
                ],
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "ClusterRoleBinding",
                "metadata": {"name": name, "labels": labels},
                "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": name},
                "subjects": subjects,
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "Role",
                "metadata": {"name": name, "namespace": NAMESPACE, "labels": labels},
                "rules": [
                    {"apiGroups": ["resource.k8s.io"], "resources": ["resourceclaims"], "verbs": owned},
                    {"apiGroups": [""], "resources": ["pods"], "verbs": owned},
                ],
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "RoleBinding",
                "metadata": {"name": name, "namespace": NAMESPACE, "labels": labels},
                "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name},
                "subjects": subjects,
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Namespace, ServiceAccount};
    use k8s_openapi::api::rbac::v1::{
        ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding,
    };

    fn item<T: serde::de::DeserializeOwned>(manifest: &Value, kind: &str) -> T {
        let items = manifest["items"].as_array().expect("items");
        let found: Vec<&Value> = items.iter().filter(|i| i["kind"] == kind).collect();
        assert_eq!(found.len(), 1, "one {kind}");
        serde_json::from_value(found[0].clone()).expect(kind)
    }

    fn rule<'a>(rules: &'a [PolicyRule], resource: &str) -> &'a PolicyRule {
        rules
            .iter()
            .find(|r| r.resources.iter().flatten().any(|x| x == resource))
            .unwrap_or_else(|| panic!("no rule for {resource}"))
    }

    fn strings(v: &Option<Vec<String>>) -> Vec<&str> {
        v.iter().flatten().map(String::as_str).collect()
    }

    #[test]
    fn manifest_parses_as_kubernetes_objects() {
        let m = rbac_manifest("gpu-node-1");

        let ns: Namespace = item(&m, "Namespace");
        let sa: ServiceAccount = item(&m, "ServiceAccount");

        assert_eq!(ns.metadata.name.as_deref(), Some("spur-system"));
        assert_eq!(
            ns.metadata.labels.unwrap()["pod-security.kubernetes.io/enforce"],
            "baseline"
        );
        assert_eq!(
            sa.metadata.name.as_deref(),
            Some("spurd-gpu-sharing-gpu-node-1")
        );
        assert_eq!(sa.metadata.namespace.as_deref(), Some("spur-system"));
    }

    #[test]
    fn cluster_rights_are_read_only_except_the_own_node() {
        let m = rbac_manifest("gpu-node-1");
        let role: ClusterRole = item(&m, "ClusterRole");
        let rules = role.rules.unwrap();

        for resource in ["resourceclaims", "resourceslices"] {
            let r = rule(&rules, resource);
            assert_eq!(r.verbs, ["get", "list", "watch"]);
            assert!(r.resource_names.is_none());
        }
        let nodes = rule(&rules, "nodes");
        assert_eq!(strings(&nodes.resource_names), ["gpu-node-1"]);
        assert_eq!(nodes.verbs, ["get", "patch"]);
        let namespaces = rule(&rules, "namespaces");
        assert_eq!(strings(&namespaces.resource_names), ["spur-system"]);
        assert_eq!(namespaces.verbs, ["get"]);
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn namespaced_rights_cover_claims_and_pods_in_spur_system_only() {
        let m = rbac_manifest("gpu-node-1");
        let role: Role = item(&m, "Role");
        let rules = role.rules.unwrap();

        assert_eq!(role.metadata.namespace.as_deref(), Some("spur-system"));
        for resource in ["resourceclaims", "pods"] {
            let r = rule(&rules, resource);
            assert_eq!(
                r.verbs,
                ["create", "get", "list", "watch", "delete", "patch"]
            );
        }
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn bindings_tie_the_roles_to_the_node_service_account() {
        let m = rbac_manifest("gpu-node-1");
        let crb: ClusterRoleBinding = item(&m, "ClusterRoleBinding");
        let rb: RoleBinding = item(&m, "RoleBinding");

        for (role_ref, subjects) in [(crb.role_ref, crb.subjects), (rb.role_ref, rb.subjects)] {
            assert_eq!(role_ref.name, "spurd-gpu-sharing-gpu-node-1");
            let subjects = subjects.unwrap();
            assert_eq!(subjects.len(), 1);
            assert_eq!(subjects[0].kind, "ServiceAccount");
            assert_eq!(subjects[0].name, "spurd-gpu-sharing-gpu-node-1");
            assert_eq!(subjects[0].namespace.as_deref(), Some("spur-system"));
        }
        assert_eq!(rb.metadata.namespace.as_deref(), Some("spur-system"));
    }
}
