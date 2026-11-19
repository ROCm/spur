// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Placement-independent checks on a typed step's resource request: the rules
//! that hold regardless of which node the step lands on, shared by `srun`
//! (before the step is even created) and `spurd` (`step_resources::resolve`).

use spur_proto::proto::StepResources;

/// Checks `r` for shape errors a node's resources cannot change. `step_nodes`
/// is the step's node count when known (so `--gpus` divisibility can be
/// checked); `None` defers that one check to the agent.
pub fn check(r: &StepResources, step_nodes: Option<u32>) -> Result<(), String> {
    if r.cpus_per_task.is_some() && r.cpus_per_gpu.is_some() {
        return Err("--cpus-per-task and --cpus-per-gpu cannot both be set".into());
    }
    if r.cpus_per_task == Some(0) {
        return Err("--cpus-per-task must be at least 1".into());
    }
    if r.cpus_per_gpu == Some(0) {
        return Err("--cpus-per-gpu must be at least 1".into());
    }

    let memory_forms = [
        r.memory_mb.is_some(),
        r.memory_per_cpu_mb.is_some(),
        r.memory_per_gpu_mb.is_some(),
    ];
    if memory_forms.iter().filter(|set| **set).count() > 1 {
        return Err("only one of --mem, --mem-per-cpu, --mem-per-gpu may be set".into());
    }
    // Whichever form is used, a requested zero would resolve to a 0-byte
    // limit rather than "no limit" (there is no such form here).
    if r.memory_mb == Some(0) {
        return Err("--mem must be greater than zero".into());
    }
    if r.memory_per_cpu_mb == Some(0) {
        return Err("--mem-per-cpu must be greater than zero".into());
    }
    if r.memory_per_gpu_mb == Some(0) {
        return Err("--mem-per-gpu must be greater than zero".into());
    }

    let gpu_forms = [
        r.gpus.is_some(),
        r.gpus_per_node.is_some(),
        r.gpus_per_task.is_some(),
    ];
    if gpu_forms.iter().filter(|set| **set).count() > 1 {
        return Err("only one of --gpus, --gpus-per-node, --gpus-per-task may be set".into());
    }

    let has_gpu_form = r.gpus.is_some_and(|n| n > 0)
        || r.gpus_per_node.is_some_and(|n| n > 0)
        || r.gpus_per_task.is_some_and(|n| n > 0);
    if r.cpus_per_gpu.is_some() && !has_gpu_form {
        return Err("--cpus-per-gpu needs the step to ask for GPUs on this node".into());
    }
    if r.memory_per_gpu_mb.is_some() && !has_gpu_form {
        return Err("--mem-per-gpu needs the step to ask for GPUs on this node".into());
    }

    if let (Some(total), Some(nodes)) = (r.gpus, step_nodes) {
        let nodes = nodes.max(1);
        if total % nodes != 0 {
            return Err(format!(
                "--gpus={total} does not split evenly across the step's {nodes} nodes; \
                 use --gpus-per-node"
            ));
        }
    }

    Ok(())
}

