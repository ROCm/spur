// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! MPI launch planning and `--mpi` validation helpers.

/// One process entry in a PMIx launch plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmixLocalProc {
    pub rank: u32,
    pub local_rank: u32,
}

/// Controller-derived PMIx bootstrap payload for a single agent dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmixLaunchPlan {
    pub job_id: u32,
    pub namespace: String,
    pub universe_size: u32,
    pub task_offset: u32,
    pub local_procs: Vec<PmixLocalProc>,
    pub tmpdir: String,
    pub job_uid: u32,
    pub job_gid: u32,
    pub num_nodes: u32,
    pub node_index: u32,
    pub peer_hosts: Vec<String>,
    pub modex_connect_timeout_secs: u32,
    pub modex_fence_timeout_secs: u32,
    pub modex_verify_timeout_secs: u32,
}

impl PmixLaunchPlan {
    pub fn namespace_for_job(job_id: u32) -> String {
        format!("spur.{job_id}")
    }

    /// Build a plan for all tasks running locally on one agent.
    #[allow(clippy::too_many_arguments)]
    pub fn local_tasks(
        job_id: u32,
        universe_size: u32,
        task_offset: u32,
        local_count: u32,
        tmpdir: impl Into<String>,
        job_uid: u32,
        job_gid: u32,
        num_nodes: u32,
        node_index: u32,
        peer_hosts: Vec<String>,
    ) -> Self {
        let local_procs = (0..local_count)
            .map(|local_rank| PmixLocalProc {
                rank: task_offset + local_rank,
                local_rank,
            })
            .collect();
        Self {
            job_id,
            namespace: Self::namespace_for_job(job_id),
            universe_size,
            task_offset,
            local_procs,
            tmpdir: tmpdir.into(),
            job_uid,
            job_gid,
            num_nodes: num_nodes.max(1),
            node_index,
            peer_hosts,
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        }
    }

    pub fn with_modex_timeouts(
        mut self,
        connect_secs: u32,
        fence_secs: u32,
        verify_secs: u32,
    ) -> Self {
        self.modex_connect_timeout_secs = connect_secs;
        self.modex_fence_timeout_secs = fence_secs;
        self.modex_verify_timeout_secs = verify_secs;
        self
    }
}

/// Supported `--mpi` values (excluding the special `list` keyword).
pub const MPI_NONE: &str = "none";
pub const MPI_PMIX: &str = "pmix";

/// Max bytes for PMIx namespace strings passed to the C plugin (NUL excluded).
pub const PMIX_NAMESPACE_MAX: usize = 255;
/// Max bytes for PMIx tmpdir strings passed to the C plugin (NUL excluded).
pub const PMIX_TMPDIR_MAX: usize = 511;
/// Max peer host entries passed to the C plugin.
pub const PMIX_MAX_PEER_HOSTS: usize = 64;
/// Max bytes for peer host strings passed to the C plugin (NUL excluded).
pub const PMIX_PEER_HOST_MAX: usize = 255;

