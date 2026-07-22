// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for multi-task and multi-node step launch.
//!
//! Used by batch `launch_job` and srun step dispatch on agents.

use std::collections::HashMap;

use crate::spur_env::SpurEnv;
use crate::step::{distribute_tasks, CpuBind, GpuBind, TaskDistribution};

/// Per-node task launch parameters derived from a step's task distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStepTasks {
    pub node_index: u32,
    pub task_offset: u32,
    pub tasks_on_node: u32,
}

/// Compute how many tasks each node runs and the global rank offset of the
/// first task on that node.
pub fn build_step_task_plan(
    num_tasks: u32,
    num_nodes: u32,
    distribution: TaskDistribution,
) -> Vec<NodeStepTasks> {
    if num_tasks == 0 {
        return Vec::new();
    }

    let num_nodes = num_nodes.max(1);
    let mapping = distribute_tasks(num_tasks, num_nodes, distribution);
    let mut plan = Vec::new();

    for node_index in 0..num_nodes {
        let task_ids: Vec<u32> = mapping
            .iter()
            .enumerate()
            .filter(|(_, node)| **node == node_index)
            .map(|(task_id, _)| task_id as u32)
            .collect();
        if task_ids.is_empty() {
            continue;
        }
        plan.push(NodeStepTasks {
            node_index,
            task_offset: task_ids[0],
            tasks_on_node: task_ids.len() as u32,
        });
    }

    plan
}

/// Apply GPU bind directives from the request environment into step launch env.
///
/// Honors `map_gpu` / `mask_gpu` overrides. `closest` and `none` keep the
/// controller-assigned device list.
pub fn apply_gpu_bind_env(
    target: &mut HashMap<String, String>,
    source: &HashMap<String, String>,
    allocated: &[u32],
) {
    let Some(bind_str) = source
        .get("SPUR_GPU_BIND")
        .or_else(|| source.get("SLURM_GPU_BIND"))
    else {
        return;
    };
    let bind = bind_str.parse::<GpuBind>().unwrap_or(GpuBind::None);
    let visible = match bind {
        GpuBind::Map(ids) => ids,
        GpuBind::Mask(mask) => mask,
        GpuBind::Closest | GpuBind::None => allocated
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(","),
    };
    if visible.is_empty() {
        return;
    }
    target.insert("SPUR_JOB_GPUS".into(), visible.clone());
    target.insert("ROCR_VISIBLE_DEVICES".into(), visible.clone());
    target.insert("CUDA_VISIBLE_DEVICES".into(), visible.clone());
    target.insert("GPU_DEVICE_ORDINAL".into(), visible);
}

/// Return the CPU bind string when step mode cannot enforce topology-based binds.
pub fn unsupported_cpu_bind(source: &HashMap<String, String>) -> Option<String> {
    let bind_str = source
        .get("SPUR_CPU_BIND")
        .or_else(|| source.get("SLURM_CPU_BIND"))?;
    if bind_str.eq_ignore_ascii_case("none") || bind_str.is_empty() {
        return None;
    }
    let bind = bind_str.parse::<CpuBind>().unwrap_or(CpuBind::None);
    match bind {
        CpuBind::Cores | CpuBind::Threads | CpuBind::Sockets | CpuBind::Ldoms => {
            Some(bind_str.clone())
        }
        CpuBind::None | CpuBind::Rank | CpuBind::Map(_) | CpuBind::Mask(_) => None,
    }
}

fn parse_cpu_bind(source: &HashMap<String, String>) -> CpuBind {
    source
        .get("SPUR_CPU_BIND")
        .or_else(|| source.get("SLURM_CPU_BIND"))
        .map(|s| s.parse().unwrap_or(CpuBind::None))
        .unwrap_or(CpuBind::None)
}

fn parse_map_cpu_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_mask_cpu_list(mask: &str) -> Vec<String> {
    parse_map_cpu_list(mask)
}

