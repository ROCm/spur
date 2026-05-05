use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// SpurJob CRD spec — Kubernetes-native way to submit GPU jobs to Spur.
///
/// apiVersion: spur.ai/v1alpha1
/// kind: SpurJob
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "spur.ai",
    version = "v1alpha1",
    kind = "SpurJob",
    namespaced,
    status = "SpurJobStatus",
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Job ID","type":"integer","jsonPath":".status.spurJobId"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SpurJobSpec {
    /// Job name.
    pub name: String,

    /// Container image to run.
    pub image: String,

    /// GPU requirements.
    #[serde(default)]
    pub gpus: GpuRequirement,

    /// Number of nodes to allocate.
    #[serde(default = "default_one")]
    pub num_nodes: u32,

    /// Tasks per node (e.g., one per GPU).
    #[serde(default = "default_one")]
    pub tasks_per_node: u32,

    /// CPUs per task.
    #[serde(default = "default_one")]
    pub cpus_per_task: u32,

    /// Memory per node (K8s quantity string, e.g. "256Gi").
    #[serde(default)]
    pub memory_per_node: Option<String>,

    /// Time limit (e.g., "4h", "30m", "1h30m").
    #[serde(default)]
    pub time_limit: Option<String>,

    /// Command to run in the container.
    #[serde(default)]
    pub command: Vec<String>,

    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,

    /// Environment variables from K8s Secrets.
    /// Map of env var name → "secret-name/key" reference.
    /// The operator injects these as secretKeyRef env vars into the pod,
    /// keeping secrets out of the SpurJob spec.
    ///
    /// Example: `{"HF_TOKEN": "hf-creds/token", "API_KEY": "my-secret/api-key"}`
    #[serde(default)]
    pub secret_env: std::collections::HashMap<String, String>,

    /// Spur partition to submit to.
    #[serde(default)]
    pub partition: Option<String>,

    /// Spur account.
    #[serde(default)]
    pub account: Option<String>,

    /// Volume mounts: "/src:/dst:ro" for hostPath or "pvc:claim-name:/dst" for PVCs.
    #[serde(default)]
    pub volumes: Vec<String>,

    /// Enable host networking (for RDMA/NCCL).
    #[serde(default)]
    pub host_network: bool,

    /// Run container in privileged mode.
    #[serde(default)]
    pub privileged: bool,

    /// Enable host IPC namespace sharing (for NCCL shared memory).
    #[serde(default)]
    pub host_ipc: bool,

    /// Shared memory size (e.g., "64Gi"). Mounted as emptyDir at /dev/shm.
    #[serde(default)]
    pub shm_size: Option<String>,

    /// Extra device plugin resources (e.g., {"rdma/devices": "1"}).
    #[serde(default)]
    pub extra_resources: std::collections::HashMap<String, String>,

    /// K8s tolerations.
    #[serde(default)]
    pub tolerations: Vec<TolerationSpec>,

    /// K8s node selector labels.
    #[serde(default)]
    pub node_selector: std::collections::HashMap<String, String>,

    /// K8s PriorityClass name.
    #[serde(default)]
    pub priority_class: Option<String>,

    /// K8s ServiceAccount name.
    #[serde(default)]
    pub service_account: Option<String>,

    /// Job array spec (e.g., "0-99%10").
    #[serde(default)]
    pub array_spec: Option<String>,

    /// Job dependencies (e.g., ["afterok:123", "afterany:456"]).
    #[serde(default)]
    pub dependencies: Vec<String>,
}

