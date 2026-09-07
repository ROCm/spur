// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Recovering a database the platform stack cannot recover itself.

use anyhow::Result;

use super::kube;

const CNPG_CLUSTER_CRD: &str = "clusters.postgresql.cnpg.io";
const CNPG_CLUSTER_KIND: &str = "cluster.postgresql.cnpg.io";

/// The databases that lost the race, as `(namespace, name)`. Two independent signals are required,
/// because either one alone also describes a database that is merely still starting: CNPG must
/// report the cluster unrecoverable, and the caller must then find no PVC for it.
fn unrecoverable_cnpg_clusters(clusters: &serde_json::Value) -> Vec<(String, String)> {
    clusters
        .get("items")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|cluster| {
                    cluster
                        .pointer("/status/phase")
                        .and_then(|phase| phase.as_str())
                        .is_some_and(|phase| phase.to_lowercase().contains("unrecoverable"))
                })
                .filter_map(|cluster| {
                    Some((
                        cluster
                            .pointer("/metadata/namespace")?
                            .as_str()?
                            .to_string(),
                        cluster.pointer("/metadata/name")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn cnpg_cluster_has_no_volume(namespace: &str, name: &str) -> Result<bool> {
    let listed = kube::kubectl()
        .args([
            "get",
            "pvc",
            "-n",
            namespace,
            "-l",
            &format!("cnpg.io/cluster={name}"),
            "-o",
            "name",
        ])
        .output()
        .await?;
    if !listed.status.success() {
        anyhow::bail!(
            "could not list the volumes of {namespace}/{name}: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&listed.stdout).trim().is_empty())
}

/// Recover a database the platform stack cannot recover itself.
///
/// cluster-forge registers the Kyverno admission webhook with `failurePolicy: Fail` before the
/// Kyverno pod serves, and on a `spur k8s` cluster the API server reaches that webhook through
/// konnectivity, which is slower than a direct call. A PVC created inside that window is therefore
/// rejected. CNPG records the instance serial before it creates the PVC, so it then refuses to
/// create the primary instance for the life of the resource and asks for a restore from backup.
///
/// Deleting the resource is safe here and only here: a cluster with no PVC holds no data, and the
/// ArgoCD application that owns it has `selfHeal`, so it comes back within about two minutes.
pub async fn repair_unrecoverable_databases() -> Result<()> {
    if !kube::wait_for_crd(CNPG_CLUSTER_CRD).await {
        return Ok(());
    }
    // The race resolves within seconds of the resource appearing, but the platform stack creates
    // the databases over several minutes, so sweep repeatedly rather than once.
    eprintln!("Watching the platform stack's databases for five minutes ...");
    let mut repaired = 0;
    for _ in 0..20 {
        let listed = kube::kubectl()
            .args(["get", CNPG_CLUSTER_KIND, "-A", "-o", "json"])
            .output()
            .await?;
        if listed.status.success() {
            let parsed: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
            for (namespace, name) in unrecoverable_cnpg_clusters(&parsed) {
                if !cnpg_cluster_has_no_volume(&namespace, &name).await? {
                    eprintln!(
                        "warning: database {namespace}/{name} is unrecoverable and has volumes, so \
                         it needs a restore from backup"
                    );
                    continue;
                }
                let deleted = kube::kubectl()
                    .args([
                        "delete",
                        CNPG_CLUSTER_KIND,
                        "-n",
                        &namespace,
                        &name,
                        "--wait=false",
                    ])
                    .output()
                    .await?;
                if !deleted.status.success() {
                    eprintln!(
                        "warning: could not recreate the empty database {namespace}/{name}: {}",
                        String::from_utf8_lossy(&deleted.stderr).trim()
                    );
                    continue;
                }
                repaired += 1;
                eprintln!("Recreated the empty database {namespace}/{name}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
    if repaired > 0 {
        eprintln!("Recreated {repaired} database(s) the admission webhook rejected");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_the_unrecoverable_databases() {
        let clusters = serde_json::json!({"items": [
            {"metadata": {"namespace": "airm", "name": "airm-cnpg"},
             "status": {"phase": "Cluster is unrecoverable and needs manual intervention"}},
            {"metadata": {"namespace": "keycloak", "name": "keycloak-cnpg"},
             "status": {"phase": "Cluster in healthy state"}},
            {"metadata": {"namespace": "aiwb", "name": "aiwb-cnpg"}},
        ]});
        assert_eq!(
            unrecoverable_cnpg_clusters(&clusters),
            vec![("airm".to_string(), "airm-cnpg".to_string())]
        );
    }

    #[test]
    fn finds_no_database_in_an_empty_list() {
        let clusters = serde_json::json!({"items": []});
        assert!(unrecoverable_cnpg_clusters(&clusters).is_empty());
    }
}
