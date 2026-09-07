// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Putting a kubeconfig where the install's `kubectl` calls find it.

use anyhow::Result;

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::ClusterKubeconfigRequest;

use super::kube::ROOT_KUBECONFIG_DIR;

/// Make the directory that holds the generated config and any generated key. `create_dir` fails
/// when the path already exists, so a pre-planted symlink cannot redirect a write that runs as root.
pub fn private_dir() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("spur-install-silo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Put the kubeconfig where `kubectl` finds it without being told. Returns false when a kubeconfig
/// is already there, which is left untouched: it may point at the cluster the operator wants.
fn write_default_kubeconfig(dir: &std::path::Path, kubeconfig: &str) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("config");
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(&path, kubeconfig)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(true)
}

/// Fetch the cluster-admin kubeconfig and put it where the deployer's `kubectl` calls find it. The
/// deployer needs full access: it creates namespaces and installs cluster-scoped resources.
///
/// Without this, `kubectl` falls back to `http://localhost:8080`. kube-router answers there on a
/// k0s cluster, so every call reaches the wrong daemon and fails with a 404 that names openapi
/// rather than the address.
pub async fn stage(controller: &str, caller: String) -> Result<()> {
    let dir = std::path::Path::new(ROOT_KUBECONFIG_DIR);
    if dir.join("config").exists() {
        return Ok(());
    }
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    let resp = client
        .cluster_kubeconfig(ClusterKubeconfigRequest {
            user: String::new(),
            caller,
            admin: true,
        })
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "could not get the cluster-admin kubeconfig the deployer needs: {}. \
                 Set `[cluster] allow_admin_kubeconfig = true` in spur.conf, or put a kubeconfig \
                 at {ROOT_KUBECONFIG_DIR}/config yourself",
                e.message()
            )
        })?
        .into_inner();
    if write_default_kubeconfig(dir, &resp.kubeconfig)? {
        eprintln!("Wrote the cluster-admin kubeconfig to {ROOT_KUBECONFIG_DIR}/config");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spur-k8s-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // kubectl with no kubeconfig falls back to http://localhost:8080, where kube-router answers on
    // a k0s cluster. The deployer then validates against the wrong daemon, so every kubectl call
    // fails with a 404 naming openapi. It reads root's default kubeconfig, and an environment
    // variable set here cannot reach it: bloom connects back over SSH and becomes root.
    #[test]
    fn the_kubeconfig_lands_where_kubectl_looks_without_being_told() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("kubeconfig");

        assert!(write_default_kubeconfig(&dir, "apiVersion: v1\n").unwrap());

        let path = dir.join("config");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "apiVersion: v1\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An existing kubeconfig may point at the cluster the operator wants, so overwriting it would
    // silently retarget the deploy.
    #[test]
    fn an_existing_kubeconfig_is_left_alone() {
        let dir = scratch_dir("existing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "operator's own\n").unwrap();

        assert!(!write_default_kubeconfig(&dir, "apiVersion: v1\n").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("config")).unwrap(),
            "operator's own\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
