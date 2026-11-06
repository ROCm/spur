// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spurstepd launch-tasks`: starts a step's tasks on one node and waits on
//! each of them, the role `slurmstepd` plays for Slurm. It runs as the
//! supervised workload in place of a generated shell wrapper, so the step's
//! exit status is the tasks' combined status rather than the wrapper's.

mod bind;
mod env;
mod label;
mod run;
mod status;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use spur_core::step::CpuBind;

pub use run::run_from_args;
pub use status::TaskStatus;

/// The argv word that selects launcher mode in the `spurstepd` binary.
pub const SUBCOMMAND: &str = "launch-tasks";

/// What spurd decides about a step's tasks on this node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub tasks_on_node: u32,
    /// Global rank of this node's first task.
    pub task_offset: u32,
    /// Script each task runs with `bash`.
    pub user_script: String,
    /// `srun -l`: prefix output lines with the task's rank.
    pub label: bool,
    pub cpu_bind: CpuBind,
    /// `--mpi=pmix`: each task runs the preamble a PMIx rank needs first.
    #[serde(default)]
    pub mpi: bool,
    /// Each local rank's `PMIx_server_setup_fork` environment. Only the process
    /// hosting the PMIx server can produce it, so the supervisor fills it in.
    #[serde(default)]
    pub rank_env: Vec<HashMap<String, String>>,
}

/// What the supervisor adds when it starts the launcher. The launcher may run
/// as root, so everything privileged comes from here and never from the job's
/// environment, which the user controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherInvocation {
    pub plan: TaskPlan,
    /// Each task drops to this user when the launcher runs as root.
    pub uid: u32,
    pub gid: u32,
    pub seccomp: bool,
    /// Work dir to confine each task to, when Landlock is on.
    pub landlock_work_dir: Option<String>,
}

/// The argv a supervisor runs to start `invocation`'s tasks with the launcher
/// at `launcher`.
pub fn launcher_argv(
    launcher: &Path,
    invocation: &LauncherInvocation,
) -> anyhow::Result<Vec<String>> {
    Ok(vec![
        launcher.to_string_lossy().into_owned(),
        SUBCOMMAND.to_string(),
        serde_json::to_string(invocation).context("encode the launcher invocation")?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_argv_carries_the_invocation_intact() {
        let invocation = LauncherInvocation {
            plan: TaskPlan {
                tasks_on_node: 2,
                task_offset: 6,
                user_script: "/work/.spur_step_9/cmd_3_1.sh".into(),
                label: false,
                cpu_bind: CpuBind::Map("0,2".into()),
                mpi: true,
                rank_env: vec![HashMap::from([("PMIX_RANK".to_string(), "6".to_string())])],
            },
            uid: 1000,
            gid: 1000,
            seccomp: true,
            landlock_work_dir: Some("/work".into()),
        };

        let argv = launcher_argv(Path::new("/opt/spur/spurstepd"), &invocation).expect("argv");

        assert_eq!(argv[..2], ["/opt/spur/spurstepd", SUBCOMMAND]);
        let decoded: LauncherInvocation = serde_json::from_str(&argv[2]).expect("decode");
        assert_eq!(decoded, invocation);
        assert_eq!(argv.len(), 3);
    }
}
