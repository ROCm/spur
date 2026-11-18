// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller policy for where a batch script runs.

use spur_core::job::JobSpec;
use spur_core::node::{Node, NodeSource};
use spur_core::script_placement::{ResolvedPlacement, ScriptPlacement};

pub const BATCH_HOST_K8S_UNSUPPORTED: &str =
    "batch-host script placement is not supported on K8s virtual agents";

/// Why a run cannot be dispatched with the placement it resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum PlacementRefusal {
    /// The nodes can never honor it; hold the job until the user changes it.
    Hold(String),
    /// Some agent is too old to honor it; retry once it is upgraded.
    Backoff(String),
}

/// Where a run's batch script runs on `nodes`. K8s virtual agents run a Pod per
/// node and cannot host steps, so a run on them always places on every node.
pub fn resolve_run_placement(
    spec: &JobSpec,
    nodes: &[Node],
    cluster_default: ScriptPlacement,
) -> Result<ResolvedPlacement, PlacementRefusal> {
    let script = spec.script.as_deref();
    let scanned = ResolvedPlacement::scanned(script);
    if !spec.is_batch() {
        return Ok(scanned);
    }
    let resolved = if nodes.iter().any(Node::is_kubernetes) {
        if spec.script_placement == Some(ScriptPlacement::BatchHost) && nodes.len() > 1 {
            return Err(PlacementRefusal::Hold(BATCH_HOST_K8S_UNSUPPORTED.into()));
        }
        ResolvedPlacement::AllNodes
    } else {
        spec.script_placement
            .unwrap_or(cluster_default)
            .resolve(script)
    };
    let outdated: Vec<&str> = nodes
        .iter()
        .filter(|node| node.reads_placement_off_script())
        .map(|node| node.name.as_str())
        .collect();
    if !outdated.is_empty()
        && resolved.launches_differently_from(scanned, nodes.len(), spec.mpi.as_deref())
    {
        return Err(PlacementRefusal::Backoff(format!(
            "{resolved} script placement needs a newer spurd on {}",
            outdated.join(",")
        )));
    }
    Ok(resolved)
}

