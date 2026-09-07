// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The StorageClass names cluster-forge asks for by name.

use anyhow::Result;

use super::kube;

/// StorageClass names cluster-forge asks for by name. cluster-bloom creates them on RKE2 from its
/// own local-path manifest, in a role `install-silo` does not run: that role also installs a second
/// provisioner and its own default class, which would collide with the one k0s already runs. So the
/// names are created here as aliases instead. OpenBao asks for `direct`, and without it the PVC
/// stays Pending forever.
const CF_STORAGE_CLASSES: [&str; 4] = ["default", "mlstorage", "direct", "multinode"];

fn missing_storage_classes(existing: &[String]) -> Vec<&'static str> {
    CF_STORAGE_CLASSES
        .iter()
        .copied()
        .filter(|name| !existing.iter().any(|e| e == name))
        .collect()
}

/// Read the provisioner of the cluster's default StorageClass. The aliases point at whatever is
/// already provisioning, so this works whether k0s ships local-path or the operator replaced it.
fn default_provisioner(storage_classes: &serde_json::Value) -> Option<(String, String)> {
    let items = storage_classes.get("items")?.as_array()?;
    let is_default = |sc: &serde_json::Value| {
        sc.pointer("/metadata/annotations/storageclass.kubernetes.io~1is-default-class")
            .and_then(|v| v.as_str())
            == Some("true")
    };
    let sc = items.iter().find(|sc| is_default(sc))?;
    Some((
        sc.pointer("/metadata/name")?.as_str()?.to_string(),
        sc.get("provisioner")?.as_str()?.to_string(),
    ))
}

/// None of the aliases carries the default-class annotation. k0s already marks one, and a cluster
/// with two default StorageClasses cannot bind a PVC that names neither.
fn render_storage_class_aliases(names: &[&str], provisioner: &str) -> String {
    names
        .iter()
        .map(|name| {
            format!(
                "---\napiVersion: storage.k8s.io/v1\nkind: StorageClass\nmetadata:\n  name: \
                 {name}\nprovisioner: {provisioner}\nvolumeBindingMode: \
                 WaitForFirstConsumer\nreclaimPolicy: Delete\nallowVolumeExpansion: true\n"
            )
        })
        .collect()
}

/// Create the StorageClass names cluster-forge expects, for the ones the cluster does not have.
pub async fn ensure_storage_classes() -> Result<()> {
    let listed = kube::kubectl()
        .args(["get", "storageclass", "-o", "json"])
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!(
                "`{}` not found — it is needed to create the StorageClasses cluster-forge expects",
                spur_core::k0s::K0S_DEFAULT_BINARY
            ),
            _ => anyhow::anyhow!("could not list StorageClasses: {e}"),
        })?;
    if !listed.status.success() {
        anyhow::bail!(
            "could not list StorageClasses: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }

    let parsed: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
    let existing: Vec<String> = parsed
        .get("items")
        .and_then(|i| i.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|sc| Some(sc.pointer("/metadata/name")?.as_str()?.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let missing = missing_storage_classes(&existing);
    if missing.is_empty() {
        return Ok(());
    }

    let Some((default_class, provisioner)) = default_provisioner(&parsed) else {
        anyhow::bail!(
            "the cluster has no default StorageClass, so cluster-forge's classes ({}) cannot alias \
             one — set `[cluster] storage_provisioner` and re-run `spur k8s up`, or create them \
             yourself",
            missing.join(", ")
        );
    };

    let manifest = render_storage_class_aliases(&missing, &provisioner);
    kube::apply_echoing(
        manifest.as_bytes(),
        "the StorageClasses cluster-forge expects",
    )
    .await?;
    eprintln!(
        "Created StorageClass {} on {provisioner}, aliasing {default_class}",
        missing.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_class_list(entries: &[(&str, &str, bool)]) -> serde_json::Value {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|(name, provisioner, is_default)| {
                serde_json::json!({
                    "metadata": {
                        "name": name,
                        "annotations": {
                            "storageclass.kubernetes.io/is-default-class": is_default.to_string()
                        }
                    },
                    "provisioner": provisioner
                })
            })
            .collect();
        serde_json::json!({ "items": items })
    }

    // OpenBao's PVC names `direct`. k0s creates only its own class, so without the aliases the PVC
    // stays Pending and the deploy stops at "Wait for OpenBao pod to be running".
    #[test]
    fn the_classes_cluster_forge_names_are_reported_missing() {
        let missing = missing_storage_classes(&["local-path".to_string()]);
        assert_eq!(missing, vec!["default", "mlstorage", "direct", "multinode"]);
    }

    #[test]
    fn a_class_that_already_exists_is_not_recreated() {
        let existing = vec!["local-path".to_string(), "direct".to_string()];
        assert_eq!(
            missing_storage_classes(&existing),
            vec!["default", "mlstorage", "multinode"]
        );
        assert!(missing_storage_classes(&CF_STORAGE_CLASSES.map(String::from)).is_empty());
    }

    #[test]
    fn the_aliases_follow_whatever_provisions_by_default() {
        let listed = storage_class_list(&[
            ("other", "example.com/other", false),
            ("local-path", "rancher.io/local-path", true),
        ]);
        assert_eq!(
            default_provisioner(&listed),
            Some((
                "local-path".to_string(),
                "rancher.io/local-path".to_string()
            ))
        );
    }

    #[test]
    fn a_cluster_with_no_default_class_gives_nothing_to_alias() {
        let listed = storage_class_list(&[("other", "example.com/other", false)]);
        assert_eq!(default_provisioner(&listed), None);
    }

    // A second default class is worse than the missing names: a PVC that names neither cannot bind
    // at all. k0s already marks one, so the aliases must stay unmarked.
    #[test]
    fn an_alias_is_never_marked_default() {
        let rendered = render_storage_class_aliases(&["direct"], "rancher.io/local-path");
        assert!(rendered.contains("name: direct\n"));
        assert!(rendered.contains("provisioner: rancher.io/local-path\n"));
        assert!(!rendered.contains("is-default-class"));
    }

    #[test]
    fn every_missing_class_is_rendered_as_its_own_document() {
        let rendered = render_storage_class_aliases(&["direct", "mlstorage"], "p");
        assert_eq!(rendered.matches("---\n").count(), 2);
        assert_eq!(rendered.matches("kind: StorageClass").count(), 2);
    }
}