/// Validate a PMIx launch plan before calling the agent plugin.
pub fn validate_pmix_plan(plan: &PmixLaunchPlan) -> Result<(), String> {
    if plan.namespace.is_empty() {
        return Err("PMIx namespace must not be empty".into());
    }
    if plan.namespace.len() > PMIX_NAMESPACE_MAX {
        return Err(format!("PMIx namespace exceeds {PMIX_NAMESPACE_MAX} bytes"));
    }
    if plan.tmpdir.is_empty() {
        return Err("PMIx tmpdir must not be empty".into());
    }
    if plan.tmpdir.len() > PMIX_TMPDIR_MAX {
        return Err(format!("PMIx tmpdir exceeds {PMIX_TMPDIR_MAX} bytes"));
    }
    if plan.universe_size == 0 {
        return Err("PMIx universe_size must be > 0".into());
    }
    if plan.local_procs.is_empty() {
        return Err("PMIx launch plan has no local procs".into());
    }
    if plan.local_procs.len() > 256 {
        return Err(format!(
            "PMIx launch plan has {} local procs (max 256)",
            plan.local_procs.len()
        ));
    }
    for (idx, proc) in plan.local_procs.iter().enumerate() {
        let expected_local = idx as u32;
        if proc.local_rank != expected_local {
            return Err(format!(
                "PMIx local proc {idx} has local_rank {} (expected {expected_local})",
                proc.local_rank
            ));
        }
        if proc.rank != plan.task_offset + proc.local_rank {
            return Err(format!(
                "PMIx local proc {idx} rank {} != task_offset + local_rank",
                proc.rank
            ));
        }
    }
    let local_count = plan.local_procs.len() as u32;
    if plan.task_offset.saturating_add(local_count) > plan.universe_size {
        return Err(format!(
            "PMIx local procs exceed universe_size (task_offset {} + {} local > {})",
            plan.task_offset, local_count, plan.universe_size
        ));
    }
    if plan.num_nodes == 0 {
        return Err("PMIx num_nodes must be > 0".into());
    }
    if plan.node_index >= plan.num_nodes {
        return Err(format!(
            "PMIx node_index {} >= num_nodes {}",
            plan.node_index, plan.num_nodes
        ));
    }
    if plan.num_nodes > 1 {
        if !plan.universe_size.is_multiple_of(plan.num_nodes) {
            return Err(format!(
                "PMIx multi-node jobs require uniform tasks per node \
                 (universe_size {0} not divisible by num_nodes {1})",
                plan.universe_size, plan.num_nodes
            ));
        }
        let expected_local = plan.universe_size / plan.num_nodes;
        if local_count != expected_local {
            return Err(format!(
                "PMIx local_count {local_count} != universe_size / num_nodes ({expected_local})"
            ));
        }
        if plan.peer_hosts.len() != plan.num_nodes as usize {
            return Err(format!(
                "PMIx peer_hosts length {} != num_nodes {}",
                plan.peer_hosts.len(),
                plan.num_nodes
            ));
        }
        if plan.peer_hosts.len() > PMIX_MAX_PEER_HOSTS {
            return Err(format!(
                "PMIx peer_hosts exceeds max {}",
                PMIX_MAX_PEER_HOSTS
            ));
        }
        for (idx, host) in plan.peer_hosts.iter().enumerate() {
            if host.is_empty() {
                return Err(format!("PMIx peer_hosts[{idx}] is empty"));
            }
            if host.len() > PMIX_PEER_HOST_MAX {
                return Err(format!(
                    "PMIx peer_hosts[{idx}] exceeds {PMIX_PEER_HOST_MAX} bytes"
                ));
            }
        }
    }
    Ok(())
}

/// Per-agent inputs for building a PMIx launch plan on the controller.
#[derive(Debug, Clone)]
pub struct PmixLocalDispatch {
    pub job_id: u32,
    pub universe_size: u32,
    pub task_offset: u32,
    pub local_count: u32,
    pub tmpdir: String,
    pub job_uid: u32,
    pub job_gid: u32,
    pub num_nodes: u32,
    pub node_index: u32,
    pub peer_hosts: Vec<String>,
    pub modex_connect_timeout_secs: u32,
    pub modex_fence_timeout_secs: u32,
    pub modex_verify_timeout_secs: u32,
}

pub fn maybe_local_pmix_plan(mpi: &str, dispatch: PmixLocalDispatch) -> Option<PmixLaunchPlan> {
    if mpi != MPI_PMIX {
        return None;
    }
    Some(
        PmixLaunchPlan::local_tasks(
            dispatch.job_id,
            dispatch.universe_size,
            dispatch.task_offset,
            dispatch.local_count,
            dispatch.tmpdir,
            dispatch.job_uid,
            dispatch.job_gid,
            dispatch.num_nodes,
            dispatch.node_index,
            dispatch.peer_hosts,
        )
        .with_modex_timeouts(
            dispatch.modex_connect_timeout_secs,
            dispatch.modex_fence_timeout_secs,
            dispatch.modex_verify_timeout_secs,
        ),
    )
}

/// Build and validate a PMIx launch plan for controller dispatch (batch, prepare, srun).
/// Returns `Ok(None)` when `mpi` is not `pmix`.
pub fn build_validated_pmix_plan_proto(
    mpi: &str,
    dispatch: PmixLocalDispatch,
) -> Result<Option<spur_proto::proto::PmixLaunchPlan>, String> {
    let Some(plan) = maybe_local_pmix_plan(mpi, dispatch) else {
        return Ok(None);
    };
    validate_pmix_plan(&plan)?;
    Ok(Some(plan_to_proto(plan)))
}

/// Modex peer set for a PMIx step that may use fewer nodes than the job allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmixStepPeers {
    pub num_nodes: u32,
    pub peer_hosts: Vec<String>,
    modex_index: std::collections::HashMap<u32, u32>,
}

