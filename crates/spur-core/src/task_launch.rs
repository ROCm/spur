// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for multi-task and multi-node step launch.
//!
//! Used by batch `launch_job` and srun step dispatch on agents.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::mpi::MPI_PMIX;
use crate::spur_env::SpurEnv;
use crate::step::{distribute_tasks, CpuBind, GpuBind, TaskDistribution};

static STEP_LAUNCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s;&|])(?:/.*/)?(srun|mpirun|mpiexec)\b").expect("valid step-launch regex")
});

/// Placeholder for a quoted span when sanitizing a line for step-launcher detection.
/// Must not be whitespace or a shell command separator so word adjacency is preserved.
const QUOTED_SPAN_PLACEHOLDER: char = 'x';

fn hash_starts_shell_comment(line: &str, hash_index: usize) -> bool {
    if hash_index == 0 {
        return true;
    }
    let prev = line.as_bytes()[hash_index - 1] as char;
    matches!(prev, ' ' | '\t' | ';' | '&' | '|' | '(' | ')')
}

/// Build a sanitized copy of a shell line for step-launcher regex matching: quoted
/// spans collapse to a single placeholder (preserving adjacency with neighbors),
/// and `#` starts a comment only at a word boundary.
fn sanitize_shell_line_for_step_launch(line: &str) -> String {
    let mut sanitized = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_single {
            if c == '\'' {
                in_single = false;
                sanitized.push(QUOTED_SPAN_PLACEHOLDER);
            }
            i += 1;
            continue;
        }
        if in_double {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
                sanitized.push(QUOTED_SPAN_PLACEHOLDER);
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            in_single = true;
            i += 1;
            continue;
        }
        if c == '"' {
            in_double = true;
            i += 1;
            continue;
        }
        if c == '#' && hash_starts_shell_comment(line, i) {
            break;
        }
        sanitized.push(c);
        i += 1;
    }

    sanitized
}

/// True when a batch script delegates rank fan-out to an inner step launcher.
pub fn batch_script_uses_step_launch(script: &str) -> bool {
    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let sanitized = sanitize_shell_line_for_step_launch(trimmed);
        if STEP_LAUNCH_RE.is_match(sanitized.trim()) {
            return true;
        }
    }
    false
}

/// Whether batch `launch_job` should fan out the user script into per-rank
/// wrappers on this node.
///
/// Batch scripts normally run once per node; multi-rank MPI is the script's
/// job via an internal `srun`. Spur additionally fans out when `--mpi=pmix`
/// is set so direct batch launches (no inner `srun`) spawn one rank per task on
/// the node. Standalone `srun` routed through the batch path uses `task_fanout`
/// for the same effect with non-PMIx commands.
pub fn use_multi_task_launch(
    tasks_per_node: u32,
    task_fanout: bool,
    mpi: &str,
    script: &str,
) -> bool {
    if tasks_per_node <= 1 {
        return false;
    }
    if task_fanout {
        return true;
    }
    mpi == MPI_PMIX && !batch_script_uses_step_launch(script)
}

/// True when batch dispatch already ran multi-node PMIx prepare for this job.
///
/// Direct `#SBATCH --mpi=pmix` on multiple nodes prepares PMIx before launch.
/// Batch scripts with an inner `srun` skip batch prepare; their steps prepare
/// independently. Must stay aligned with `confirm_dispatch_on_nodes`.
pub fn batch_dispatched_multi_node_pmix(
    job_mpi: Option<&str>,
    num_allocated_nodes: u32,
    script: Option<&str>,
) -> bool {
    job_mpi == Some(MPI_PMIX)
        && num_allocated_nodes > 1
        && !script.is_some_and(batch_script_uses_step_launch)
}

/// Whether a multi-node PMIx step needs controller-side `PreparePmix` before
/// fan-out to agents.
pub fn step_needs_pmix_prepare(
    step_num_nodes: u32,
    job_mpi: Option<&str>,
    num_allocated_nodes: u32,
    script: Option<&str>,
) -> bool {
    step_num_nodes > 1 && !batch_dispatched_multi_node_pmix(job_mpi, num_allocated_nodes, script)
}