fn mask_cpu_bind_bash_prefix(masks: &[&str]) -> String {
    if masks.len() > 1 {
        let entries = masks.join(" ");
        format!(
            "_CPU_MASK=({entries})\n  \
             _CPU_IDX=$((SPUR_TASK_OFFSET + LOCAL_RANK))\n  \
             if [ \"$_CPU_IDX\" -ge ${{#_CPU_MASK[@]}} ]; then\n    \
               echo \"mask_cpu: rank $_CPU_IDX exceeds CPU mask list (len ${{#_CPU_MASK[@]}})\" >&2\n    \
               exit 1\n  \
             fi\n  \
             taskset ${{_CPU_MASK[$_CPU_IDX]}} "
        )
    } else if let Some(mask) = masks.first() {
        format!("taskset {mask} ")
    } else {
        String::new()
    }
}

/// Returns an error message when `map_cpu` lists fewer CPUs than `num_tasks`.
pub fn map_cpu_bind_error(source: &HashMap<String, String>, num_tasks: u32) -> Option<String> {
    if num_tasks == 0 {
        return None;
    }
    let bind = parse_cpu_bind(source);
    let CpuBind::Map(list) = bind else {
        return None;
    };
    let cpus = parse_map_cpu_list(&list);
    let need = num_tasks as usize;
    if cpus.len() < need {
        Some(format!(
            "map_cpu lists {} CPU(s) but the step requires {} task(s)",
            cpus.len(),
            need
        ))
    } else {
        None
    }
}

/// Returns an error message when comma-separated `mask_cpu` lists fewer masks than `num_tasks`.
pub fn mask_cpu_bind_error(source: &HashMap<String, String>, num_tasks: u32) -> Option<String> {
    if num_tasks == 0 {
        return None;
    }
    let bind = parse_cpu_bind(source);
    let CpuBind::Mask(mask) = bind else {
        return None;
    };
    let masks = parse_mask_cpu_list(&mask);
    if masks.len() <= 1 {
        return None;
    }
    let need = num_tasks as usize;
    if masks.len() < need {
        Some(format!(
            "mask_cpu lists {} CPU mask(s) but the step requires {} task(s)",
            masks.len(),
            need
        ))
    } else {
        None
    }
}

/// Bash prefix that pins a task to CPUs per `map_cpu`, `mask_cpu`, or `rank`.
fn cpu_bind_bash_prefix(bind: &CpuBind, map_cpus: &[&str]) -> String {
    match bind {
        CpuBind::Rank => "taskset -c $((SPUR_TASK_OFFSET + LOCAL_RANK)) ".to_string(),
        CpuBind::Map(_) if !map_cpus.is_empty() => {
            let entries = map_cpus.join(" ");
            format!(
                "_CPU_MAP=({entries})\n  \
                 _CPU_IDX=$((SPUR_TASK_OFFSET + LOCAL_RANK))\n  \
                 if [ \"$_CPU_IDX\" -ge ${{#_CPU_MAP[@]}} ]; then\n    \
                   echo \"map_cpu: rank $_CPU_IDX exceeds CPU map (len ${{#_CPU_MAP[@]}})\" >&2\n    \
                   exit 1\n  \
                 fi\n  \
                 taskset -c ${{_CPU_MAP[$_CPU_IDX]}} "
            )
        }
        CpuBind::Map(_) => String::new(),
        CpuBind::Mask(mask) => {
            let masks: Vec<&str> = mask
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            mask_cpu_bind_bash_prefix(&masks)
        }
        CpuBind::None | CpuBind::Cores | CpuBind::Threads | CpuBind::Sockets | CpuBind::Ldoms => {
            String::new()
        }
    }
}

