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
        }
    }
}

/// Supported `--mpi` values (excluding the special `list` keyword).
pub const MPI_NONE: &str = "none";
pub const MPI_PMIX: &str = "pmix";

pub fn maybe_local_pmix_plan(
    mpi: &str,
    job_id: u32,
    universe_size: u32,
    task_offset: u32,
    local_count: u32,
    tmpdir: impl Into<String>,
) -> Option<PmixLaunchPlan> {
    if mpi != MPI_PMIX {
        return None;
    }
    Some(PmixLaunchPlan::local_tasks(
        job_id,
        universe_size,
        task_offset,
        local_count,
        tmpdir,
    ))
}

/// Parse `--mpi` / `#SBATCH --mpi`. Returns `None` for `list`.
pub fn parse_mpi_option(value: &str) -> Result<Option<String>, String> {
    if value == "list" {
        return Ok(None);
    }
    match value {
        MPI_NONE | MPI_PMIX => Ok(Some(value.to_string())),
        other => Err(format!("invalid --mpi value '{other}' (supported: none, pmix)")),
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
        let plan = PmixLaunchPlan::local_tasks(42, 4, 0, 4, "/tmp/pmix");
        assert_eq!(plan.namespace, "spur.42");
        assert_eq!(plan.universe_size, 4);
        assert_eq!(plan.local_procs.len(), 4);
        assert_eq!(plan.local_procs[0].rank, 0);
        assert_eq!(plan.local_procs[3].rank, 3);
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
}