/// Bash body for non-primary batch nodes when the user script uses `srun`.
///
/// The batch script runs only on the first allocated node; companions hold
/// their slice of the allocation until the controller tears them down.
pub fn batch_companion_hold_script() -> &'static str {
    "#!/bin/bash\nwhile true; do sleep 60; done\n"
}

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
    // Zero allocated GPUs is authoritative: deny regardless of bind mode, so a
    // map_gpu/mask_gpu cannot name a device the job was not granted.
    if allocated.is_empty() {
        gpu_deny_visibility(target);
        return;
    }
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
        gpu_deny_visibility(target);
        return;
    }
    target.insert("SPUR_JOB_GPUS".into(), visible.clone());
    target.insert("ROCR_VISIBLE_DEVICES".into(), visible.clone());
    target.insert("CUDA_VISIBLE_DEVICES".into(), visible.clone());
    target.insert("GPU_DEVICE_ORDINAL".into(), visible);
}

/// Hide all GPUs from the common vendor runtimes for a zero-GPU job.
///
/// Unset/empty isn't reliably "no devices", so each selector gets a deny token:
/// `-1` for the ROCm/CUDA/Level-Zero index selectors, `void` for
/// nvidia-container-runtime, empty for Spur's own `SPUR_JOB_GPUS`. Advisory.
pub fn gpu_deny_visibility(target: &mut HashMap<String, String>) {
    for var in [
        "ROCR_VISIBLE_DEVICES",
        "HIP_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        "GPU_DEVICE_ORDINAL",
        "ZE_AFFINITY_MASK",
    ] {
        target.insert(var.to_string(), "-1".to_string());
    }
    target.insert("NVIDIA_VISIBLE_DEVICES".to_string(), "void".to_string());
    target.insert("SPUR_JOB_GPUS".to_string(), String::new());
}

/// True when env requests CPU bind that the single-`mpirun` PMIx wrapper does not apply.
pub fn mpi_mpirun_skips_cpu_bind(source: &HashMap<String, String>) -> bool {
    matches!(
        source
            .get("SPUR_CPU_BIND")
            .or_else(|| source.get("SLURM_CPU_BIND")),
        Some(bind) if !bind.is_empty() && !bind.eq_ignore_ascii_case("none")
    )
}

/// True when env requests GPU bind that the single-`mpirun` PMIx wrapper does not apply.
pub fn mpi_mpirun_skips_gpu_bind(source: &HashMap<String, String>) -> bool {
    let Some(bind_str) = source
        .get("SPUR_GPU_BIND")
        .or_else(|| source.get("SLURM_GPU_BIND"))
    else {
        return false;
    };
    if bind_str.is_empty() || bind_str.eq_ignore_ascii_case("none") {
        return false;
    }
    !matches!(bind_str.parse::<GpuBind>(), Ok(GpuBind::None))
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

/// Wrap `value` in single quotes for safe embedding in generated bash scripts.
fn bash_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Bash prefix shared by PMIx/Open MPI launch wrappers.
fn mpi_launch_preamble() -> &'static str {
    concat!(
        "if [ -f \"${HOME}/spur/mpi/env.sh\" ]; then . \"${HOME}/spur/mpi/env.sh\"; fi\n",
        "if [ -z \"${SPUR_MPIRUN:-}\" ]; then\n",
        "  if command -v mpirun >/dev/null 2>&1; then SPUR_MPIRUN=$(command -v mpirun)\n",
        "  elif [ -n \"${OPAL_PREFIX:-}\" ] && [ -x \"${OPAL_PREFIX}/bin/mpirun\" ]; then SPUR_MPIRUN=\"${OPAL_PREFIX}/bin/mpirun\"\n",
        "  elif [ -n \"${OPAL_PREFIX:-}\" ] && [ -x \"${OPAL_PREFIX}/bin/mpirun.openmpi\" ]; then SPUR_MPIRUN=\"${OPAL_PREFIX}/bin/mpirun.openmpi\"\n",
        "  else SPUR_MPIRUN=mpirun; fi\n",
        "fi\n",
        "export PMIX_SERVER_URI4=${PMIX_SERVER_URI4:-$PMIX_SERVER_URI}\n",
        "export PMIX_SERVER_URI3=${PMIX_SERVER_URI3:-$PMIX_SERVER_URI}\n",
    )
}

/// Legacy mpirun wrapper kept for unit tests; Spur PMIx jobs use direct per-rank
/// launch via [`build_multi_task_pmix_wrapper`].
pub fn build_mpi_mpirun_wrapper(user_script_path: &str, tasks_on_node: u32) -> String {
    let quoted = bash_single_quote(user_script_path);
    format!(
        concat!(
            "#!/bin/bash\n",
            "_TASKS_ON_NODE={tasks_on_node}\n",
            "{preamble}",
            "if [ \"$SPUR_LABEL\" = \"1\" ]; then\n",
            "  exec \"$SPUR_MPIRUN\" -np \"$_TASKS_ON_NODE\" --bind-to none --tag-output {quoted}\n",
            "else\n",
            "  exec \"$SPUR_MPIRUN\" -np \"$_TASKS_ON_NODE\" --bind-to none {quoted}\n",
            "fi\n",
        ),
        tasks_on_node = tasks_on_node,
        preamble = mpi_launch_preamble(),
        quoted = quoted,
    )
}

