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
}

impl PmixLaunchPlan {
    pub fn namespace_for_job(job_id: u32) -> String {
        format!("spur.{job_id}")
    }

    /// Build a plan for all tasks running locally on one agent.
    pub fn local_tasks(
        job_id: u32,
        universe_size: u32,
        task_offset: u32,
        local_count: u32,
        tmpdir: impl Into<String>,
        job_uid: u32,
        job_gid: u32,
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
        }
    }
}

/// Supported `--mpi` values (excluding the special `list` keyword).
pub const MPI_NONE: &str = "none";
pub const MPI_PMIX: &str = "pmix";

/// Max bytes for PMIx namespace strings passed to the C plugin (NUL excluded).
pub const PMIX_NAMESPACE_MAX: usize = 255;
/// Max bytes for PMIx tmpdir strings passed to the C plugin (NUL excluded).
pub const PMIX_TMPDIR_MAX: usize = 511;

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
}

pub fn maybe_local_pmix_plan(mpi: &str, dispatch: PmixLocalDispatch) -> Option<PmixLaunchPlan> {
    if mpi != MPI_PMIX {
        return None;
    }
    Some(PmixLaunchPlan::local_tasks(
        dispatch.job_id,
        dispatch.universe_size,
        dispatch.task_offset,
        dispatch.local_count,
        dispatch.tmpdir,
        dispatch.job_uid,
        dispatch.job_gid,
    ))
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

/// Reject PMIx for allocations that span more than one node (Mode 1 scope).
pub fn validate_single_node_pmix(mpi: &str, num_nodes: u32) -> Result<(), String> {
    if mpi == MPI_PMIX && num_nodes > 1 {
        return Err(
            "--mpi=pmix is only supported for single-node jobs; multi-node PMIx is not yet available"
                .into(),
        );
    }
    Ok(())
}

/// Reject PMIx steps that fan out to more than one agent.
pub fn validate_pmix_step_agents(mpi: &str, agent_count: usize) -> Result<(), String> {
    if mpi == MPI_PMIX && agent_count > 1 {
        return Err(
            "--mpi=pmix steps must run on a single node; multi-node PMIx is not yet available"
                .into(),
        );
    }
    Ok(())
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
        let plan = PmixLaunchPlan::local_tasks(42, 4, 0, 4, "/tmp/pmix", 1000, 1000);
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
    fn validate_single_node_pmix_rejects_multi_node() {
        validate_single_node_pmix("none", 4).unwrap();
        validate_single_node_pmix("pmix", 1).unwrap();
        assert!(validate_single_node_pmix("pmix", 2).is_err());
    }

    #[test]
    fn validate_pmix_step_agents_rejects_multi_agent() {
        validate_pmix_step_agents("none", 2).unwrap();
        validate_pmix_step_agents("pmix", 1).unwrap();
        assert!(validate_pmix_step_agents("pmix", 2).is_err());
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
        let mut plan = PmixLaunchPlan::local_tasks(1, 1, 0, 1, "/tmp/pmix", 0, 0);
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
        };
        assert!(validate_pmix_plan(&plan).is_err());
    }
}