/// Prefix argv with `taskset` when an explicit CPU bind applies to one task.
pub fn wrap_command_with_cpu_bind(
    program: &str,
    args: &[String],
    source: &HashMap<String, String>,
    global_rank: u32,
) -> (String, Vec<String>) {
    let bind = parse_cpu_bind(source);
    match bind {
        CpuBind::Rank => (
            "taskset".into(),
            std::iter::once("-c".to_string())
                .chain(std::iter::once(global_rank.to_string()))
                .chain(std::iter::once(program.to_string()))
                .chain(args.iter().cloned())
                .collect(),
        ),
        CpuBind::Map(list) => {
            let cpus = parse_map_cpu_list(&list);
            let Some(cpu) = cpus.get(global_rank as usize) else {
                return (program.to_string(), args.to_vec());
            };
            (
                "taskset".into(),
                std::iter::once("-c".to_string())
                    .chain(std::iter::once(cpu.clone()))
                    .chain(std::iter::once(program.to_string()))
                    .chain(args.iter().cloned())
                    .collect(),
            )
        }
        CpuBind::Mask(mask) => {
            let masks = parse_mask_cpu_list(&mask);
            let mask_arg = if masks.len() > 1 {
                let Some(m) = masks.get(global_rank as usize) else {
                    return (program.to_string(), args.to_vec());
                };
                m.clone()
            } else {
                masks.first().cloned().unwrap_or_else(|| mask.clone())
            };
            (
                "taskset".into(),
                std::iter::once(mask_arg)
                    .chain(std::iter::once(program.to_string()))
                    .chain(args.iter().cloned())
                    .collect(),
            )
        }
        CpuBind::None | CpuBind::Cores | CpuBind::Threads | CpuBind::Sockets | CpuBind::Ldoms => {
            (program.to_string(), args.to_vec())
        }
    }
}

