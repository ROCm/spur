pub mod fixture;
pub mod multi_node;
pub mod single_node;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fixture::SuiteContext;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use kube::Client;
use spur_k8s::crd::{GpuRequirement, SpurJob, SpurJobSpec};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

static SUITE: OnceCell<SuiteContext> = OnceCell::const_new();

/// Get (or create) the suite-wide K8s context (namespace + CRD).
/// The namespace is created on first call and persists after the test
/// process exits. RBAC is applied by the CI workflow before tests run.
/// CI workflows provision and tear down namespaces externally; local
/// runs are cleaned up manually when needed.
pub async fn suite_context() -> &'static SuiteContext {
    SUITE
        .get_or_init(|| async {
            SuiteContext::setup()
                .await
                .expect("failed to set up K8s test suite (namespace + CRD)")
        })
        .await
}

/// Derive a unique namespace for this test run. In CI, uses the GitHub run
/// ID. Locally, combines PID and unix timestamp so parallel runs never
/// collide. Override with `SPUR_TEST_NS` for manual control.
pub fn spur_namespace() -> String {
    if let Ok(ns) = std::env::var("SPUR_TEST_NS") {
        return ns;
    }
    if let Ok(id) = std::env::var("GITHUB_RUN_ID") {
        return format!("spur-test-{id}");
    }
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("spur-ci-{pid}-{ts}")
}

/// Poll until a SpurJob reaches `target` state or a terminal state is hit.
pub async fn wait_spurjob_state(
    api: &Api<SpurJob>,
    name: &str,
    target: &str,
    timeout: Duration,
) -> Result<SpurJob> {
    let start = std::time::Instant::now();
    loop {
        let job = api
            .get(name)
            .await
            .with_context(|| format!("failed to get SpurJob {name}"))?;

        let state = job.status.as_ref().map(|s| s.state.as_str()).unwrap_or("");

        if state == target {
            return Ok(job);
        }

        if matches!(
            state,
            "Completed" | "Failed" | "Cancelled" | "Timeout" | "NodeFail"
        ) && state != target
        {
            bail!("SpurJob {name} reached terminal state '{state}', wanted '{target}'");
        }

        if start.elapsed() > timeout {
            bail!(
                "SpurJob {name} timed out after {:?} in state '{state}', wanted '{target}'",
                timeout
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn base_spec(name: &str, command: Vec<String>, num_nodes: u32) -> SpurJobSpec {
    SpurJobSpec {
        name: name.to_string(),
        image: "busybox:latest".to_string(),
        gpus: GpuRequirement::default(),
        num_nodes,
        tasks_per_node: 1,
        cpus_per_task: 1,
        memory_per_node: Some("100Mi".to_string()),
        time_limit: None,
        command,
        args: vec![],
        env: HashMap::new(),
        secret_env: HashMap::new(),
        partition: None,
        account: None,
        volumes: vec![],
        host_network: false,
        privileged: false,
        host_ipc: false,
        shm_size: None,
        extra_resources: HashMap::new(),
        tolerations: vec![],
        node_selector: HashMap::new(),
        priority_class: None,
        service_account: None,
        array_spec: None,
        dependencies: vec![],
    }
}

/// Build a minimal SpurJob object with busybox.
pub fn simple_spurjob(name: &str, command: Vec<String>) -> SpurJob {
    SpurJob::new(name, base_spec(name, command, 1))
}

/// Build a SpurJob with custom env vars.
pub fn spurjob_with_env(name: &str, command: Vec<String>, env: HashMap<String, String>) -> SpurJob {
    let mut spec = base_spec(name, command, 1);
    spec.env = env;
    SpurJob::new(name, spec)
}

/// Build a multi-node SpurJob.
pub fn multinode_spurjob(name: &str, command: Vec<String>, num_nodes: u32) -> SpurJob {
    SpurJob::new(name, base_spec(name, command, num_nodes))
}

/// Delete a SpurJob, ignoring NotFound errors.
pub async fn cleanup_spurjob(api: &Api<SpurJob>, name: &str) {
    let _ = api.delete(name, &DeleteParams::default()).await;
}

/// Create a namespace if it doesn't exist.
pub async fn ensure_namespace(client: &Client, name: &str) -> Result<()> {
    let api: Api<Namespace> = Api::all(client.clone());
    let ns: Namespace = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": { "name": name }
    }))?;
    api.create(&PostParams::default(), &ns).await.or_else(|e| {
        if is_already_exists(&e) {
            Ok(ns)
        } else {
            Err(e)
        }
    })?;
    Ok(())
}

/// Delete a namespace, ignoring NotFound.
pub async fn delete_namespace(client: &Client, name: &str) {
    let api: Api<Namespace> = Api::all(client.clone());
    let _ = api.delete(name, &DeleteParams::default()).await;
}

/// Execute a command in a pod and return stdout as a string.
pub async fn exec_in_pod(
    client: &Client,
    ns: &str,
    pod_name: &str,
    command: Vec<&str>,
) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let ap = AttachParams {
        stdin: false,
        stdout: true,
        stderr: false,
        tty: false,
        container: None,
        max_stdin_buf_size: None,
        max_stdout_buf_size: Some(1024 * 1024),
        max_stderr_buf_size: None,
    };
    let mut attached = pods.exec(pod_name, command, &ap).await?;

    let mut stdout_data = Vec::new();
    if let Some(mut stdout) = attached.stdout() {
        stdout.read_to_end(&mut stdout_data).await?;
    }

    let _ = attached.take_status();

    Ok(String::from_utf8_lossy(&stdout_data).to_string())
}

/// Read a file from inside a pod (equivalent to kubectl exec cat).
pub async fn read_file_from_pod(
    client: &Client,
    ns: &str,
    pod_name: &str,
    path: &str,
) -> Result<String> {
    exec_in_pod(client, ns, pod_name, vec!["cat", path]).await
}

/// Force-delete a pod (grace period 0).
pub async fn delete_pod(client: &Client, ns: &str, name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let dp = DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    };
    pods.delete(name, &dp)
        .await
        .with_context(|| format!("failed to delete pod {name}"))?;
    Ok(())
}

/// Wait until a pod is Ready.
pub async fn wait_pod_ready(
    client: &Client,
    ns: &str,
    pod_name: &str,
    timeout: Duration,
) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            bail!("timeout waiting for pod {pod_name} to be ready");
        }
        if let Ok(pod) = pods.get(pod_name).await {
            let ready = pod
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .map(|conds| {
                    conds
                        .iter()
                        .any(|c| c.type_ == "Ready" && c.status == "True")
                })
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Count running pods matching a label selector.
pub async fn count_ready_pods(client: &Client, ns: &str, label_selector: &str) -> Result<usize> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let list = pods
        .list(&ListParams::default().labels(label_selector))
        .await?;
    Ok(list
        .items
        .iter()
        .filter(|p| {
            p.status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .map(|ph| ph == "Running")
                .unwrap_or(false)
        })
        .count())
}

/// Retry an async check every `interval` until it returns `true` or
/// `timeout` elapses.
pub async fn assert_eventually<F, Fut>(timeout: Duration, interval: Duration, msg: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if f().await {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out after {timeout:?}: {msg}"
        );
        tokio::time::sleep(interval).await;
    }
}

fn is_already_exists(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 409)
}