fn default_one() -> u32 {
    1
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuRequirement {
    /// Number of GPUs needed.
    #[serde(default)]
    pub count: u32,
    /// GPU type filter (e.g., "mi300x", "h100").
    #[serde(default)]
    pub gpu_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TolerationSpec {
    pub key: Option<String>,
    #[serde(default = "default_equal")]
    pub operator: String,
    pub value: Option<String>,
    pub effect: Option<String>,
    pub toleration_seconds: Option<i64>,
}

fn default_equal() -> String {
    "Equal".to_string()
}

/// Status subresource for SpurJob.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpurJobStatus {
    /// Current state (Pending, Running, Completed, Failed, Cancelled).
    #[serde(default)]
    pub state: String,
    /// Spur-assigned job ID.
    #[serde(default)]
    pub spur_job_id: Option<u32>,
    /// Nodes assigned by the scheduler.
    #[serde(default)]
    pub assigned_nodes: Vec<String>,
    /// Pod names created for this job.
    #[serde(default)]
    pub pods: Vec<String>,
    /// Human-readable message.
    #[serde(default)]
    pub message: Option<String>,
}

/// Convert a SpurJobSpec into a spur-core JobSpec for submission to spurctld.
pub fn to_core_job_spec(spec: &SpurJobSpec, user: &str) -> spur_core::job::JobSpec {
    let mut gres = Vec::new();
    if spec.gpus.count > 0 {
        gres.push(crate::agent::gpu_request_to_gres(
            spec.gpus.count,
            spec.gpus.gpu_type.as_deref(),
        ));
    }

    let memory_per_node_mb = spec
        .memory_per_node
        .as_deref()
        .and_then(parse_k8s_memory_to_mb);

    let time_limit = spec.time_limit.as_deref().and_then(parse_duration_string);

    // Combine container_mounts from CRD volumes field
    let container_mounts = spec.volumes.clone();

    spur_core::job::JobSpec {
        name: spec.name.clone(),
        partition: spec.partition.clone(),
        account: spec.account.clone(),
        user: user.to_string(),
        num_nodes: spec.num_nodes,
        num_tasks: spec.num_nodes * spec.tasks_per_node,
        tasks_per_node: Some(spec.tasks_per_node),
        cpus_per_task: spec.cpus_per_task,
        memory_per_node_mb,
        gres,
        container_image: Some(spec.image.clone()),
        container_mounts,
        argv: if spec.command.is_empty() {
            spec.args.clone()
        } else {
            let mut v = spec.command.clone();
            v.extend(spec.args.iter().cloned());
            v
        },
        environment: spec.env.clone(),
        time_limit,
        dependency: spec.dependencies.clone(),
        array_spec: spec.array_spec.clone(),
        host_network: spec.host_network,
        privileged: spec.privileged,
        host_ipc: spec.host_ipc,
        shm_size: spec.shm_size.clone(),
        extra_resources: spec.extra_resources.clone(),
        ..Default::default()
    }
}

/// Parse K8s memory quantity (e.g., "256Gi", "1024Mi", "4096") to megabytes.
pub fn parse_k8s_memory_to_mb(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.ends_with("Ti") {
        let val: u64 = s.strip_suffix("Ti")?.trim().parse().ok()?;
        Some(val * 1024 * 1024)
    } else if s.ends_with("Gi") {
        let val: u64 = s.strip_suffix("Gi")?.trim().parse().ok()?;
        Some(val * 1024)
    } else if s.ends_with("Mi") {
        let val: u64 = s.strip_suffix("Mi")?.trim().parse().ok()?;
        Some(val)
    } else if s.ends_with("Ki") {
        let val: u64 = s.strip_suffix("Ki")?.trim().parse().ok()?;
        Some(val / 1024)
    } else {
        // Plain bytes
        let val: u64 = s.parse().ok()?;
        Some(val / (1024 * 1024))
    }
}

/// Parse a duration string like "4h", "30m", "1h30m" into chrono::Duration.
fn parse_duration_string(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let val: i64 = num_buf.parse().ok()?;
            num_buf.clear();
            match ch {
                'h' | 'H' => total_secs += val * 3600,
                'm' | 'M' => total_secs += val * 60,
                's' | 'S' => total_secs += val,
                'd' | 'D' => total_secs += val * 86400,
                _ => return None,
            }
        }
    }

    // Trailing number with no unit = minutes (Slurm convention)
    if !num_buf.is_empty() {
        let val: i64 = num_buf.parse().ok()?;
        total_secs += val * 60;
    }

    if total_secs > 0 {
        Some(chrono::Duration::seconds(total_secs))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to build a minimal SpurJobSpec
    fn minimal_spec() -> SpurJobSpec {
        SpurJobSpec {
            name: "test".into(),
            image: "test:latest".into(),
            gpus: Default::default(),
            num_nodes: 1,
            tasks_per_node: 1,
            cpus_per_task: 1,
            memory_per_node: None,
            time_limit: None,
            command: vec![],
            args: vec![],
            env: Default::default(),
            partition: None,
            account: None,
            volumes: vec![],
            host_network: false,
            privileged: false,
            host_ipc: false,
            shm_size: None,
            extra_resources: std::collections::HashMap::new(),
            secret_env: std::collections::HashMap::new(),
            tolerations: vec![],
            node_selector: Default::default(),
            priority_class: None,
            service_account: None,
            array_spec: None,
            dependencies: vec![],
        }
    }

    // --- parse_k8s_memory_to_mb ---

    #[test]
    fn test_parse_k8s_memory() {
        assert_eq!(parse_k8s_memory_to_mb("256Gi"), Some(262144));
        assert_eq!(parse_k8s_memory_to_mb("1024Mi"), Some(1024));
        assert_eq!(parse_k8s_memory_to_mb("2Ti"), Some(2 * 1024 * 1024));
        assert_eq!(parse_k8s_memory_to_mb("1048576Ki"), Some(1024));
        assert_eq!(parse_k8s_memory_to_mb("1073741824"), Some(1024));
    }

    #[test]
    fn test_parse_k8s_memory_with_whitespace() {
        assert_eq!(parse_k8s_memory_to_mb("  256Gi  "), Some(262144));
    }

    #[test]
    fn test_parse_k8s_memory_zero() {
        assert_eq!(parse_k8s_memory_to_mb("0"), Some(0));
        assert_eq!(parse_k8s_memory_to_mb("0Mi"), Some(0));
    }

    #[test]
    fn test_parse_k8s_memory_invalid() {
        assert_eq!(parse_k8s_memory_to_mb("invalid"), None);
        assert_eq!(parse_k8s_memory_to_mb(""), None);
    }

    #[test]
    fn test_parse_k8s_memory_small_ki_rounds_down() {
        // 512Ki = 0.5Mi, truncates to 0
        assert_eq!(parse_k8s_memory_to_mb("512Ki"), Some(0));
    }

    // --- parse_duration_string ---

    #[test]
    fn test_parse_duration() {
        assert_eq!(
            parse_duration_string("4h"),
            Some(chrono::Duration::seconds(14400))
        );
        assert_eq!(
            parse_duration_string("30m"),
            Some(chrono::Duration::seconds(1800))
        );
        assert_eq!(
            parse_duration_string("1h30m"),
            Some(chrono::Duration::seconds(5400))
        );
        assert_eq!(
            parse_duration_string("60"),
            Some(chrono::Duration::seconds(3600))
        );
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(
            parse_duration_string("90s"),
            Some(chrono::Duration::seconds(90))
        );
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(
            parse_duration_string("2d"),
            Some(chrono::Duration::seconds(2 * 86400))
        );
    }

    #[test]
    fn test_parse_duration_complex() {
        assert_eq!(
            parse_duration_string("1d2h30m15s"),
            Some(chrono::Duration::seconds(86400 + 7200 + 1800 + 15))
        );
    }

    #[test]
    fn test_parse_duration_zero() {
        assert_eq!(parse_duration_string("0"), None);
        assert_eq!(parse_duration_string("0h"), None);
    }

    #[test]
    fn test_parse_duration_invalid_unit() {
        assert_eq!(parse_duration_string("5x"), None);
    }

    #[test]
    fn test_parse_duration_whitespace() {
        assert_eq!(
            parse_duration_string("  2h  "),
            Some(chrono::Duration::seconds(7200))
        );
    }

    // --- to_core_job_spec ---

    #[test]
    fn test_spur_job_to_core_spec() {
        let spec = SpurJobSpec {
            name: "training-run".into(),
            image: "nvcr.io/nvidia/pytorch:24.01".into(),
            gpus: GpuRequirement {
                count: 8,
                gpu_type: Some("mi300x".into()),
            },
            num_nodes: 4,
            tasks_per_node: 8,
            cpus_per_task: 16,
            memory_per_node: Some("256Gi".into()),
            time_limit: Some("4h".into()),
            command: vec!["torchrun".into(), "--nproc_per_node=8".into()],
            args: vec!["train.py".into()],
            env: Default::default(),
            partition: None,
            account: None,
            volumes: vec!["/data:/mnt/data:ro".into()],
            host_network: false,
            privileged: false,
            host_ipc: false,
            shm_size: None,
            extra_resources: std::collections::HashMap::new(),
            secret_env: std::collections::HashMap::new(),
            tolerations: vec![],
            node_selector: Default::default(),
            priority_class: None,
            service_account: None,
            array_spec: None,
            dependencies: vec![],
        };

        let core = to_core_job_spec(&spec, "testuser");
        assert_eq!(core.name, "training-run");
        assert_eq!(core.num_nodes, 4);
        assert_eq!(core.num_tasks, 32); // 4 * 8
        assert_eq!(core.cpus_per_task, 16);
        assert_eq!(core.gres, vec!["gpu:mi300x:8"]);
        assert_eq!(core.memory_per_node_mb, Some(262144));
        assert_eq!(
            core.container_image,
            Some("nvcr.io/nvidia/pytorch:24.01".into())
        );
        assert_eq!(
            core.argv,
            vec!["torchrun", "--nproc_per_node=8", "train.py"]
        );
        assert!(core.time_limit.is_some());
        assert_eq!(core.container_mounts, vec!["/data:/mnt/data:ro"]);
    }

    #[test]
    fn test_gpu_to_gres() {
        let mut spec = minimal_spec();
        spec.gpus = GpuRequirement {
            count: 4,
            gpu_type: None,
        };
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.gres, vec!["gpu:any:4"]);
    }

    #[test]
    fn test_no_gpus_no_gres() {
        let spec = minimal_spec();
        let core = to_core_job_spec(&spec, "user");
        assert!(core.gres.is_empty());
    }

    #[test]
    fn test_user_passthrough() {
        let spec = minimal_spec();
        let core = to_core_job_spec(&spec, "alice");
        assert_eq!(core.user, "alice");
    }

    #[test]
    fn test_args_only_no_command() {
        let mut spec = minimal_spec();
        spec.command = vec![];
        spec.args = vec!["run.sh".into(), "--flag".into()];
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.argv, vec!["run.sh", "--flag"]);
    }

    #[test]
    fn test_command_and_args_combined() {
        let mut spec = minimal_spec();
        spec.command = vec!["python".into()];
        spec.args = vec!["train.py".into(), "--epochs=10".into()];
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.argv, vec!["python", "train.py", "--epochs=10"]);
    }

    #[test]
    fn test_partition_and_account() {
        let mut spec = minimal_spec();
        spec.partition = Some("gpu-a100".into());
        spec.account = Some("research-lab".into());
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.partition, Some("gpu-a100".into()));
        assert_eq!(core.account, Some("research-lab".into()));
    }

    #[test]
    fn test_volumes_mapped_to_container_mounts() {
        let mut spec = minimal_spec();
        spec.volumes = vec![
            "/data:/mnt/data:ro".into(),
            "pvc:my-claim:/checkpoints".into(),
        ];
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.container_mounts.len(), 2);
        assert_eq!(core.container_mounts[0], "/data:/mnt/data:ro");
        assert_eq!(core.container_mounts[1], "pvc:my-claim:/checkpoints");
    }

    #[test]
    fn test_dependencies_mapped() {
        let mut spec = minimal_spec();
        spec.dependencies = vec!["afterok:100".into(), "afterany:200".into()];
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.dependency, vec!["afterok:100", "afterany:200"]);
    }

    #[test]
    fn test_array_spec_mapped() {
        let mut spec = minimal_spec();
        spec.array_spec = Some("0-99%10".into());
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.array_spec, Some("0-99%10".into()));
    }

    #[test]
    fn test_num_tasks_calculation() {
        let mut spec = minimal_spec();
        spec.num_nodes = 3;
        spec.tasks_per_node = 4;
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.num_tasks, 12); // 3 * 4
    }

    #[test]
    fn test_env_passthrough() {
        let mut spec = minimal_spec();
        spec.env.insert("NCCL_DEBUG".into(), "INFO".into());
        spec.env.insert("CUDA_VISIBLE_DEVICES".into(), "0,1".into());
        let core = to_core_job_spec(&spec, "user");
        assert_eq!(core.environment.get("NCCL_DEBUG").unwrap(), "INFO");
        assert_eq!(core.environment.get("CUDA_VISIBLE_DEVICES").unwrap(), "0,1");
    }

    // --- TolerationSpec defaults ---

    #[test]
    fn test_toleration_spec_serde_default_operator() {
        // When deserialized from JSON without an "operator" field, it should default to "Equal"
        let json = r#"{"key": "spur.ai/gpu-node"}"#;
        let t: TolerationSpec = serde_json::from_str(json).unwrap();
        assert_eq!(t.operator, "Equal");
        assert_eq!(t.key.as_deref(), Some("spur.ai/gpu-node"));
    }

    // --- SpurJobStatus ---

    #[test]
    fn test_spur_job_status_default() {
        let status = SpurJobStatus::default();
        assert_eq!(status.state, "");
        assert!(status.spur_job_id.is_none());
        assert!(status.assigned_nodes.is_empty());
        assert!(status.pods.is_empty());
        assert!(status.message.is_none());
    }

    // --- CRD generation ---

    #[test]
    fn test_crd_generation() {
        use kube::CustomResourceExt;
        let crd = SpurJob::crd();
        let json = serde_json::to_string(&crd).unwrap();
        assert!(json.contains("spur.ai"));
        assert!(json.contains("SpurJob"));
        assert!(json.contains("v1alpha1"));
    }

    // --- SpurJobSpec JSON roundtrip ---

    #[test]
    fn test_spur_job_spec_json_roundtrip() {
        let spec = SpurJobSpec {
            name: "roundtrip-test".into(),
            image: "ubuntu:22.04".into(),
            gpus: GpuRequirement {
                count: 2,
                gpu_type: Some("h100".into()),
            },
            num_nodes: 2,
            tasks_per_node: 2,
            cpus_per_task: 4,
            memory_per_node: Some("128Gi".into()),
            time_limit: Some("2h".into()),
            command: vec!["bash".into()],
            args: vec!["-c".into(), "echo hello".into()],
            env: [("KEY".to_string(), "VAL".to_string())].into(),
            partition: Some("gpu".into()),
            account: Some("acct".into()),
            volumes: vec!["/data:/data".into()],
            host_network: true,
            privileged: false,
            host_ipc: false,
            shm_size: None,
            extra_resources: std::collections::HashMap::new(),
            secret_env: std::collections::HashMap::new(),
            tolerations: vec![TolerationSpec {
                key: Some("spur.ai/gpu-node".into()),
                operator: "Exists".into(),
                value: None,
                effect: Some("NoSchedule".into()),
                toleration_seconds: None,
            }],
            node_selector: [("zone".to_string(), "us-east".to_string())].into(),
            priority_class: Some("high-priority".into()),
            service_account: Some("trainer".into()),
            array_spec: Some("0-9".into()),
            dependencies: vec!["afterok:5".into()],
        };

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: SpurJobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "roundtrip-test");
        assert_eq!(parsed.gpus.count, 2);
        assert_eq!(parsed.gpus.gpu_type.as_deref(), Some("h100"));
        assert_eq!(parsed.num_nodes, 2);
        assert!(parsed.host_network);
        assert_eq!(parsed.tolerations.len(), 1);
        assert_eq!(
            parsed.tolerations[0].key.as_deref(),
            Some("spur.ai/gpu-node")
        );
        assert_eq!(parsed.tolerations[0].operator, "Exists");
        assert_eq!(parsed.node_selector.get("zone").unwrap(), "us-east");
        assert_eq!(parsed.priority_class.as_deref(), Some("high-priority"));
        assert_eq!(parsed.service_account.as_deref(), Some("trainer"));
        assert_eq!(parsed.array_spec.as_deref(), Some("0-9"));
        assert_eq!(parsed.dependencies, vec!["afterok:5"]);
        assert_eq!(parsed.volumes, vec!["/data:/data"]);
    }

    // --- AMD/ROCm-specific spec ---

    #[test]
    fn test_spur_job_to_core_spec_amd_rocm() {
        let spec = SpurJobSpec {
            name: "rocm-training".into(),
            image: "rocm/pytorch:rocm6.2_ubuntu22.04_py3.10_pytorch_release_2.3.0".into(),
            gpus: GpuRequirement {
                count: 8,
                gpu_type: Some("mi300x".into()),
            },
            num_nodes: 2,
            tasks_per_node: 8,
            cpus_per_task: 16,
            memory_per_node: Some("512Gi".into()),
            time_limit: Some("12h".into()),
            command: vec!["torchrun".into(), "--nproc_per_node=8".into()],
            args: vec!["train.py".into()],
            env: [
                ("NCCL_SOCKET_IFNAME".to_string(), "eth0".to_string()),
                ("GPU_ENABLE_PAL".to_string(), "0".to_string()),
            ]
            .into(),
            partition: Some("mi300x".into()),
            account: None,
            volumes: vec![
                "/data:/data:ro".into(),
                "pvc:checkpoints:/checkpoints".into(),
            ],
            host_network: false,
            privileged: false,
            host_ipc: false,
            shm_size: None,
            extra_resources: std::collections::HashMap::new(),
            secret_env: std::collections::HashMap::new(),
            tolerations: vec![TolerationSpec {
                key: Some("spur.ai/gpu-node".into()),
                operator: "Exists".into(),
                value: None,
                effect: Some("NoSchedule".into()),
                toleration_seconds: None,
            }],
            node_selector: Default::default(),
            priority_class: None,
            service_account: None,
            array_spec: None,
            dependencies: vec![],
        };

        let core = to_core_job_spec(&spec, "researcher");
        assert_eq!(core.name, "rocm-training");
        assert_eq!(core.user, "researcher");
        assert_eq!(core.gres, vec!["gpu:mi300x:8"]);
        assert_eq!(core.num_nodes, 2);
        assert_eq!(core.num_tasks, 16); // 2 * 8
        assert_eq!(core.memory_per_node_mb, Some(524288)); // 512Gi
        assert!(core
            .container_image
            .as_ref()
            .unwrap()
            .contains("rocm/pytorch"));
        assert_eq!(core.environment.get("GPU_ENABLE_PAL").unwrap(), "0");
        assert_eq!(core.container_mounts.len(), 2);
        assert_eq!(core.partition, Some("mi300x".into()));
    }

    // --- SpurJobSpec defaults via JSON ---

    #[test]
    fn test_spur_job_spec_minimal_json() {
        let json = r#"{
            "name": "simple",
            "image": "busybox"
        }"#;
        let spec: SpurJobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.name, "simple");
        assert_eq!(spec.image, "busybox");
        assert_eq!(spec.num_nodes, 1);
        assert_eq!(spec.tasks_per_node, 1);
        assert_eq!(spec.cpus_per_task, 1);
        assert_eq!(spec.gpus.count, 0);
        assert!(spec.gpus.gpu_type.is_none());
        assert!(!spec.host_network);
        assert!(spec.tolerations.is_empty());
        assert!(spec.node_selector.is_empty());
        assert!(spec.volumes.is_empty());
        assert!(spec.dependencies.is_empty());
    }
}
