// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller policy for where a batch script runs.

use std::collections::HashSet;

use spur_core::job::JobSpec;
use spur_core::node::{Node, NodeSource};
use spur_core::script_placement::{ResolvedPlacement, ScriptPlacement};

pub const BATCH_SCRIPT_K8S_UNSUPPORTED: &str =
    "batch scripts cannot run on K8s virtual agents, which launch only SpurJobs";

/// Why a run cannot be dispatched with the placement it resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum PlacementRefusal {
    /// The nodes can never honor it; hold the job until the user changes it.
    Hold(String),
    /// Some agent is too old to honor it; retry once it is upgraded.
    Backoff(String),
}

/// Where a run's batch script runs on `nodes`. K8s virtual agents launch only
/// SpurJobs, which carry no script, so a batch run that includes one is refused.
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
    if nodes.iter().any(Node::is_kubernetes) {
        return Err(PlacementRefusal::Hold(BATCH_SCRIPT_K8S_UNSUPPORTED.into()));
    }
    let resolved = spec
        .script_placement
        .unwrap_or(cluster_default)
        .resolve(script);
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

/// Reject at submit a batch job whose `--nodelist` leaves too few native nodes: an
/// additive list must all be used, and a candidate pool must hold enough.
pub fn validate_batch_nodelist(
    spec: &JobSpec,
    node_source: impl Fn(&str) -> Option<NodeSource>,
) -> Result<(), String> {
    if !spec.needs_native_nodes() {
        return Ok(());
    }
    let Some(nodelist) = spec.nodelist.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let listed: HashSet<String> = spur_sched::node_match::expand_hostlist_or_split(nodelist)
        .into_iter()
        .collect();
    let k8s = listed
        .iter()
        .filter(|name| matches!(node_source(name), Some(NodeSource::Kubernetes { .. })))
        .count();
    let needed = spec.num_nodes as usize;
    let impossible = if listed.len() < needed {
        k8s > 0
    } else {
        listed.len() - k8s < needed
    };
    if impossible {
        return Err(BATCH_SCRIPT_K8S_UNSUPPORTED.into());
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
    fn a_batch_run_that_includes_a_k8s_node_is_held() {
        let held = Err(PlacementRefusal::Hold(BATCH_SCRIPT_K8S_UNSUPPORTED.into()));

        assert_eq!(
            resolve_run_placement(
                &spec("srun ./train\n", None),
                &[current("n1"), k8s("k1")],
                ScriptPlacement::Auto
            ),
            held
        );
        assert_eq!(
            resolve_run_placement(
                &spec("./train\n", None),
                &[k8s("k1")],
                ScriptPlacement::Auto
            ),
            held,
            "even on one node"
        );
    }

    #[test]
    fn a_spurjob_on_k8s_virtual_agents_runs_on_every_node() {
        let spurjob = JobSpec {
            argv: vec!["./train".into()],
            num_nodes: 2,
            ..Default::default()
        };

        assert_eq!(
            resolve_run_placement(
                &spurjob,
                &[k8s("k1"), k8s("k2")],
                ScriptPlacement::BatchHost
            ),
            Ok(ResolvedPlacement::AllNodes),
            "a SpurJob carries no script, so no placement applies to it"
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
    fn submit_rejects_only_a_nodelist_that_leaves_too_few_native_nodes() {
        let source = |name: &str| match name {
            "k1" | "k2" => Some(NodeSource::Kubernetes {
                namespace: "spur".into(),
            }),
            _ => Some(NodeSource::NativeHost),
        };
        let pinned = |nodelist: &str, num_nodes| JobSpec {
            num_nodes,
            nodelist: Some(nodelist.into()),
            ..spec("./runner.sh\n", None)
        };
        let check =
            |nodelist, num_nodes| validate_batch_nodelist(&pinned(nodelist, num_nodes), source);

        let rejected = Err(BATCH_SCRIPT_K8S_UNSUPPORTED.to_string());
        assert_eq!(check("k[1-2]", 2), rejected);
        assert_eq!(check("k1,n1", 2), rejected, "a pool with one native node");
        assert_eq!(check("k1", 2), rejected, "an additive list must use k1");
        assert_eq!(check("k1", 1), rejected, "a single K8s node too");
        assert!(check("n1,n2", 2).is_ok());
        assert!(
            check("k1,n1,n2", 2).is_ok(),
            "the pool holds two native nodes"
        );
        assert!(
            check("n1", 2).is_ok(),
            "the rest may come from native nodes"
        );
        let spurjob = JobSpec {
            script: None,
            argv: vec!["./train".into()],
            ..pinned("k1,k2", 2)
        };
        assert!(validate_batch_nodelist(&spurjob, source).is_ok());
    }
}