/// Bash prefix shared by PMIx direct-launch wrappers (multi-node per-rank fork).
///
/// Open MPI 4.x expects `PMIX_SERVER_URI4`/`URI3`; the same aliases exist in
/// `spurd::mpi_plugin` and `crates/spur-mpi-pmix/c/pmix_server.c`.
fn mpi_direct_task_preamble(indent: &str) -> String {
    format!(
        concat!(
            "{indent}unset PMI_RANK PMI_SIZE PMI_FD PMI_PORT PMI_PROCESS_KVS_ID 2>/dev/null || true\n",
            "{indent}unset OMPI_MCA_ess OMPI_MCA_ess_base_env 2>/dev/null || true\n",
            "{indent}unset SLURM_PROCID SLURM_LOCALID SLURM_NODEID SLURM_TASKS_PER_NODE 2>/dev/null || true\n",
            "{indent}export SLURM_STEP_ID=${{SLURM_STEP_ID:-0}}\n",
            "{indent}export SLURM_STEPID=${{SLURM_STEPID:-0}}\n",
            "{indent}export OMPI_MCA_ess='^singleton,^slurm,^srun'\n",
            "{indent}if [ -f \"${{HOME}}/spur/mpi/env.sh\" ]; then . \"${{HOME}}/spur/mpi/env.sh\"; fi\n",
            "{indent}export PMIX_SERVER_URI4=${{PMIX_SERVER_URI4:-$PMIX_SERVER_URI}}\n",
            "{indent}export PMIX_SERVER_URI3=${{PMIX_SERVER_URI3:-$PMIX_SERVER_URI}}\n",
        ),
        indent = indent
    )
}

fn bash_export_block(env: &HashMap<String, String>, indent: &str) -> String {
    let mut keys: Vec<_> = env.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| format!("{indent}export {key}={}\n", bash_single_quote(&env[key])))
        .collect()
}

