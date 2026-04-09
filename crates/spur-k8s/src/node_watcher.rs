use std::pin::pin;
use std::sync::Arc;

use futures_util::TryStreamExt;
use k8s_openapi::api::core::v1::Node as K8sNode;
use kube::api::Api;
use kube::runtime::watcher::{self, Event};
use kube::Client;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{RegisterAgentRequest, ResourceSet, UpdateNodeRequest};

use crate::heartbeat::HeartbeatManager;

/// Watch K8s nodes matching `label_selector`, register them with spurctld, and
/// keep `hb` in sync so the heartbeat task knows which nodes to ping.
pub async fn run(
    client: Client,
    controller_addr: String,
    operator_grpc_addr: String,
    operator_grpc_port: u32,
    _namespace: String,
    label_selector: String,
    hb: Arc<HeartbeatManager>,
) -> anyhow::Result<()> {
    let nodes: Api<K8sNode> = Api::all(client);

    info!(selector = %label_selector, "starting K8s node watcher");

    let mut ctrl_client = connect_controller(&controller_addr).await?;

    let stream = watcher::watcher(nodes, watcher::Config::default().labels(&label_selector));
    let mut stream = pin!(stream);

    while let Some(event) = stream.try_next().await? {
        match event {
            Event::Apply(node) | Event::InitApply(node) => {
                let name = node.metadata.name.clone().unwrap_or_default();

                // Check if node is not-ready via taints
                if is_node_not_ready(&node) {
                    warn!(node = %name, "K8s node has NotReady taint, marking DOWN");
                    hb.untrack(&name).await;
                    let req = UpdateNodeRequest {
                        name: name.clone(),
                        state: Some(3), // NODE_DOWN
                        reason: Some("K8s node NotReady".into()),
                    };
                    if let Err(e) = ctrl_client.update_node(req).await {
                        error!(node = %name, error = %e, "failed to mark node DOWN");
                    }
                    continue;
                }

                let resources = extract_resources(&node);

                info!(node = %name, cpus = resources.cpus, memory_mb = resources.memory_mb, gpus = resources.gpus.len(), "registering K8s node");

                let req = RegisterAgentRequest {
                    hostname: name.clone(),
                    resources: Some(resources),
                    version: "spur-k8s-operator".into(),
                    address: operator_grpc_addr.clone(),
                    port: operator_grpc_port,
                    wg_pubkey: String::new(),
                };

                match ctrl_client.register_agent(req.clone()).await {
                    Ok(_) => {
                        debug!(node = %name, "K8s node registered with spurctld");
                        hb.track(name, req).await;
                    }
                    Err(e) => error!(node = %name, error = %e, "failed to register K8s node"),
                }
            }
            Event::Delete(node) => {
                let name = node.metadata.name.clone().unwrap_or_default();
                warn!(node = %name, "K8s node deleted, marking DOWN");
                hb.untrack(&name).await;

                let req = UpdateNodeRequest {
                    name: name.clone(),
                    state: Some(3), // NODE_DOWN
                    reason: Some("K8s node removed".into()),
                };

                if let Err(e) = ctrl_client.update_node(req).await {
                    error!(node = %name, error = %e, "failed to mark K8s node DOWN");
                }
            }
            Event::Init => {
                debug!("node watcher init bookmark");
            }
            Event::InitDone => {
                info!("node watcher initial list complete");
            }
        }
    }

    Ok(())
}

/// Check if a K8s node has the not-ready taint.
fn is_node_not_ready(node: &K8sNode) -> bool {
    node.spec
        .as_ref()
        .and_then(|s| s.taints.as_ref())
        .map_or(false, |taints| {
            taints
                .iter()
                .any(|t| t.key == "node.kubernetes.io/not-ready" && t.effect == "NoSchedule")
        })
}

