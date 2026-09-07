// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Applying manifests and waiting for what they create.
//!
//! Everything goes through `k0s kubectl`, so this needs no standalone client and no cluster
//! credentials of its own. A manifest always arrives on stdin: it keeps an argument list free of
//! anything a process listing would expose, and it is the only form that carries a whole rendered
//! chart.

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;

/// How long to wait for a workload the bootstrap depends on.
pub const ROLLOUT_TIMEOUT: &str = "5m";

/// Root's kubeconfig on this host. Some of the install runs as root through a separate process, so
/// this is the path its `kubectl` calls read; an environment variable set here never reaches them.
pub const ROOT_KUBECONFIG_DIR: &str = "/root/.kube";

/// `k0s kubectl`, so the install depends on no standalone client. k0s carries one already, and a
/// node that has just come up has nothing else.
pub fn kubectl() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(spur_core::k0s::K0S_DEFAULT_BINARY);
    cmd.arg("kubectl");
    cmd.env("KUBECONFIG", format!("{ROOT_KUBECONFIG_DIR}/config"));
    cmd
}

/// Poll for a CRD the platform stack installs. Five minutes: the measured wait is about one, and a
/// slow image pull makes that longer.
pub async fn wait_for_crd(name: &str) -> bool {
    for attempt in 0..60 {
        if exists(&["get", "crd", name]).await {
            return true;
        }
        if attempt == 0 {
            eprintln!("Waiting for the platform stack to register {name} ...");
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    false
}

/// Apply a manifest stream, letting the API server own the merge.
///
/// `--force-conflicts` takes ownership of fields another manager wrote, which is what makes a
/// re-install converge instead of erroring on every field the last run set. It cannot rewrite an
/// immutable field, so a caller that re-applies a bare Pod must delete it first.
pub async fn apply_server_side(manifest: &[u8], what: &str) -> Result<()> {
    apply_with(
        manifest,
        &[
            "apply",
            "--server-side",
            "--field-manager=argocd-controller",
            "--force-conflicts",
            "-f",
            "-",
        ],
        what,
        Echo::Silent,
    )
    .await
}

/// Apply a manifest. Use [`apply_echoing`] where nothing else names what was created.
pub async fn apply(manifest: &[u8], what: &str) -> Result<()> {
    apply_with(manifest, &["apply", "-f", "-"], what, Echo::Silent).await
}

/// Apply a manifest and print kubectl's own report of what it created, for the objects the install
/// adds outside the platform stack. A caller that prints its own summary uses [`apply`] instead.
pub async fn apply_echoing(manifest: &[u8], what: &str) -> Result<()> {
    apply_with(manifest, &["apply", "-f", "-"], what, Echo::Report).await
}

/// What to do with kubectl's own report of what it applied.
enum Echo {
    Report,
    Silent,
}

async fn apply_with(manifest: &[u8], args: &[&str], what: &str, echo: Echo) -> Result<()> {
    // The output is captured rather than inherited: a caller that retries would otherwise print the
    // same rejection on every attempt.
    let out = run_with_stdin(args, manifest)
        .await
        .with_context(|| format!("could not apply {what}"))?;
    if !out.status.success() {
        bail!(
            "could not apply {what}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if matches!(echo, Echo::Report) {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

/// Run a kubectl command with a body on stdin.
///
/// Everything that carries a manifest or a secret goes through here. stdin is the one channel that
/// keeps the payload out of the argument list, and so out of every process listing on the node.
pub async fn run_with_stdin(args: &[&str], stdin: &[u8]) -> Result<std::process::Output> {
    let mut child = kubectl()
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("could not run kubectl")?;
    let mut pipe = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("could not write to kubectl"))?;
    pipe.write_all(stdin)
        .await
        .context("could not write to kubectl")?;
    drop(pipe);
    child
        .wait_with_output()
        .await
        .context("could not run kubectl")
}

/// Create a namespace, tolerating one that already exists.
pub async fn ensure_namespace(namespace: &str) -> Result<()> {
    let manifest = format!("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {namespace}\n");
    apply(manifest.as_bytes(), &format!("namespace {namespace}")).await
}

/// Wait for a workload to finish rolling out.
pub async fn wait_rollout(kind: &str, name: &str, namespace: &str) -> Result<()> {
    let target = format!("{kind}/{name}");
    wait_exists(&target, namespace).await?;
    let out = kubectl()
        .args([
            "rollout",
            "status",
            &target,
            "-n",
            namespace,
            &format!("--timeout={ROLLOUT_TIMEOUT}"),
        ])
        .output()
        .await
        .with_context(|| format!("could not wait for {target}"))?;
    if !out.status.success() {
        bail!(
            "{target} in {namespace} did not become ready within {ROLLOUT_TIMEOUT}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Wait for a condition on one resource. Unlike `wait_rollout` this covers a bare Pod, which has no
/// rollout to follow.
pub async fn wait_condition(
    condition: &str,
    target: &str,
    namespace: &str,
    timeout: &str,
) -> Result<()> {
    wait_exists(target, namespace).await?;
    let out = kubectl()
        .args([
            "wait",
            &format!("--for=condition={condition}"),
            target,
            "-n",
            namespace,
            &format!("--timeout={timeout}"),
        ])
        .output()
        .await
        .with_context(|| format!("could not wait for {target}"))?;
    if !out.status.success() {
        bail!(
            "{target} in {namespace} did not become {condition} within {timeout}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// How long to wait for a controller to create a resource before giving up on it.
const EXISTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const EXISTS_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait for a resource to be created.
///
/// `kubectl wait` and `kubectl rollout status` both fail at once on a resource that is not there
/// yet, rather than waiting for it. Everything this bootstrap waits on is made by a controller
/// some time after the manifest is applied — a StatefulSet's pod, a Deployment — so the wait has
/// to start by waiting for the object to exist at all.
async fn wait_exists(target: &str, namespace: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + EXISTS_TIMEOUT;
    loop {
        if exists(&["get", target, "-n", namespace]).await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "{target} was not created in {namespace} within {}s",
                EXISTS_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(EXISTS_POLL).await;
    }
}

/// Wait for a field of one resource to reach a value.
///
/// OpenBao needs this rather than `wait_condition`: it starts sealed, and its readiness probe
/// reports not ready until the init job unseals it. A wait for `Ready` never returns, so the wait
/// is for the pod phase instead.
pub async fn wait_jsonpath(
    path: &str,
    value: &str,
    target: &str,
    namespace: &str,
    timeout: &str,
) -> Result<()> {
    wait_exists(target, namespace).await?;
    let out = kubectl()
        .args([
            "wait",
            &format!("--for=jsonpath={path}={value}"),
            target,
            "-n",
            namespace,
            &format!("--timeout={timeout}"),
        ])
        .output()
        .await
        .with_context(|| format!("could not wait for {target}"))?;
    if !out.status.success() {
        bail!(
            "{target} in {namespace} did not reach {path}={value} within {timeout}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a Job has already run to completion.
///
/// A Job's pod template is immutable, so re-applying one that already exists fails. A bootstrap
/// stage that resumes has to skip a finished Job and replace an unfinished one.
pub async fn job_succeeded(name: &str, namespace: &str) -> bool {
    let out = kubectl()
        .args([
            "get",
            "job",
            name,
            "-n",
            namespace,
            "-o",
            "jsonpath={.status.succeeded}",
        ])
        .output()
        .await;
    matches!(out, Ok(out) if out.status.success()
        && String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().unwrap_or(0) > 0)
}

/// Read one field of a Secret, decoded. The value never reaches an argument list, so it stays out
/// of every process listing on the node.
pub async fn secret_field(name: &str, namespace: &str, field: &str) -> Result<String> {
    let template = format!("go-template={{{{index .data \"{field}\" | base64decode}}}}");
    let out = kubectl()
        .args(["get", "secret", name, "-n", namespace, "-o", &template])
        .output()
        .await
        .with_context(|| format!("could not read secret {name}"))?;
    if !out.status.success() {
        bail!(
            "could not read {field} from secret {name} in {namespace}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        bail!("secret {name} in {namespace} holds no {field}");
    }
    Ok(value)
}

/// Create or update a Secret from literal values.
///
/// The manifest is built here and applied on stdin, so no value appears in an argument list.
/// `kubectl create secret --from-literal` would put every one of them there.
pub async fn apply_secret(name: &str, namespace: &str, entries: &[(&str, &str)]) -> Result<()> {
    let data: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .map(|(k, v)| {
            (
                (*k).to_string(),
                serde_json::Value::String((*v).to_string()),
            )
        })
        .collect();
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": namespace },
        "stringData": data,
    });
    let body = serde_yaml::to_string(&manifest)
        .with_context(|| format!("could not build secret {name}"))?;
    apply(body.as_bytes(), &format!("secret {name}")).await
}

/// Delete a resource that may not be there. Used for cleanup, where a missing resource is the
/// wanted end state rather than a failure.
pub async fn delete_ignore_missing(args: &[&str]) -> Result<()> {
    let out = kubectl()
        .arg("delete")
        .args(args)
        .arg("--ignore-not-found")
        .output()
        .await
        .context("could not delete a resource")?;
    if !out.status.success() {
        bail!(
            "could not delete {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Print a failed Job's own output.
///
/// A bootstrap job does its work over the API and reports what went wrong on stdout, so the log is
/// the only account of the failure. Without it the operator gets a timeout and nothing to act on.
pub fn print_job_logs(name: &str, namespace: &str) {
    let out = std::process::Command::new(spur_core::k0s::K0S_DEFAULT_BINARY)
        .args([
            "kubectl",
            "logs",
            &format!("job/{name}"),
            "-n",
            namespace,
            "--tail=50",
        ])
        .output();
    if let Ok(out) = out {
        let log = String::from_utf8_lossy(&out.stdout);
        if !log.trim().is_empty() {
            eprintln!("--- last 50 lines of {name} ---\n{}", log.trim_end());
        }
    }
}

/// Whether a resource exists. Used to skip a bootstrap stage that already ran, so a re-install
/// resumes rather than repeating work that takes minutes.
pub async fn exists(args: &[&str]) -> bool {
    kubectl()
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|st| st.success())
        .unwrap_or(false)
}
