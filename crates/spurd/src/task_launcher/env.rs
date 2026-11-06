// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// A task's local and global rank, in both `SPUR_` and `SLURM_` form.
pub fn rank_ids(task_offset: u32, local_id: u32) -> Vec<(String, String)> {
    let local = local_id.to_string();
    let procid = (task_offset + local_id).to_string();
    vec![
        ("SPUR_LOCALID".to_string(), local.clone()),
        ("SLURM_LOCALID".to_string(), local),
        ("SPUR_PROCID".to_string(), procid.clone()),
        ("SLURM_PROCID".to_string(), procid),
    ]
}

/// The GPU selectors narrowed to one task's contiguous slice of the node's
/// GPUs, or nothing when the task keeps the step's whole device set.
///
/// The slice keeps the bash wrapper's rules: integer division, leftover GPUs
/// unassigned, and no narrowing when there are fewer GPUs than tasks.
pub fn gpu_slice_env(
    local_id: u32,
    tasks_on_node: u32,
    job_gpus: Option<&str>,
) -> Vec<(String, String)> {
    let Some(slice) = gpu_slice(local_id, tasks_on_node, job_gpus) else {
        return Vec::new();
    };
    let devices = slice.join(",");
    let mut env: Vec<(String, String)> = [
        "ROCR_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        "GPU_DEVICE_ORDINAL",
    ]
    .into_iter()
    .map(|name| (name.to_string(), devices.clone()))
    .collect();
    // ROCR has already renumbered the visible set by the time HIP applies its
    // own selector, so HIP counts from zero.
    let hip = (0..slice.len())
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    env.push(("HIP_VISIBLE_DEVICES".to_string(), hip));
    env
}

fn gpu_slice(local_id: u32, tasks_on_node: u32, job_gpus: Option<&str>) -> Option<Vec<&str>> {
    if tasks_on_node <= 1 {
        return None;
    }
    let gpus: Vec<&str> = job_gpus?.split(',').filter(|g| !g.is_empty()).collect();
    let per_task = gpus.len() / tasks_on_node as usize;
    if per_task == 0 {
        return None;
    }
    let start = local_id as usize * per_task;
    gpus.get(start..start + per_task).map(<[&str]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(
        task_offset: u32,
        local_id: u32,
        tasks: u32,
        gpus: Option<&str>,
    ) -> HashMap<String, String> {
        rank_ids(task_offset, local_id)
            .into_iter()
            .chain(gpu_slice_env(local_id, tasks, gpus))
            .collect()
    }

    #[test]
    fn ids_are_offset_by_the_nodes_first_rank() {
        let env = env_of(4, 1, 2, None);
        assert_eq!(env["SPUR_LOCALID"], "1");
        assert_eq!(env["SLURM_LOCALID"], "1");
        assert_eq!(env["SPUR_PROCID"], "5");
        assert_eq!(env["SLURM_PROCID"], "5");
    }

    #[test]
    fn each_task_gets_its_own_gpu_slice() {
        let env = env_of(0, 1, 2, Some("0,1,2,3"));
        assert_eq!(env["ROCR_VISIBLE_DEVICES"], "2,3");
        assert_eq!(env["CUDA_VISIBLE_DEVICES"], "2,3");
        assert_eq!(env["GPU_DEVICE_ORDINAL"], "2,3");
        assert_eq!(env["HIP_VISIBLE_DEVICES"], "0,1");
    }

    #[test]
    fn leftover_gpus_stay_unassigned() {
        let env = env_of(0, 1, 2, Some("4,5,6"));
        assert_eq!(env["ROCR_VISIBLE_DEVICES"], "5");
        assert_eq!(env["HIP_VISIBLE_DEVICES"], "0");
    }

    #[test]
    fn fewer_gpus_than_tasks_narrows_nothing() {
        let env = env_of(0, 0, 2, Some("0"));
        assert!(!env.contains_key("ROCR_VISIBLE_DEVICES"));
        assert!(!env.contains_key("HIP_VISIBLE_DEVICES"));
    }

    #[test]
    fn a_lone_task_keeps_the_whole_device_set() {
        let env = env_of(0, 0, 1, Some("0,1"));
        assert!(!env.contains_key("ROCR_VISIBLE_DEVICES"));
    }

    #[test]
    fn no_gpus_sets_no_gpu_variables() {
        let env = env_of(0, 0, 2, Some(""));
        assert!(!env.contains_key("ROCR_VISIBLE_DEVICES"));
        let env = env_of(0, 0, 2, None);
        assert!(!env.contains_key("CUDA_VISIBLE_DEVICES"));
    }
}
