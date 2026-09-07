// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The node the platform stack pins its gateway and its singletons to.

use anyhow::Result;

use super::kube;

const FIRST_NODE_LABEL: &str = "cluster-bloom/first-node";

/// One node, its creation time, and the value it carries for [`FIRST_NODE_LABEL`].
type NodeEntry = (String, String, Option<String>);

fn node_entries(nodes: &serde_json::Value) -> Vec<NodeEntry> {
    let label_pointer = format!("/metadata/labels/{}", FIRST_NODE_LABEL.replace('/', "~1"));
    nodes
        .get("items")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|node| {
                    let name = node.pointer("/metadata/name")?.as_str()?.to_string();
                    let created = node
                        .pointer("/metadata/creationTimestamp")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let label = node
                        .pointer(&label_pointer)
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    Some((name, created, label))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn first_node_holder(entries: &[NodeEntry]) -> Option<&str> {
    entries
        .iter()
        .find(|(_, _, label)| label.as_deref() == Some("true"))
        .map(|(name, _, _)| name.as_str())
}

/// Which node gets the label. The control plane node wins where it also runs workloads, and the
/// oldest node otherwise. Both rules are stable, so a re-run picks the same node and never moves
/// the platform stack's singletons. The timestamps are RFC 3339 in UTC, so they sort as text.
fn pick_first_node<'a>(entries: &'a [NodeEntry], preferred: &[String]) -> Option<&'a str> {
    for wanted in preferred {
        if let Some((name, _, _)) = entries.iter().find(|(name, _, _)| name == wanted) {
            return Some(name.as_str());
        }
    }
    entries
        .iter()
        .min_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)))
        .map(|(name, _, _)| name.as_str())
}

/// cluster-forge pins the Envoy proxy pods to `cluster-bloom/first-node=true`. cluster-bloom writes
/// that label through the RKE2 node config, which `spur k8s` does not use, so without this the pods
/// stay Pending and the Gateway reports `NoResources` forever.
pub async fn ensure_first_node_label(preferred: &[String]) -> Result<String> {
    let listed = kube::kubectl()
        .args(["get", "nodes", "-o", "json"])
        .output()
        .await?;
    if !listed.status.success() {
        anyhow::bail!(
            "could not list nodes: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    let entries = node_entries(&serde_json::from_slice(&listed.stdout)?);
    if let Some(holder) = first_node_holder(&entries) {
        return Ok(holder.to_string());
    }
    let Some(target) = pick_first_node(&entries, preferred) else {
        anyhow::bail!(
            "the cluster has no nodes, so the platform stack has nowhere to run — give the k0s \
             cluster a worker node and re-run"
        );
    };
    let out = kube::kubectl()
        .args([
            "label",
            "node",
            target,
            &format!("{FIRST_NODE_LABEL}=true"),
            "--overwrite",
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "could not label node {target} with {FIRST_NODE_LABEL}=true: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    eprintln!("Labelled node {target} with {FIRST_NODE_LABEL}=true");
    Ok(target.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_list(nodes: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
        let items: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(name, created, label)| {
                let mut labels = serde_json::Map::new();
                if let Some(value) = label {
                    labels.insert(FIRST_NODE_LABEL.to_string(), (*value).into());
                }
                serde_json::json!({
                    "metadata": {
                        "name": name,
                        "creationTimestamp": created,
                        "labels": labels,
                    }
                })
            })
            .collect();
        serde_json::json!({ "items": items })
    }

    #[test]
    fn reads_the_first_node_label_off_a_node() {
        let entries = node_entries(&node_list(&[
            ("worker-b", "2026-08-28T09:00:00Z", None),
            ("worker-a", "2026-08-28T10:00:00Z", Some("true")),
        ]));
        assert_eq!(first_node_holder(&entries), Some("worker-a"));
    }

    #[test]
    fn a_false_first_node_label_holds_nothing() {
        let entries = node_entries(&node_list(&[(
            "worker-a",
            "2026-08-28T09:00:00Z",
            Some("false"),
        )]));
        assert_eq!(first_node_holder(&entries), None);
    }

    #[test]
    fn prefers_a_control_plane_node_that_runs_workloads() {
        let entries = node_entries(&node_list(&[
            ("worker-a", "2026-08-28T09:00:00Z", None),
            ("head-node", "2026-08-28T10:00:00Z", None),
        ]));
        let preferred = vec!["head-node".to_string()];
        assert_eq!(pick_first_node(&entries, &preferred), Some("head-node"));
    }

    #[test]
    fn falls_back_to_the_oldest_node() {
        let entries = node_entries(&node_list(&[
            ("worker-b", "2026-08-28T10:00:00Z", None),
            ("worker-a", "2026-08-28T09:00:00Z", None),
        ]));
        // The control plane node is a k0s controller with no kubelet, so it is not in the list.
        let preferred = vec!["head-node".to_string()];
        assert_eq!(pick_first_node(&entries, &preferred), Some("worker-a"));
    }

    #[test]
    fn breaks_a_creation_time_tie_by_name() {
        let entries = node_entries(&node_list(&[
            ("worker-b", "2026-08-28T09:00:00Z", None),
            ("worker-a", "2026-08-28T09:00:00Z", None),
        ]));
        assert_eq!(pick_first_node(&entries, &[]), Some("worker-a"));
    }

    #[test]
    fn picks_no_node_when_the_cluster_is_empty() {
        let entries = node_entries(&node_list(&[]));
        assert_eq!(pick_first_node(&entries, &[]), None);
    }
}