impl PmixStepPeers {
    pub fn modex_node_index(&self, allocation_node_index: u32) -> Option<u32> {
        self.modex_index.get(&allocation_node_index).copied()
    }

    pub fn from_participants(
        mut participant_indices: Vec<u32>,
        host_for_index: impl Fn(u32) -> Option<String>,
    ) -> Result<Self, String> {
        participant_indices.sort_unstable();
        participant_indices.dedup();
        let mut peer_hosts = Vec::with_capacity(participant_indices.len());
        let mut modex_index = std::collections::HashMap::new();
        for (modex_idx, alloc_idx) in participant_indices.iter().enumerate() {
            let host = host_for_index(*alloc_idx).ok_or_else(|| {
                format!("missing agent address for allocation node index {alloc_idx}")
            })?;
            peer_hosts.push(host);
            modex_index.insert(*alloc_idx, modex_idx as u32);
        }
        Ok(Self {
            num_nodes: peer_hosts.len() as u32,
            peer_hosts,
            modex_index,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub fn pmix_local_dispatch_for_step(
    peers: &PmixStepPeers,
    allocation_node_index: u32,
    job_id: u32,
    universe_size: u32,
    task_offset: u32,
    local_count: u32,
    tmpdir: impl Into<String>,
    job_uid: u32,
    job_gid: u32,
    modex_connect_timeout_secs: u32,
    modex_fence_timeout_secs: u32,
    modex_verify_timeout_secs: u32,
) -> Result<PmixLocalDispatch, String> {
    let node_index = peers
        .modex_node_index(allocation_node_index)
        .ok_or_else(|| {
            format!("allocation node index {allocation_node_index} is not in this step")
        })?;
    Ok(PmixLocalDispatch {
        job_id,
        universe_size,
        task_offset,
        local_count,
        tmpdir: tmpdir.into(),
        job_uid,
        job_gid,
        num_nodes: peers.num_nodes,
        node_index,
        peer_hosts: peers.peer_hosts.clone(),
        modex_connect_timeout_secs,
        modex_fence_timeout_secs,
        modex_verify_timeout_secs,
    })
}

/// TCP port used for cross-agent PMIx modex exchange.
pub fn modex_port_for_job(job_id: u32) -> u16 {
    const BASE: u32 = 16819;
    const SPAN: u32 = 8000;
    (BASE + (job_id % SPAN)) as u16
}

/// Parse `--mpi` / `#SBATCH --mpi`. Returns `None` for `list`.
pub fn parse_mpi_option(value: &str) -> Result<Option<String>, String> {
    if value == "list" {
        return Ok(None);
    }
    match value {
        MPI_NONE | MPI_PMIX => Ok(Some(value.to_string())),
        other => Err(format!(
            "invalid --mpi value '{other}' (supported: none, pmix)"
        )),
    }
}

pub fn mpi_list_lines(plugin_dir: &str) -> Vec<String> {
    vec![
        MPI_NONE.to_string(),
        MPI_PMIX.to_string(),
        format!("plugin_dir={plugin_dir}"),
    ]
}

pub fn resolve_step_mpi<'a>(step_mpi: &'a str, job_mpi: &'a str) -> &'a str {
    if step_mpi.is_empty() {
        job_mpi
    } else {
        step_mpi
    }
}

pub fn plan_to_proto(plan: PmixLaunchPlan) -> spur_proto::proto::PmixLaunchPlan {
    spur_proto::proto::PmixLaunchPlan {
        job_id: plan.job_id,
        namespace: plan.namespace,
        universe_size: plan.universe_size,
        task_offset: plan.task_offset,
        local_procs: plan
            .local_procs
            .into_iter()
            .map(|proc| spur_proto::proto::PmixLocalProc {
                rank: proc.rank,
                local_rank: proc.local_rank,
            })
            .collect(),
        tmpdir: plan.tmpdir,
        job_uid: plan.job_uid,
        job_gid: plan.job_gid,
        num_nodes: plan.num_nodes,
        node_index: plan.node_index,
        peer_hosts: plan.peer_hosts,
        modex_connect_timeout_secs: plan.modex_connect_timeout_secs,
        modex_fence_timeout_secs: plan.modex_fence_timeout_secs,
        modex_verify_timeout_secs: plan.modex_verify_timeout_secs,
    }
}

/// Compare dotted version tokens (e.g. `4.1.0` >= `4.1.0`).
pub fn version_at_least(runtime: &str, required: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    let runtime_parts = parse(runtime);
    let required_parts = parse(required);
    let len = runtime_parts.len().max(required_parts.len());
    for idx in 0..len {
        let got = *runtime_parts.get(idx).unwrap_or(&0);
        let need = *required_parts.get(idx).unwrap_or(&0);
        if got > need {
            return true;
        }
        if got < need {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_tasks_plan() {
        let plan = PmixLaunchPlan::local_tasks(
            42,
            4,
            0,
            4,
            "/tmp/pmix",
            1000,
            1000,
            1,
            0,
            vec!["10.0.0.1".into()],
        );
        assert_eq!(plan.namespace, "spur.42");
        assert_eq!(plan.universe_size, 4);
        assert_eq!(plan.local_procs.len(), 4);
        assert_eq!(plan.local_procs[0].rank, 0);
        assert_eq!(plan.local_procs[3].rank, 3);
        assert_eq!(plan.job_uid, 1000);
        assert_eq!(plan.job_gid, 1000);
        let proto = plan_to_proto(plan);
        assert_eq!(proto.job_uid, 1000);
        assert_eq!(proto.job_gid, 1000);
    }

    #[test]
    fn parse_mpi_option_values() {
        assert_eq!(parse_mpi_option("list").unwrap(), None);
        assert_eq!(parse_mpi_option("pmix").unwrap(), Some("pmix".into()));
        assert!(parse_mpi_option("pmi2").is_err());
    }

    #[test]
    fn resolve_step_mpi_inherits_job_when_step_unset() {
        assert_eq!(resolve_step_mpi("", "none"), "none");
        assert_eq!(resolve_step_mpi("", "pmix"), "pmix");
    }

    #[test]
    fn resolve_step_mpi_prefers_step_override() {
        assert_eq!(resolve_step_mpi("pmix", "none"), "pmix");
        assert_eq!(resolve_step_mpi("none", "pmix"), "none");
        assert_eq!(resolve_step_mpi("pmix", "pmix"), "pmix");
    }

    #[test]
    fn build_validated_pmix_plan_proto_rejects_invalid_multi_node() {
        let dispatch = PmixLocalDispatch {
            job_id: 1,
            universe_size: 5,
            task_offset: 0,
            local_count: 2,
            tmpdir: "/tmp/pmix".into(),
            job_uid: 0,
            job_gid: 0,
            num_nodes: 2,
            node_index: 0,
            peer_hosts: vec!["10.0.0.1".into(), "10.0.0.2".into()],
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        };
        let err = build_validated_pmix_plan_proto(MPI_PMIX, dispatch).unwrap_err();
        assert!(err.contains("uniform tasks per node"), "{err}");
    }

    #[test]
    fn build_validated_pmix_plan_proto_none_for_non_pmix() {
        let dispatch = PmixLocalDispatch {
            job_id: 1,
            universe_size: 4,
            task_offset: 0,
            local_count: 4,
            tmpdir: "/tmp/pmix".into(),
            job_uid: 0,
            job_gid: 0,
            num_nodes: 1,
            node_index: 0,
            peer_hosts: vec![],
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        };
        assert!(build_validated_pmix_plan_proto(MPI_NONE, dispatch)
            .unwrap()
            .is_none());
    }

    #[test]
    fn validate_multi_node_pmix_plan_rejects_non_uniform_tasks() {
        let plan = PmixLaunchPlan::local_tasks(
            1,
            5,
            0,
            2,
            "/tmp/pmix",
            0,
            0,
            2,
            0,
            vec!["10.0.0.1".into(), "10.0.0.2".into()],
        );
        let err = validate_pmix_plan(&plan).unwrap_err();
        assert!(err.contains("uniform tasks per node"), "{err}");
    }

    #[test]
    fn validate_multi_node_pmix_plan_rejects_local_count_mismatch() {
        let plan = PmixLaunchPlan::local_tasks(
            1,
            4,
            0,
            3,
            "/tmp/pmix",
            0,
            0,
            2,
            0,
            vec!["10.0.0.1".into(), "10.0.0.2".into()],
        );
        let err = validate_pmix_plan(&plan).unwrap_err();
        assert!(err.contains("local_count"), "{err}");
    }

    #[test]
    fn validate_multi_node_pmix_plan_accepts_peer_hosts() {
        let plan = PmixLaunchPlan::local_tasks(
            1,
            4,
            2,
            2,
            "/tmp/pmix",
            0,
            0,
            2,
            1,
            vec!["10.0.0.1".into(), "10.0.0.2".into()],
        );
        validate_pmix_plan(&plan).unwrap();
    }

    #[test]
    fn validate_multi_node_pmix_plan_rejects_peer_mismatch() {
        let plan = PmixLaunchPlan::local_tasks(
            1,
            4,
            0,
            2,
            "/tmp/pmix",
            0,
            0,
            2,
            0,
            vec!["10.0.0.1".into()],
        );
        assert!(validate_pmix_plan(&plan).is_err());
    }

    #[test]
    fn mpi_list_lines_include_supported_modes() {
        let lines = mpi_list_lines("/usr/lib/spur");
        assert!(lines.iter().any(|l| l == "none"));
        assert!(lines.iter().any(|l| l == "pmix"));
        assert!(lines.iter().any(|l| l.contains("plugin_dir=")));
    }

    #[test]
    fn version_at_least_compares_dotted_tokens() {
        assert!(version_at_least("4.2.8", "4.1.0"));
        assert!(version_at_least("4.1.0", "4.1.0"));
        assert!(!version_at_least("4.0.9", "4.1.0"));
        assert!(version_at_least("4.10.0", "4.9.0"));
    }

    #[test]
    fn validate_pmix_plan_rejects_empty_tmpdir() {
        let mut plan = PmixLaunchPlan::local_tasks(1, 1, 0, 1, "/tmp/pmix", 0, 0, 1, 0, vec![]);
        plan.tmpdir.clear();
        assert!(validate_pmix_plan(&plan).is_err());
    }

    #[test]
    fn validate_pmix_plan_rejects_inconsistent_ranks() {
        let plan = PmixLaunchPlan {
            job_id: 1,
            namespace: "spur.1".into(),
            universe_size: 2,
            task_offset: 0,
            local_procs: vec![
                PmixLocalProc {
                    rank: 0,
                    local_rank: 0,
                },
                PmixLocalProc {
                    rank: 2,
                    local_rank: 1,
                },
            ],
            tmpdir: "/tmp/pmix".into(),
            job_uid: 0,
            job_gid: 0,
            num_nodes: 1,
            node_index: 0,
            peer_hosts: vec![],
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        };
        assert!(validate_pmix_plan(&plan).is_err());
    }

    #[test]
    fn validate_pmix_plan_rejects_local_procs_beyond_universe_size() {
        let plan = PmixLaunchPlan {
            job_id: 1,
            namespace: "spur.1".into(),
            universe_size: 2,
            task_offset: 1,
            local_procs: vec![
                PmixLocalProc {
                    rank: 1,
                    local_rank: 0,
                },
                PmixLocalProc {
                    rank: 2,
                    local_rank: 1,
                },
            ],
            tmpdir: "/tmp/pmix".into(),
            job_uid: 0,
            job_gid: 0,
            num_nodes: 1,
            node_index: 0,
            peer_hosts: vec![],
            modex_connect_timeout_secs: 0,
            modex_fence_timeout_secs: 0,
            modex_verify_timeout_secs: 0,
        };
        assert!(validate_pmix_plan(&plan).is_err());
    }

    #[test]
    fn modex_port_for_job_is_stable() {
        assert_eq!(modex_port_for_job(42), 16861);
        assert_eq!(modex_port_for_job(8042), 16819 + (8042 % 8000));
    }

    #[test]
    fn pmix_step_peers_dense_modex_index_for_subset() {
        let peers =
            PmixStepPeers::from_participants(vec![0, 2], |idx| Some(format!("10.0.0.{}", idx + 1)))
                .unwrap();
        assert_eq!(peers.num_nodes, 2);
        assert_eq!(
            peers.peer_hosts,
            vec![String::from("10.0.0.1"), String::from("10.0.0.3")]
        );
        assert_eq!(peers.modex_node_index(0), Some(0));
        assert_eq!(peers.modex_node_index(2), Some(1));
        assert_eq!(peers.modex_node_index(1), None);

        let dispatch =
            pmix_local_dispatch_for_step(&peers, 2, 9, 2, 1, 1, "/tmp/pmix", 0, 0, 0, 0, 0)
                .unwrap();
        assert_eq!(dispatch.num_nodes, 2);
        assert_eq!(dispatch.node_index, 1);
        assert_eq!(dispatch.peer_hosts, peers.peer_hosts);
    }
}