/// Fork one process per local rank, injecting per-rank `PMIx_server_setup_fork` env.
pub fn build_multi_task_pmix_wrapper(
    user_script_path: &str,
    tasks_on_node: u32,
    per_local_rank_env: &[HashMap<String, String>],
    environment: Option<&HashMap<String, String>>,
) -> Result<String, String> {
    if per_local_rank_env.len() != tasks_on_node as usize {
        return Err(format!(
            "PMIx per-rank env count {} != tasks_on_node {tasks_on_node}",
            per_local_rank_env.len()
        ));
    }

    let quoted = bash_single_quote(user_script_path);
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
    wrapper.push_str("  case $LOCAL_RANK in\n");
    for (local_rank, rank_env) in per_local_rank_env.iter().enumerate() {
        wrapper.push_str(&format!("  {local_rank})\n"));
        wrapper.push_str(&mpi_direct_task_preamble("    "));
        wrapper.push_str(&bash_export_block(rank_env, "    "));
        wrapper.push_str("    ;;\n");
    }
    wrapper.push_str("  esac\n");

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
        "    {taskset_prefix}bash {quoted} 2>&1 | sed \"s/^/[$SPUR_PROCID] /\" &\n"
    ));
    wrapper.push_str("  else\n");
    wrapper.push_str(&format!("    {taskset_prefix}bash {quoted} &\n"));
    wrapper.push_str("  fi\n");
    wrapper.push_str("done\nwait\n");
    Ok(wrapper)
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
    fn use_multi_task_launch_batch_pmix_fans_out() {
        assert!(use_multi_task_launch(4, false, MPI_PMIX, "/tmp/hello_mpi"));
        assert!(!use_multi_task_launch(1, false, MPI_PMIX, "/tmp/hello_mpi"));
        assert!(!use_multi_task_launch(4, false, "none", "echo hi"));
        assert!(use_multi_task_launch(4, true, "none", "hostname"));
        assert!(!use_multi_task_launch(1, true, "none", "hostname"));
    }

    #[test]
    fn use_multi_task_launch_batch_pmix_skips_inner_srun() {
        let direct = "#!/bin/bash\n#SBATCH --mpi=pmix\n/tmp/hello_mpi\n";
        assert!(use_multi_task_launch(2, false, MPI_PMIX, direct));

        let inner_srun = "#!/bin/bash\n#SBATCH --mpi=pmix\nsrun --mpi=pmix /tmp/hello_mpi\n";
        assert!(!use_multi_task_launch(2, false, MPI_PMIX, inner_srun));

        let commented = "#!/bin/bash\n# srun should not match\n/tmp/hello_mpi\n";
        assert!(use_multi_task_launch(2, false, MPI_PMIX, commented));
    }

    #[test]
    fn batch_script_uses_step_launch_detects_launchers() {
        assert!(batch_script_uses_step_launch("srun --mpi=pmix /tmp/a\n"));
        assert!(batch_script_uses_step_launch("mpirun -np 4 /tmp/a\n"));
        assert!(batch_script_uses_step_launch("mpiexec -n 4 /tmp/a\n"));
        assert!(batch_script_uses_step_launch("&& srun hostname\n"));
        assert!(!batch_script_uses_step_launch(
            "#SBATCH --mpi=pmix\n/tmp/a\n"
        ));
        assert!(!batch_script_uses_step_launch("# srun hidden in comment\n"));
    }

    #[test]
    fn batch_script_uses_step_launch_ignores_launchers_in_quotes() {
        assert!(!batch_script_uses_step_launch(
            "echo \"this job launches no steps, but the word srun appears in this string\"\n",
        ));
        assert!(!batch_script_uses_step_launch(
            "echo \"token=use srun here\"\n"
        ));
        assert!(!batch_script_uses_step_launch(
            "echo \"token=prefixsrunsuffix\"\n"
        ));
        assert!(!batch_script_uses_step_launch(
            "echo 'srun is only in single quotes'\n"
        ));
        assert!(batch_script_uses_step_launch(
            "echo \"quoted\"; srun -n 2 hostname\n"
        ));
        assert!(batch_script_uses_step_launch(
            "srun hostname # real invocation, srun in comment ignored\n",
        ));
        assert!(!batch_script_uses_step_launch("echo \"x\"srun\n"));
        assert!(!batch_script_uses_step_launch("foo|\"x\"srun\n"));
        assert!(!batch_script_uses_step_launch("\"$VAR\"srun\n"));
        assert!(batch_script_uses_step_launch(
            "echo foo#bar; srun -n 2 hostname\n"
        ));
        assert!(!batch_script_uses_step_launch("echo ${VAR#prefix}\n"));
        // Unclosed quotes: remainder of line is treated as quoted; not full shell parsing.
        assert!(!batch_script_uses_step_launch(
            "echo \"unclosed; srun -n 2 hostname\n"
        ));
    }

    #[test]
    fn batch_dispatched_multi_node_pmix_direct_batch() {
        assert!(batch_dispatched_multi_node_pmix(
            Some(MPI_PMIX),
            2,
            Some("#!/bin/bash\n/tmp/hello_mpi\n"),
        ));
    }

    #[test]
    fn batch_dispatched_multi_node_pmix_skips_inner_srun() {
        assert!(!batch_dispatched_multi_node_pmix(
            Some(MPI_PMIX),
            2,
            Some("#!/bin/bash\nsrun --mpi=pmix /tmp/hello_mpi\n"),
        ));
    }

    #[test]
    fn batch_dispatched_multi_node_pmix_requires_multi_node() {
        assert!(!batch_dispatched_multi_node_pmix(
            Some(MPI_PMIX),
            1,
            Some("#!/bin/bash\n/tmp/hello_mpi\n"),
        ));
    }

    #[test]
    fn step_needs_pmix_prepare_inner_srun_batch() {
        let script = "#!/bin/bash\nsrun --mpi=pmix /tmp/hello_mpi\n";
        assert!(step_needs_pmix_prepare(2, Some(MPI_PMIX), 2, Some(script),));
    }

    #[test]
    fn step_needs_pmix_prepare_skips_after_direct_batch_pmix() {
        let script = "#!/bin/bash\n/tmp/hello_mpi\n";
        assert!(!step_needs_pmix_prepare(2, Some(MPI_PMIX), 2, Some(script),));
    }

    #[test]
    fn step_needs_pmix_prepare_hold_allocation_without_batch_mpi() {
        assert!(step_needs_pmix_prepare(
            2,
            None,
            2,
            Some("#!/bin/bash\nsleep 60\n")
        ));
    }

    #[test]
    fn multi_task_wrapper_honors_spur_label_env() {
        let script = build_multi_task_wrapper("/tmp/work.sh", 1, None);
        assert!(script.contains("SPUR_LABEL"));
        assert!(script.contains("sed \"s/^/[$SPUR_PROCID] /\""));
    }

    #[test]
    fn mpi_mpirun_wrapper_uses_single_mpirun() {
        let script = build_mpi_mpirun_wrapper("/tmp/hello_mpi", 4);
        assert!(script.contains("\"$SPUR_MPIRUN\" -np \"$_TASKS_ON_NODE\""));
        assert!(script.contains("_TASKS_ON_NODE=4"));
        assert!(script.contains("PMIX_SERVER_URI4"));
        assert!(script.contains("'/tmp/hello_mpi'"));
        assert!(!script.contains("for LOCAL_RANK in"));
    }

    #[test]
    fn mpi_mpirun_wrapper_single_quotes_user_script() {
        let script = build_mpi_mpirun_wrapper("$(rm -rf /)", 2);
        assert!(script.contains("'$(rm -rf /)'"));
        assert!(!script.contains("$(rm -rf /)\""));
    }

    #[test]
    fn pmix_direct_wrapper_single_quotes_user_script() {
        let mut rank0 = HashMap::new();
        rank0.insert("PMIX_RANK".into(), "0".into());
        let script = build_multi_task_pmix_wrapper("$(rm -rf /)", 1, &[rank0], None).unwrap();
        assert!(script.contains("'$(rm -rf /)'"));
        assert!(!script.contains("$(rm -rf /)\""));
    }

    #[test]
    fn pmix_direct_wrapper_exports_per_rank_env() {
        let mut rank0 = HashMap::new();
        rank0.insert("PMIX_RANK".into(), "0".into());
        rank0.insert("PMIX_SIZE".into(), "4".into());
        let mut rank1 = HashMap::new();
        rank1.insert("PMIX_RANK".into(), "1".into());
        rank1.insert("PMIX_SIZE".into(), "4".into());
        let script =
            build_multi_task_pmix_wrapper("/tmp/hello_mpi", 2, &[rank0, rank1], None).unwrap();
        assert!(script.contains("case $LOCAL_RANK in"));
        assert!(script.contains("0)\n"));
        assert!(script.contains("export PMIX_RANK='0'"));
        assert!(script.contains("export PMIX_RANK='1'"));
        assert!(script.contains("for LOCAL_RANK in"));
        assert!(!script.contains("mpirun"));
        assert!(script.contains("export SLURM_STEP_ID=${SLURM_STEP_ID:-0}"));
        assert!(script.contains("OMPI_MCA_ess='^singleton,^slurm,^srun'"));
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
    fn gpu_deny_visibility_sets_no_device_sentinels() {
        let mut env = HashMap::new();
        gpu_deny_visibility(&mut env);
        for var in [
            "ROCR_VISIBLE_DEVICES",
            "HIP_VISIBLE_DEVICES",
            "CUDA_VISIBLE_DEVICES",
            "GPU_DEVICE_ORDINAL",
            "ZE_AFFINITY_MASK",
        ] {
            assert_eq!(
                env.get(var).map(String::as_str),
                Some("-1"),
                "{var} must be -1 (invalid index = no devices)"
            );
        }
        // nvidia-container-runtime has its own no-GPU token, not an index.
        assert_eq!(
            env.get("NVIDIA_VISIBLE_DEVICES").map(String::as_str),
            Some("void")
        );
        // Spur's own allocated-GPU list: empty is "none", not a device index.
        assert_eq!(env.get("SPUR_JOB_GPUS").map(String::as_str), Some(""));
    }

    #[test]
    fn apply_gpu_bind_env_denies_when_no_gpus_allocated() {
        let mut target = HashMap::new();
        let mut source = HashMap::new();
        source.insert("SPUR_GPU_BIND".to_string(), "closest".to_string());
        apply_gpu_bind_env(&mut target, &source, &[]);
        assert_eq!(
            target.get("ROCR_VISIBLE_DEVICES").map(String::as_str),
            Some("-1")
        );
        assert_eq!(
            target.get("CUDA_VISIBLE_DEVICES").map(String::as_str),
            Some("-1")
        );
    }

    #[test]
    fn apply_gpu_bind_env_denies_map_gpu_when_no_gpus_allocated() {
        let mut target = HashMap::new();
        let mut source = HashMap::new();
        source.insert("SPUR_GPU_BIND".to_string(), "map_gpu:0".to_string());
        apply_gpu_bind_env(&mut target, &source, &[]);
        assert_eq!(
            target.get("ROCR_VISIBLE_DEVICES").map(String::as_str),
            Some("-1")
        );
        assert_eq!(
            target.get("CUDA_VISIBLE_DEVICES").map(String::as_str),
            Some("-1")
        );
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