/// Reject at submit an explicit batch-host job pinned to K8s virtual agents.
pub fn validate_script_placement_nodelist(
    spec: &JobSpec,
    node_source: impl Fn(&str) -> Option<NodeSource>,
) -> Result<(), String> {
    if spec.script_placement != Some(ScriptPlacement::BatchHost)
        || spec.num_nodes <= 1
        || !spec.is_batch()
    {
        return Ok(());
    }
    let Some(nodelist) = spec.nodelist.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let pins_k8s = spur_sched::node_match::expand_hostlist_or_split(nodelist)
        .iter()
        .any(|name| {
            node_source(name).is_some_and(|source| matches!(source, NodeSource::Kubernetes { .. }))
        });
    if pins_k8s {
        return Err(BATCH_HOST_K8S_UNSUPPORTED.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::node::CAP_SCRIPT_PLACEMENT;

    fn spec(script: &str, requested: Option<ScriptPlacement>) -> JobSpec {
        JobSpec {
            script: Some(script.into()),
            script_placement: requested,
            ..Default::default()
        }
    }

    fn node(name: &str, source: NodeSource, capabilities: &[&str]) -> Node {
        let mut node = Node::new(name.into(), Default::default());
        node.source = source;
        node.capabilities = capabilities.iter().map(|c| c.to_string()).collect();
        node
    }

    fn current(name: &str) -> Node {
        node(name, NodeSource::NativeHost, &[CAP_SCRIPT_PLACEMENT])
    }

    fn k8s(name: &str) -> Node {
        let source = NodeSource::Kubernetes {
            namespace: "spur".into(),
        };
        node(name, source, &[])
    }

    #[test]
    fn an_unset_placement_takes_the_cluster_default() {
        let nodes = [current("n1"), current("n2")];
        let plain = spec("./train\n", None);
        let driver = spec("srun ./train\n", None);

        assert_eq!(
            resolve_run_placement(&plain, &nodes, ScriptPlacement::Auto),
            Ok(ResolvedPlacement::AllNodes)
        );
        assert_eq!(
            resolve_run_placement(&driver, &nodes, ScriptPlacement::Auto),
            Ok(ResolvedPlacement::BatchHost)
        );
        assert_eq!(
            resolve_run_placement(&plain, &nodes, ScriptPlacement::BatchHost),
            Ok(ResolvedPlacement::BatchHost)
        );
    }

    #[test]
    fn a_requested_placement_beats_the_cluster_default() {
        let nodes = [current("n1"), current("n2")];

        let per_node = spec("srun ./train\n", Some(ScriptPlacement::AllNodes));
        assert_eq!(
            resolve_run_placement(&per_node, &nodes, ScriptPlacement::BatchHost),
            Ok(ResolvedPlacement::AllNodes)
        );
        let runner = spec("./runner.sh\n", Some(ScriptPlacement::BatchHost));
        assert_eq!(
            resolve_run_placement(&runner, &nodes, ScriptPlacement::Auto),
            Ok(ResolvedPlacement::BatchHost)
        );
    }

    #[test]
    fn srun_and_salloc_jobs_keep_reading_their_script() {
        let nodes = [current("n1"), current("n2")];

        let mut srun = spec("hostname\n", Some(ScriptPlacement::BatchHost));
        srun.srun_job = true;
        assert_eq!(
            resolve_run_placement(&srun, &nodes, ScriptPlacement::BatchHost),
            Ok(ResolvedPlacement::AllNodes)
        );
        let mut salloc = spec("sleep infinity\n", None);
        salloc.interactive = true;
        assert_eq!(
            resolve_run_placement(&salloc, &nodes, ScriptPlacement::BatchHost),
            Ok(ResolvedPlacement::AllNodes)
        );
    }

    #[test]
    fn k8s_virtual_agents_run_the_script_on_every_node() {
        let nodes = [k8s("k1"), k8s("k2")];

        assert_eq!(
            resolve_run_placement(
                &spec("srun ./train\n", None),
                &nodes,
                ScriptPlacement::BatchHost
            ),
            Ok(ResolvedPlacement::AllNodes),
            "neither the script nor the cluster default moves a K8s job to one node"
        );
    }

    #[test]
    fn an_explicit_batch_host_on_k8s_virtual_agents_is_held() {
        let runner = spec("./runner.sh\n", Some(ScriptPlacement::BatchHost));

        assert_eq!(
            resolve_run_placement(&runner, &[current("n1"), k8s("k1")], ScriptPlacement::Auto),
            Err(PlacementRefusal::Hold(BATCH_HOST_K8S_UNSUPPORTED.into()))
        );
        assert_eq!(
            resolve_run_placement(&runner, &[k8s("k1")], ScriptPlacement::Auto),
            Ok(ResolvedPlacement::AllNodes),
            "on one node both placements run the script once"
        );
    }

    #[test]
    fn an_agent_that_predates_placement_blocks_one_its_scan_would_not_reach() {
        let nodes = [current("n1"), node("n2", NodeSource::NativeHost, &[])];

        match resolve_run_placement(
            &spec("./runner.sh\n", Some(ScriptPlacement::BatchHost)),
            &nodes,
            ScriptPlacement::Auto,
        ) {
            Err(PlacementRefusal::Backoff(detail)) => {
                assert!(detail.contains("n2") && !detail.contains("n1"), "{detail}");
            }
            other => panic!("expected a backoff, got {other:?}"),
        }
        assert_eq!(
            resolve_run_placement(
                &spec("srun ./train\n", Some(ScriptPlacement::BatchHost)),
                &nodes,
                ScriptPlacement::Auto
            ),
            Ok(ResolvedPlacement::BatchHost),
            "the old agent's scan places this script the same way"
        );
    }

    #[test]
    fn an_agent_that_predates_placement_still_runs_a_job_it_launches_alike() {
        let old = [node("n1", NodeSource::NativeHost, &[])];
        let runner = spec("./runner.sh\n", Some(ScriptPlacement::BatchHost));

        assert_eq!(
            resolve_run_placement(&runner, &old, ScriptPlacement::Auto),
            Ok(ResolvedPlacement::BatchHost),
            "on one node both placements run the script once"
        );
        let pmix = JobSpec {
            mpi: Some("pmix".into()),
            ..runner
        };
        assert!(
            matches!(
                resolve_run_placement(&pmix, &old, ScriptPlacement::Auto),
                Err(PlacementRefusal::Backoff(_))
            ),
            "a PMIx batch host launches a rank or a driver depending on the placement"
        );
    }

    #[test]
    fn submit_rejects_a_batch_host_job_pinned_to_k8s_virtual_agents() {
        let source = |name: &str| match name {
            "k1" | "k2" => Some(NodeSource::Kubernetes {
                namespace: "spur".into(),
            }),
            _ => Some(NodeSource::NativeHost),
        };
        let pinned = |nodelist: &str, requested| JobSpec {
            num_nodes: 2,
            nodelist: Some(nodelist.into()),
            ..spec("./runner.sh\n", requested)
        };

        assert_eq!(
            validate_script_placement_nodelist(
                &pinned("k[1-2]", Some(ScriptPlacement::BatchHost)),
                source
            ),
            Err(BATCH_HOST_K8S_UNSUPPORTED.to_string())
        );
        assert!(validate_script_placement_nodelist(
            &pinned("n1,n2", Some(ScriptPlacement::BatchHost)),
            source
        )
        .is_ok());
        assert!(validate_script_placement_nodelist(&pinned("k1,k2", None), source).is_ok());
        assert!(validate_script_placement_nodelist(
            &JobSpec {
                num_nodes: 1,
                ..pinned("k1", Some(ScriptPlacement::BatchHost))
            },
            source
        )
        .is_ok());
    }
}
