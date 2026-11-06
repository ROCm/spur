// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Drives the real `spurstepd launch-tasks` binary: each test starts real
//! tasks and checks what the step's supervisor would observe.

use std::io::{BufRead, BufReader};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Output, Stdio};

use spur_core::step::CpuBind;
use spurd::task_launcher::{LauncherInvocation, TaskPlan, SUBCOMMAND};

struct Step {
    dir: tempfile::TempDir,
    plan: TaskPlan,
    env: Vec<(String, String)>,
}

impl Step {
    fn new(tasks_on_node: u32, script: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let user_script = dir.path().join("task.sh");
        std::fs::write(&user_script, script).expect("write task script");
        Self {
            plan: TaskPlan {
                tasks_on_node,
                user_script: user_script.to_string_lossy().into_owned(),
                ..TaskPlan::default()
            },
            dir,
            env: Vec::new(),
        }
    }

    fn offset(mut self, task_offset: u32) -> Self {
        self.plan.task_offset = task_offset;
        self
    }

    fn labeled(mut self) -> Self {
        self.plan.label = true;
        self
    }

    fn cpu_bind(mut self, bind: CpuBind) -> Self {
        self.plan.cpu_bind = bind;
        self
    }

    fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// A PMIx launch whose ranks get these environments, with `HOME` pointing
    /// at the step's directory so a test can supply `spur/mpi/env.sh`.
    fn pmix(mut self, rank_env: &[&[(&str, &str)]]) -> Self {
        self.plan.mpi = true;
        self.plan.rank_env = rank_env
            .iter()
            .map(|vars| {
                vars.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .collect();
        let home = self.dir.path().to_string_lossy().into_owned();
        self.env("HOME", &home)
    }

    fn env_sh(self, content: &str) -> Self {
        let dir = self.dir.path().join("spur/mpi");
        std::fs::create_dir_all(&dir).expect("create spur/mpi");
        std::fs::write(dir.join("env.sh"), content).expect("write env.sh");
        self
    }

    fn command(&self) -> Command {
        let invocation = LauncherInvocation {
            plan: self.plan.clone(),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            seccomp: false,
            landlock_work_dir: None,
        };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_spurstepd"));
        cmd.arg(SUBCOMMAND)
            .arg(serde_json::to_string(&invocation).expect("serialize invocation"))
            .current_dir(self.dir.path())
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd
    }

    fn run(&self) -> Output {
        self.command().output().expect("run launcher")
    }
}

fn sorted_lines(bytes: &[u8]) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

#[test]
fn the_highest_task_exit_code_is_the_steps() {
    let out = Step::new(4, "exit $SPUR_LOCALID\n").run();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
}

#[test]
fn tasks_that_all_succeed_exit_zero() {
    let out = Step::new(3, "exit 0\n").run();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn a_signaled_task_outranks_exit_codes() {
    let out = Step::new(2, "[ \"$SPUR_LOCALID\" = 1 ] && kill -9 $$\nexit 7\n").run();
    assert_eq!(out.status.signal(), Some(libc::SIGKILL), "{out:?}");
}

#[test]
fn each_task_runs_with_its_own_ranks() {
    let out = Step::new(
        2,
        "echo \"$SPUR_PROCID $SLURM_PROCID $SPUR_LOCALID $SLURM_LOCALID\"\n",
    )
    .offset(4)
    .run();
    assert_eq!(sorted_lines(&out.stdout), ["4 4 0 0", "5 5 1 1"]);
}

#[test]
fn tasks_get_their_slice_of_the_nodes_gpus() {
    let out = Step::new(
        2,
        "echo \"$SPUR_LOCALID $ROCR_VISIBLE_DEVICES $HIP_VISIBLE_DEVICES\"\n",
    )
    .env("SPUR_JOB_GPUS", "0,1,2,3")
    .run();
    assert_eq!(sorted_lines(&out.stdout), ["0 0,1 0,1", "1 2,3 0,1"]);
}

#[test]
fn labels_prefix_every_line_with_the_rank_and_merge_stderr() {
    let out = Step::new(2, "echo out\necho err >&2\n").labeled().run();
    assert_eq!(
        sorted_lines(&out.stdout),
        ["[0] err", "[0] out", "[1] err", "[1] out"]
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_labeled_task_keeps_its_exit_code() {
    let out = Step::new(1, "exit 4\n").labeled().run();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
}

#[test]
fn a_labeled_partial_last_line_gets_no_newline() {
    let out = Step::new(1, "printf done\n").labeled().offset(2).run();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[2] done");
}

#[test]
fn a_cpu_map_shorter_than_the_ranks_fails_that_task() {
    let out = Step::new(1, "exit 0\n")
        .offset(1)
        .cpu_bind(CpuBind::Map("0".into()))
        .run();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("map_cpu: rank 1 exceeds CPU map (len 1)"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn pmix_ranks_run_the_mpi_preamble_with_their_own_environment() {
    let out = Step::new(
        2,
        "echo \"$SPUR_LOCALID $PMIX_RANK ${PMI_RANK:-unset} $OMPI_MCA_ess $FROM_ENV_SH\"\n",
    )
    .pmix(&[&[("PMIX_RANK", "0")], &[("PMIX_RANK", "1")]])
    .env_sh("export FROM_ENV_SH=yes\nexport PMIX_RANK=from-env-sh\n")
    .env("PMI_RANK", "stale")
    .run();
    assert_eq!(
        sorted_lines(&out.stdout),
        [
            "0 0 unset ^singleton,^slurm,^srun yes",
            "1 1 unset ^singleton,^slurm,^srun yes"
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_failing_pmix_rank_fails_the_step() {
    let out = Step::new(2, "exit $((SPUR_LOCALID + 4))\n")
        .pmix(&[&[], &[]])
        .run();
    assert_eq!(out.status.code(), Some(5), "{out:?}");
}

#[test]
fn a_lone_pmix_rank_without_its_own_environment_still_gets_the_preamble() {
    let out = Step::new(1, "echo \"$OMPI_MCA_ess\"\n").pmix(&[]).run();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "^singleton,^slurm,^srun\n"
    );
}

#[test]
fn rank_environments_that_do_not_match_the_tasks_are_refused() {
    let out = Step::new(2, "exit 0\n").pmix(&[&[]]).run();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("rank environment"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Spur signals a step's whole process group or cgroup, so the launcher sees
/// the signal alongside its tasks and must outlive them to report it.
fn signal_running_tasks(signal: libc::c_int) -> std::process::ExitStatus {
    let step = Step::new(2, "echo ready\nexec sleep 600\n");
    let mut child = step
        .command()
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn launcher");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    for _ in 0..2 {
        assert_eq!(lines.next().expect("a task line").expect("read"), "ready");
    }
    let pgid = child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::killpg(pgid, signal) }, 0);
    child.wait().expect("wait launcher")
}

#[test]
fn the_launcher_outlives_a_group_signal_and_reports_it() {
    assert_eq!(
        signal_running_tasks(libc::SIGTERM).signal(),
        Some(libc::SIGTERM)
    );
}

#[test]
fn tasks_receive_sigint() {
    assert_eq!(
        signal_running_tasks(libc::SIGINT).signal(),
        Some(libc::SIGINT)
    );
}