/// Whether `r` asks for its own share instead of sharing the job's. A `gpu_type`
/// alone is a filter on a count, not a request by itself: it narrows which of
/// the job's GPUs a count or form picks from, so it takes nothing on its own.
pub fn takes_own_share(r: &StepResources) -> bool {
    r.cpus_per_task.is_some()
        || r.memory_mb.is_some()
        || r.gpus.is_some()
        || r.gpus_per_node.is_some()
        || r.gpus_per_task.is_some()
        || r.cpus_per_gpu.is_some()
        || r.memory_per_gpu_mb.is_some()
        || r.memory_per_cpu_mb.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(f: impl FnOnce(&mut StepResources)) -> StepResources {
        let mut r = StepResources::default();
        f(&mut r);
        r
    }

    #[test]
    fn an_empty_request_is_fine() {
        assert!(check(&StepResources::default(), None).is_ok());
    }

    #[test]
    fn both_cpu_forms_are_rejected() {
        let r = typed(|r| {
            r.cpus_per_task = Some(1);
            r.cpus_per_gpu = Some(1);
        });
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn zero_cpus_per_task_is_rejected() {
        let r = typed(|r| r.cpus_per_task = Some(0));
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn zero_cpus_per_gpu_is_rejected() {
        let r = typed(|r| r.cpus_per_gpu = Some(0));
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn two_memory_forms_are_rejected() {
        let r = typed(|r| {
            r.memory_mb = Some(1);
            r.memory_per_cpu_mb = Some(1);
        });
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn zero_mem_is_rejected() {
        let r = typed(|r| r.memory_mb = Some(0));
        assert_eq!(
            check(&r, None),
            Err("--mem must be greater than zero".into())
        );
    }

    #[test]
    fn zero_mem_per_cpu_or_per_gpu_is_rejected_too() {
        let per_cpu = typed(|r| r.memory_per_cpu_mb = Some(0));
        assert_eq!(
            check(&per_cpu, None),
            Err("--mem-per-cpu must be greater than zero".into())
        );
        let per_gpu = typed(|r| {
            r.memory_per_gpu_mb = Some(0);
            r.gpus_per_node = Some(1);
        });
        assert_eq!(
            check(&per_gpu, None),
            Err("--mem-per-gpu must be greater than zero".into())
        );
    }

    #[test]
    fn two_gpu_forms_are_rejected() {
        let r = typed(|r| {
            r.gpus = Some(1);
            r.gpus_per_node = Some(1);
        });
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn cpus_per_gpu_without_a_gpu_form_is_rejected() {
        let r = typed(|r| r.cpus_per_gpu = Some(4));
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn mem_per_gpu_without_a_gpu_form_is_rejected() {
        let r = typed(|r| r.memory_per_gpu_mb = Some(1024));
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn cpus_per_gpu_with_a_zero_gpu_form_is_still_rejected() {
        let r = typed(|r| {
            r.cpus_per_gpu = Some(4);
            r.gpus_per_node = Some(0);
        });
        assert!(check(&r, None).is_err());
    }

    #[test]
    fn cpus_per_gpu_with_a_real_gpu_form_is_fine() {
        let r = typed(|r| {
            r.cpus_per_gpu = Some(4);
            r.gpus_per_node = Some(1);
        });
        assert!(check(&r, None).is_ok());
    }

    #[test]
    fn uneven_gpus_across_explicit_nodes_is_rejected() {
        let r = typed(|r| r.gpus = Some(3));
        assert!(check(&r, Some(2)).is_err());
        assert!(check(&r, Some(3)).is_ok());
        assert!(
            check(&r, None).is_ok(),
            "deferred to the agent without a node count"
        );
    }

    #[test]
    fn zero_gpus_is_a_valid_request_for_none() {
        let r = typed(|r| r.gpus = Some(0));
        assert!(check(&r, Some(2)).is_ok());
    }

    #[test]
    fn a_gpu_type_alone_does_not_take_its_own_share() {
        let r = typed(|r| r.gpu_type = "mi355x".into());
        assert!(!takes_own_share(&r));
    }

    #[test]
    fn a_count_with_a_type_takes_its_own_share() {
        let r = typed(|r| {
            r.gpus_per_node = Some(1);
            r.gpu_type = "mi355x".into();
        });
        assert!(takes_own_share(&r));
    }

    #[test]
    fn each_resource_field_alone_takes_its_own_share() {
        assert!(takes_own_share(&typed(|r| r.cpus_per_task = Some(1))));
        assert!(takes_own_share(&typed(|r| r.memory_mb = Some(1))));
        assert!(takes_own_share(&typed(|r| r.gpus = Some(1))));
        assert!(takes_own_share(&typed(|r| r.gpus_per_node = Some(1))));
        assert!(takes_own_share(&typed(|r| r.gpus_per_task = Some(1))));
        assert!(takes_own_share(&typed(|r| r.cpus_per_gpu = Some(1))));
        assert!(takes_own_share(&typed(|r| r.memory_per_gpu_mb = Some(1))));
        assert!(takes_own_share(&typed(|r| r.memory_per_cpu_mb = Some(1))));
    }
}
