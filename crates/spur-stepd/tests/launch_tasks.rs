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
                task_offset: 0,
                user_script: user_script.to_string_lossy().into_owned(),
                label: false,
                cpu_bind: CpuBind::None,
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