/// Extract CPU, memory, and GPU resources from a K8s Node's allocatable.
fn extract_resources(node: &K8sNode) -> ResourceSet {
    let allocatable = node.status.as_ref().and_then(|s| s.allocatable.as_ref());

    let cpus = allocatable
        .and_then(|a| a.get("cpu"))
        .and_then(|q| q.0.parse::<u32>().ok())
        .unwrap_or(0);

    let memory_mb = allocatable
        .and_then(|a| a.get("memory"))
        .and_then(|q| crate::crd::parse_k8s_memory_to_mb(&q.0))
        .unwrap_or(0);

    // Check for AMD or NVIDIA GPUs (AMD first — ROCm project default)
    let (gpu_count, gpu_vendor) = if let Some(alloc) = allocatable {
        if let Some(q) = alloc.get("amd.com/gpu") {
            (q.0.parse::<u32>().unwrap_or(0), "amd")
        } else if let Some(q) = alloc.get("nvidia.com/gpu") {
            (q.0.parse::<u32>().unwrap_or(0), "nvidia")
        } else {
            (0, "unknown")
        }
    } else {
        (0, "unknown")
    };

    let labels = node.metadata.labels.as_ref();

    // Use explicit label if set, otherwise default based on detected vendor
    let gpu_type = labels
        .and_then(|l| l.get("spur.ai/gpu-type"))
        .cloned()
        .unwrap_or_else(|| match gpu_vendor {
            "amd" => "amd-gpu".into(),
            "nvidia" => "nvidia-gpu".into(),
            _ => "gpu".into(),
        });

    // Read GPU memory from label if available
    let gpu_memory_mb: u64 = labels
        .and_then(|l| l.get("spur.ai/gpu-memory-mb"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Read link type from label
    let link_type: i32 = labels
        .and_then(|l| l.get("spur.ai/gpu-link"))
        .map(|v| match v.as_str() {
            "xgmi" | "XGMI" => 1,     // GPU_LINK_XGMI
            "nvlink" | "NVLink" => 2, // GPU_LINK_NVLINK
            _ => 0,                   // GPU_LINK_PCIE
        })
        .unwrap_or(0);

    let gpus: Vec<spur_proto::proto::GpuResource> = (0..gpu_count)
        .map(|i| spur_proto::proto::GpuResource {
            device_id: i,
            gpu_type: gpu_type.clone(),
            memory_mb: gpu_memory_mb,
            peer_gpus: (0..gpu_count).filter(|&j| j != i).collect(),
            link_type,
        })
        .collect();

    ResourceSet {
        cpus,
        memory_mb,
        gpus,
        generic: Default::default(),
    }
}

async fn connect_controller(addr: &str) -> anyhow::Result<SlurmControllerClient<Channel>> {
    let url = if addr.starts_with("http") {
        addr.to_string()
    } else {
        format!("http://{}", addr)
    };
    let client = SlurmControllerClient::connect(url).await?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{NodeSpec, NodeStatus, Taint};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use std::collections::BTreeMap;

    fn make_node(
        name: &str,
        labels: BTreeMap<String, String>,
        allocatable: BTreeMap<String, Quantity>,
        taints: Vec<Taint>,
    ) -> K8sNode {
        K8sNode {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(NodeSpec {
                taints: if taints.is_empty() {
                    None
                } else {
                    Some(taints)
                },
                ..Default::default()
            }),
            status: Some(NodeStatus {
                allocatable: Some(allocatable),
                ..Default::default()
            }),
        }
    }

    // --- is_node_not_ready ---

    #[test]
    fn test_is_node_not_ready_with_taint() {
        let node = make_node(
            "node-1",
            BTreeMap::new(),
            BTreeMap::new(),
            vec![Taint {
                key: "node.kubernetes.io/not-ready".into(),
                effect: "NoSchedule".into(),
                ..Default::default()
            }],
        );
        assert!(is_node_not_ready(&node));
    }

    #[test]
    fn test_is_node_not_ready_without_taint() {
        let node = make_node("node-1", BTreeMap::new(), BTreeMap::new(), vec![]);
        assert!(!is_node_not_ready(&node));
    }

    #[test]
    fn test_is_node_not_ready_wrong_effect() {
        let node = make_node(
            "node-1",
            BTreeMap::new(),
            BTreeMap::new(),
            vec![Taint {
                key: "node.kubernetes.io/not-ready".into(),
                effect: "PreferNoSchedule".into(),
                ..Default::default()
            }],
        );
        assert!(!is_node_not_ready(&node));
    }

    #[test]
    fn test_is_node_not_ready_wrong_key() {
        let node = make_node(
            "node-1",
            BTreeMap::new(),
            BTreeMap::new(),
            vec![Taint {
                key: "node.kubernetes.io/disk-pressure".into(),
                effect: "NoSchedule".into(),
                ..Default::default()
            }],
        );
        assert!(!is_node_not_ready(&node));
    }

    #[test]
    fn test_is_node_not_ready_among_multiple_taints() {
        let node = make_node(
            "node-1",
            BTreeMap::new(),
            BTreeMap::new(),
            vec![
                Taint {
                    key: "spur.ai/gpu-node".into(),
                    effect: "NoSchedule".into(),
                    ..Default::default()
                },
                Taint {
                    key: "node.kubernetes.io/not-ready".into(),
                    effect: "NoSchedule".into(),
                    ..Default::default()
                },
            ],
        );
        assert!(is_node_not_ready(&node));
    }

    #[test]
    fn test_is_node_not_ready_no_spec() {
        let node = K8sNode {
            metadata: Default::default(),
            spec: None,
            status: None,
        };
        assert!(!is_node_not_ready(&node));
    }

    // --- extract_resources ---

    #[test]
    fn test_extract_resources_basic() {
        let mut alloc = BTreeMap::new();
        alloc.insert("cpu".into(), Quantity("64".into()));
        alloc.insert("memory".into(), Quantity("262144Mi".into()));

        let node = make_node("node-1", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.cpus, 64);
        assert_eq!(res.memory_mb, 262144);
        assert!(res.gpus.is_empty());
    }

    #[test]
    fn test_extract_resources_amd_gpus() {
        let mut alloc = BTreeMap::new();
        alloc.insert("cpu".into(), Quantity("32".into()));
        alloc.insert("memory".into(), Quantity("128Gi".into()));
        alloc.insert("amd.com/gpu".into(), Quantity("8".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-type".into(), "mi300x".into());

        let node = make_node("gpu-node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 8);
        for (i, gpu) in res.gpus.iter().enumerate() {
            assert_eq!(gpu.device_id, i as u32);
            assert_eq!(gpu.gpu_type, "mi300x");
        }
    }

    #[test]
    fn test_extract_resources_nvidia_gpus() {
        let mut alloc = BTreeMap::new();
        alloc.insert("cpu".into(), Quantity("16".into()));
        alloc.insert("memory".into(), Quantity("64Gi".into()));
        alloc.insert("nvidia.com/gpu".into(), Quantity("4".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-type".into(), "h100".into());

        let node = make_node("nvidia-node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 4);
        assert_eq!(res.gpus[0].gpu_type, "h100");
    }

    #[test]
    fn test_extract_resources_amd_gpu_default_type() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("2".into()));

        // No gpu-type label → should default to "amd-gpu" for AMD devices
        let node = make_node("unlabeled", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 2);
        assert_eq!(res.gpus[0].gpu_type, "amd-gpu");
    }

    #[test]
    fn test_extract_resources_nvidia_gpu_default_type() {
        let mut alloc = BTreeMap::new();
        alloc.insert("nvidia.com/gpu".into(), Quantity("2".into()));

        // No gpu-type label → should default to "nvidia-gpu" for NVIDIA devices
        let node = make_node("unlabeled", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 2);
        assert_eq!(res.gpus[0].gpu_type, "nvidia-gpu");
    }

    #[test]
    fn test_extract_resources_gpu_memory_from_label() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("4".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-type".into(), "mi300x".into());
        labels.insert("spur.ai/gpu-memory-mb".into(), "196608".into()); // 192Gi

        let node = make_node("gpu-node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 4);
        for gpu in &res.gpus {
            assert_eq!(gpu.memory_mb, 196608);
        }
    }

    #[test]
    fn test_extract_resources_gpu_memory_default_zero() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("1".into()));

        let node = make_node("node", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus[0].memory_mb, 0);
    }

    #[test]
    fn test_extract_resources_gpu_link_xgmi() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("2".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-link".into(), "xgmi".into());

        let node = make_node("node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus[0].link_type, 1); // GPU_LINK_XGMI
    }

    #[test]
    fn test_extract_resources_gpu_link_nvlink() {
        let mut alloc = BTreeMap::new();
        alloc.insert("nvidia.com/gpu".into(), Quantity("2".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-link".into(), "NVLink".into());

        let node = make_node("node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus[0].link_type, 2); // GPU_LINK_NVLINK
    }

    #[test]
    fn test_extract_resources_gpu_link_default_pcie() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("1".into()));

        let node = make_node("node", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus[0].link_type, 0); // GPU_LINK_PCIE
    }

    #[test]
    fn test_extract_resources_gpu_link_unknown_defaults_pcie() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("1".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-link".into(), "something-else".into());

        let node = make_node("node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus[0].link_type, 0); // defaults to PCIE
    }

    #[test]
    fn test_extract_resources_peer_gpus() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("3".into()));

        let node = make_node("node", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        // GPU 0 should have peers [1, 2]
        assert_eq!(res.gpus[0].peer_gpus, vec![1, 2]);
        // GPU 1 should have peers [0, 2]
        assert_eq!(res.gpus[1].peer_gpus, vec![0, 2]);
        // GPU 2 should have peers [0, 1]
        assert_eq!(res.gpus[2].peer_gpus, vec![0, 1]);
    }

    #[test]
    fn test_extract_resources_single_gpu_no_peers() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("1".into()));

        let node = make_node("node", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        assert!(res.gpus[0].peer_gpus.is_empty());
    }

    #[test]
    fn test_extract_resources_no_allocatable() {
        let node = K8sNode {
            metadata: Default::default(),
            spec: None,
            status: None,
        };
        let res = extract_resources(&node);
        assert_eq!(res.cpus, 0);
        assert_eq!(res.memory_mb, 0);
        assert!(res.gpus.is_empty());
    }

    #[test]
    fn test_extract_resources_empty_allocatable() {
        let node = make_node("empty", BTreeMap::new(), BTreeMap::new(), vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.cpus, 0);
        assert_eq!(res.memory_mb, 0);
        assert!(res.gpus.is_empty());
    }

    #[test]
    fn test_extract_resources_prefers_amd_over_nvidia() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("8".into()));
        alloc.insert("nvidia.com/gpu".into(), Quantity("4".into()));

        let node = make_node("both", BTreeMap::new(), alloc, vec![]);
        let res = extract_resources(&node);
        // amd.com/gpu is checked first — ROCm project default
        assert_eq!(res.gpus.len(), 8);
        assert_eq!(res.gpus[0].gpu_type, "amd-gpu");
    }

    #[test]
    fn test_extract_resources_amd_explicit_label_mi300x() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("8".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-type".into(), "mi300x".into());
        labels.insert("spur.ai/gpu-memory-mb".into(), "196608".into());
        labels.insert("spur.ai/gpu-link".into(), "xgmi".into());

        let node = make_node("mi300x-node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 8);
        for gpu in &res.gpus {
            assert_eq!(gpu.gpu_type, "mi300x");
            assert_eq!(gpu.memory_mb, 196608);
            assert_eq!(gpu.link_type, 1); // XGMI
        }
        // 8 GPUs each have 7 peers
        assert_eq!(res.gpus[0].peer_gpus.len(), 7);
    }

    #[test]
    fn test_extract_resources_amd_mi250x() {
        let mut alloc = BTreeMap::new();
        alloc.insert("amd.com/gpu".into(), Quantity("4".into()));

        let mut labels = BTreeMap::new();
        labels.insert("spur.ai/gpu-type".into(), "mi250x".into());
        labels.insert("spur.ai/gpu-memory-mb".into(), "131072".into());
        labels.insert("spur.ai/gpu-link".into(), "xgmi".into());

        let node = make_node("mi250x-node", labels, alloc, vec![]);
        let res = extract_resources(&node);
        assert_eq!(res.gpus.len(), 4);
        assert_eq!(res.gpus[0].gpu_type, "mi250x");
        assert_eq!(res.gpus[0].memory_mb, 131072);
        assert_eq!(res.gpus[0].link_type, 1);
    }
}