/// Build a bash wrapper that forks `tasks_on_node` copies of `user_script_path`,
/// assigning distinct `LOCAL_RANK` / `SLURM_PROCID` values in each fork.
///
/// Output labeling is controlled at runtime via the `SPUR_LABEL=1` environment
/// variable (matching batch `launch_job` and srun `-l`).
pub fn build_multi_task_wrapper(
    user_script_path: &str,
    tasks_on_node: u32,
    environment: Option<&HashMap<String, String>>,
) -> String {
    let escaped = user_script_path.replace('"', "\\\"");
    let bind = environment.map(parse_cpu_bind).unwrap_or(CpuBind::None);
    let map_cpus: Vec<&str> = match &bind {
        CpuBind::Map(list) => list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        CpuBind::Mask(mask) => mask
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let taskset_prefix = cpu_bind_bash_prefix(&bind, &map_cpus);
    let mut wrapper = String::from("#!/bin/bash\n");
    wrapper.push_str(&format!(
        "_TASKS_ON_NODE={tasks_on_node}\nSPUR_TASK_OFFSET=${{SPUR_TASK_OFFSET:-0}}\n"
    ));
    wrapper.push_str("for LOCAL_RANK in $(seq 0 $((_TASKS_ON_NODE - 1))); do\n");
    wrapper.push_str("  export LOCAL_RANK\n");
    wrapper.push_str(SpurEnv::per_task_bash_exports());
    wrapper.push_str("  export PMI_RANK=$SPUR_PROCID\n");
    wrapper.push_str("  export PMIX_RANK=$SPUR_PROCID\n");
    wrapper.push_str("  export OMPI_COMM_WORLD_RANK=$SPUR_PROCID\n");
    wrapper.push_str("  export OMPI_COMM_WORLD_LOCAL_RANK=$LOCAL_RANK\n");

    wrapper.push_str("  if [ -n \"$SPUR_JOB_GPUS\" ]; then\n");
    wrapper.push_str("    IFS=',' read -ra _ALL_GPUS <<< \"$SPUR_JOB_GPUS\"\n");
    wrapper.push_str("    _GPUS_PER_TASK=$(( ${#_ALL_GPUS[@]} / _TASKS_ON_NODE ))\n");
    wrapper.push_str("    if [ $_GPUS_PER_TASK -gt 0 ]; then\n");
    wrapper.push_str("      _START=$((LOCAL_RANK * _GPUS_PER_TASK))\n");
    wrapper.push_str(
        "      _TASK_GPUS=$(echo \"${_ALL_GPUS[@]:$_START:$_GPUS_PER_TASK}\" | tr ' ' ',')\n",
    );
    wrapper.push_str("      export ROCR_VISIBLE_DEVICES=$_TASK_GPUS\n");
    wrapper.push_str("      export CUDA_VISIBLE_DEVICES=$_TASK_GPUS\n");
    wrapper.push_str("      export GPU_DEVICE_ORDINAL=$_TASK_GPUS\n");
    wrapper.push_str("    fi\n");
    wrapper.push_str("  fi\n");

    wrapper.push_str("  if [ \"$SPUR_LABEL\" = \"1\" ]; then\n");
    wrapper.push_str(&format!(
        "    {taskset_prefix}bash \"{escaped}\" 2>&1 | sed \"s/^/[$SPUR_PROCID] /\" &\n"
    ));
    wrapper.push_str("  else\n");
    wrapper.push_str(&format!("    {taskset_prefix}bash \"{escaped}\" &\n"));
    wrapper.push_str("  fi\n");
    wrapper.push_str("done\nwait\n");
    wrapper
}

/// Bash wrapper for a single labeled task (one task per node in fan-out steps).
pub fn build_labeled_single_task_wrapper(
    user_script_path: &str,
    procid: u32,
    environment: Option<&HashMap<String, String>>,
) -> String {
    let escaped = user_script_path.replace('"', "\\\"");
    let bind = environment.map(parse_cpu_bind).unwrap_or(CpuBind::None);
    let map_cpus: Vec<&str> = match &bind {
        CpuBind::Map(list) => list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        CpuBind::Mask(mask) => mask
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let taskset_prefix = cpu_bind_bash_prefix(&bind, &map_cpus);
    format!("#!/bin/bash\n{taskset_prefix}bash \"{escaped}\" 2>&1 | sed \"s/^/[{procid}] /\"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_plan_one_task_per_node() {
        let plan = build_step_task_plan(2, 2, TaskDistribution::Block);
        assert_eq!(
            plan,
            vec![
                NodeStepTasks {
                    node_index: 0,
                    task_offset: 0,
                    tasks_on_node: 1,
                },
                NodeStepTasks {
                    node_index: 1,
                    task_offset: 1,
                    tasks_on_node: 1,
                },
            ]
        );
    }

    #[test]
    fn step_plan_two_tasks_per_node() {
        let plan = build_step_task_plan(4, 2, TaskDistribution::Block);
        assert_eq!(
            plan,
            vec![
                NodeStepTasks {
                    node_index: 0,
                    task_offset: 0,
                    tasks_on_node: 2,
                },
                NodeStepTasks {
                    node_index: 1,
                    task_offset: 2,
                    tasks_on_node: 2,
                },
            ]
        );
    }

    #[test]
    fn step_plan_single_node_multi_task() {
        let plan = build_step_task_plan(4, 1, TaskDistribution::Block);
        assert_eq!(
            plan,
            vec![NodeStepTasks {
                node_index: 0,
                task_offset: 0,
                tasks_on_node: 4,
            }]
        );
    }

    #[test]
    fn multi_task_wrapper_exports_procid() {
        let script = build_multi_task_wrapper("/tmp/work.sh", 2, None);
        assert!(script.contains("SPUR_PROCID"));
        assert!(script.contains("SLURM_PROCID"));
        assert!(script.contains("_TASKS_ON_NODE=2"));
        assert!(script.contains("/tmp/work.sh"));
    }

    #[test]
    fn multi_task_wrapper_applies_map_cpu_bind() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "map_cpu:0,4".into());
        let script = build_multi_task_wrapper("/tmp/work.sh", 2, Some(&env));
        assert!(script.contains("_CPU_MAP=(0 4)"));
        assert!(script.contains("_CPU_IDX\" -ge"));
        assert!(script.contains("taskset -c ${_CPU_MAP[$_CPU_IDX]}"));
    }

    #[test]
    fn multi_task_wrapper_honors_spur_label_env() {
        let script = build_multi_task_wrapper("/tmp/work.sh", 1, None);
        assert!(script.contains("SPUR_LABEL"));
        assert!(script.contains("sed \"s/^/[$SPUR_PROCID] /\""));
    }

    #[test]
    fn apply_gpu_bind_map_override() {
        let mut env = HashMap::new();
        env.insert("SPUR_GPU_BIND".into(), "map_gpu:2,3".into());
        let mut target = HashMap::new();
        apply_gpu_bind_env(&mut target, &env, &[0, 1]);
        assert_eq!(target.get("ROCR_VISIBLE_DEVICES").unwrap(), "2,3");
        assert_eq!(target.get("SPUR_JOB_GPUS").unwrap(), "2,3");
    }

    #[test]
    fn wrap_command_with_cpu_bind_rank() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "rank".into());
        let (program, args) = wrap_command_with_cpu_bind("hostname", &[], &env, 3);
        assert_eq!(program, "taskset");
        assert_eq!(args, vec!["-c", "3", "hostname"]);
    }

    #[test]
    fn unsupported_cpu_bind_flags_topology_modes() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "cores".into());
        assert_eq!(unsupported_cpu_bind(&env).as_deref(), Some("cores"));
        env.insert("SPUR_CPU_BIND".into(), "map_cpu:0,1".into());
        assert_eq!(unsupported_cpu_bind(&env), None);
    }

    #[test]
    fn map_cpu_bind_error_when_map_shorter_than_tasks() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "map_cpu:0,1".into());
        assert_eq!(
            map_cpu_bind_error(&env, 3).as_deref(),
            Some("map_cpu lists 2 CPU(s) but the step requires 3 task(s)")
        );
        assert_eq!(map_cpu_bind_error(&env, 2), None);
    }

    #[test]
    fn mask_cpu_bind_error_when_mask_list_shorter_than_tasks() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "mask_cpu:0x3,0xc".into());
        assert_eq!(
            mask_cpu_bind_error(&env, 3).as_deref(),
            Some("mask_cpu lists 2 CPU mask(s) but the step requires 3 task(s)")
        );
        assert_eq!(mask_cpu_bind_error(&env, 2), None);
    }

    #[test]
    fn mask_cpu_bind_error_allows_single_mask_for_all_tasks() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "mask_cpu:0x3".into());
        assert_eq!(mask_cpu_bind_error(&env, 4), None);
    }

    #[test]
    fn wrap_command_with_cpu_bind_map_uses_list_entry() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "map_cpu:2,4".into());
        let (program, args) = wrap_command_with_cpu_bind("hostname", &[], &env, 1);
        assert_eq!(program, "taskset");
        assert_eq!(args, vec!["-c", "4", "hostname"]);
    }

    #[test]
    fn wrap_command_with_cpu_bind_mask_uses_hex_mask() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "mask_cpu:0x3".into());
        let (program, args) = wrap_command_with_cpu_bind("hostname", &[], &env, 0);
        assert_eq!(program, "taskset");
        assert_eq!(args, vec!["0x3", "hostname"]);
    }

    #[test]
    fn wrap_command_with_cpu_bind_mask_uses_per_rank_entry() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "mask_cpu:0x3,0xc".into());
        let (_, args) = wrap_command_with_cpu_bind("hostname", &[], &env, 1);
        assert_eq!(args, vec!["0xc", "hostname"]);
    }

    #[test]
    fn labeled_single_task_wrapper_applies_sed_prefix() {
        let script = build_labeled_single_task_wrapper("/tmp/step.sh", 4, None);
        assert!(script.contains("sed \"s/^/[4] /\""));
    }

    #[test]
    fn wrap_command_with_cpu_bind_map_skips_taskset_when_rank_oob() {
        let mut env = HashMap::new();
        env.insert("SPUR_CPU_BIND".into(), "map_cpu:0".into());
        let (program, args) = wrap_command_with_cpu_bind("hostname", &[], &env, 2);
        assert_eq!(program, "hostname");
        assert!(args.is_empty());
    }
}
