// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{bail, Context};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sched::CpuSet;
use nix::sys::signal::{SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::unistd::Pid;

use super::bind::task_affinity;
use super::env::{gpu_slice_env, rank_ids};
use super::label::LineLabeler;
use super::status::{aggregate, TaskStatus};
use super::{LauncherInvocation, SUBCOMMAND};
use crate::privdrop::PrivDrop;

/// Spur signals a step's whole process group or cgroup, so tasks receive these
/// directly. The launcher holds them so it outlives its tasks and can report
/// how they ended.
const HELD_SIGNALS: [Signal; 7] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGALRM,
];

/// Entry point for `spurstepd launch-tasks <invocation-json>`.
pub fn run_from_args(args: &[String]) -> ! {
    match parse(args).and_then(|invocation| launch(&invocation)) {
        Ok(status) => exit_with(status),
        Err(error) => {
            eprintln!("spurstepd {SUBCOMMAND}: {error:#}");
            std::process::exit(1);
        }
    }
}

fn parse(args: &[String]) -> anyhow::Result<LauncherInvocation> {
    let [invocation] = args else {
        bail!("expected one argument, got {}", args.len());
    };
    serde_json::from_str(invocation).context("invalid invocation")
}

fn launch(invocation: &LauncherInvocation) -> anyhow::Result<TaskStatus> {
    let plan = &invocation.plan;
    // A lone rank's PMIx environment is already in the process environment;
    // several ranks each need their own, or they would rendezvous as one.
    if plan.mpi && plan.tasks_on_node > 1 && plan.rank_env.len() != plan.tasks_on_node as usize {
        bail!(
            "got {} PMIx rank environments for {} tasks",
            plan.rank_env.len(),
            plan.tasks_on_node
        );
    }

    let mut held = SigSet::empty();
    for signal in HELD_SIGNALS {
        held.add(signal);
    }
    held.add(Signal::SIGCHLD);
    // Blocked before the first fork, so no task exit can slip past the signalfd.
    nix::sys::signal::sigprocmask(SigmaskHow::SIG_BLOCK, Some(&held), None)
        .context("block signals")?;
    let mut child_exits = SigSet::empty();
    child_exits.add(Signal::SIGCHLD);
    let exits = SignalFd::with_flags(&child_exits, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
        .context("signalfd")?;

    let setup = Arc::new(TaskSetup {
        privdrop: PrivDrop::resolve_if_needed(invocation.uid, invocation.gid),
        seccomp: invocation.seccomp,
        landlock_work_dir: invocation.landlock_work_dir.clone(),
    });
    let job_gpus = std::env::var("SPUR_JOB_GPUS").ok();

    let mut running = HashSet::new();
    let mut outputs = Vec::new();
    let mut statuses = Vec::new();
    for local_id in 0..plan.tasks_on_node {
        match spawn_task(invocation, &setup, local_id, job_gpus.as_deref()) {
            Ok(task) => {
                running.insert(task.pid);
                outputs.extend(task.output);
            }
            Err(failure) => {
                eprintln!("{}", failure.message);
                statuses.push(TaskStatus::Exited(failure.code));
            }
        }
    }
    supervise(&exits, running, outputs, &mut statuses)?;
    Ok(aggregate(statuses))
}

struct Task {
    pid: Pid,
    output: Option<LabeledOutput>,
}

struct LabeledOutput {
    pipe: OwnedFd,
    labeler: LineLabeler,
}

struct Failure {
    message: String,
    code: i32,
}

/// What each task applies to itself between fork and exec.
struct TaskSetup {
    privdrop: Option<PrivDrop>,
    seccomp: bool,
    landlock_work_dir: Option<String>,
}

impl TaskSetup {
    /// Runs in the forked task. The launcher is single-threaded, so ordinary
    /// code is safe here.
    fn apply(&self, cpus: Option<&CpuSet>) {
        // A task inherits the launcher's held signals and Rust's ignored
        // SIGPIPE; it must start with the defaults a shell would give it.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        }
        if let Some(privdrop) = &self.privdrop {
            if let Err(error) = privdrop.apply() {
                eprintln!("spurstepd: cannot drop privileges: {error}");
                exit_task(1);
            }
        }
        if let Some(cpus) = cpus {
            if let Err(error) = nix::sched::sched_setaffinity(Pid::from_raw(0), cpus) {
                eprintln!(
                    "taskset: failed to set pid {}'s affinity: {}",
                    std::process::id(),
                    error.desc()
                );
                exit_task(1);
            }
        }
        if self.seccomp {
            if let Err(error) = crate::seccomp::apply_seccomp_filter() {
                eprintln!("spur: seccomp filter not applied: {error}");
            }
        }
        if let Some(work_dir) = &self.landlock_work_dir {
            if let Err(error) = crate::landlock::apply_landlock_rules(work_dir) {
                eprintln!("spur: landlock not applied: {error}");
            }
        }
    }
}

fn exit_task(code: i32) -> ! {
    // _exit, not exit: the forked task must not run the launcher's atexit work.
    unsafe { libc::_exit(code) }
}

fn spawn_task(
    invocation: &LauncherInvocation,
    setup: &Arc<TaskSetup>,
    local_id: u32,
    job_gpus: Option<&str>,
) -> Result<Task, Failure> {
    let plan = &invocation.plan;
    let rank = plan.task_offset + local_id;
    let cpus = task_affinity(&plan.cpu_bind, rank)
        .map_err(|message| Failure { message, code: 1 })?
        .map(|cpus| cpu_set(&cpus))
        .transpose()
        .map_err(|message| Failure { message, code: 1 })?;

    let mut cmd = Command::new("bash");
    cmd.envs(rank_ids(plan.task_offset, local_id));
    let gpus = gpu_slice_env(local_id, plan.tasks_on_node, job_gpus);
    if plan.mpi {
        let rank_env = plan
            .rank_env
            .get(local_id as usize)
            .cloned()
            .unwrap_or_default();
        // The GPU slice goes after the site's env.sh, as the rank wrapper did.
        cmd.arg("-c")
            .arg(spur_core::task_launch::mpi_rank_program(&rank_env, &gpus));
    } else {
        cmd.envs(gpus);
    }
    cmd.arg(&plan.user_script);
    let output = if plan.label {
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            .map_err(|error| task_failure(rank, "create output pipe", error.into()))?;
        let write_stderr = write
            .try_clone()
            .map_err(|error| task_failure(rank, "create output pipe", error))?;
        cmd.stdout(Stdio::from(write))
            .stderr(Stdio::from(write_stderr));
        Some(LabeledOutput {
            pipe: read,
            labeler: LineLabeler::new(rank),
        })
    } else {
        None
    };

    let setup = Arc::clone(setup);
    unsafe {
        cmd.pre_exec(move || {
            setup.apply(cpus.as_ref());
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|error| task_failure(rank, "cannot start bash", error))?;
    // `cmd` drops on return, closing the launcher's copies of the pipe's write
    // end so the task's exit reads as EOF.
    Ok(Task {
        pid: Pid::from_raw(child.id() as i32),
        output,
    })
}

fn task_failure(rank: u32, what: &str, error: std::io::Error) -> Failure {
    // The codes a shell reports for a command it cannot find or cannot run.
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    };
    Failure {
        message: format!("spurstepd: task {rank}: {what}: {error}"),
        code,
    }
}

fn cpu_set(cpus: &[usize]) -> Result<CpuSet, String> {
    let mut set = CpuSet::new();
    for &cpu in cpus {
        set.set(cpu)
            .map_err(|_| format!("taskset: CPU {cpu} is out of range"))?;
    }
    Ok(set)
}

/// Pump labeled output and reap tasks until every task has exited and every
/// output pipe has closed, as `bash ... | sed` waited for EOF.
fn supervise(
    exits: &SignalFd,
    mut running: HashSet<Pid>,
    mut outputs: Vec<LabeledOutput>,
    statuses: &mut Vec<TaskStatus>,
) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut stdout_broken = false;
    let mut buf = vec![0u8; 64 * 1024];
    let mut labeled = Vec::new();
    while !running.is_empty() || !outputs.is_empty() {
        let mut fds: Vec<libc::pollfd> = outputs
            .iter()
            .map(|output| readable(output.pipe.as_raw_fd()))
            .chain(std::iter::once(readable(exits.as_raw_fd())))
            .collect();
        poll(&mut fds)?;

        // Highest index first, so swap_remove only moves already-handled pipes.
        for i in (0..outputs.len()).rev() {
            if fds[i].revents == 0 {
                continue;
            }
            labeled.clear();
            match nix::unistd::read(&outputs[i].pipe, &mut buf) {
                Ok(n) if n > 0 => outputs[i].labeler.feed(&buf[..n], &mut labeled),
                Err(Errno::EINTR | Errno::EAGAIN) => continue,
                // EOF, or an error that leaves nothing more to read.
                _ => {
                    let mut finished = outputs.swap_remove(i);
                    finished.labeler.finish(&mut labeled);
                }
            }
            if !stdout_broken && stdout.write_all(&labeled).is_err() {
                stdout_broken = true;
                eprintln!("spurstepd: cannot write task output; dropping the rest");
            }
        }
        while exits.read_signal().context("read signalfd")?.is_some() {}
        reap(&mut running, statuses)?;
    }
    let _ = stdout.flush();
    Ok(())
}

fn readable(fd: libc::c_int) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

fn poll(fds: &mut [libc::pollfd]) -> anyhow::Result<()> {
    loop {
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready >= 0 {
            return Ok(());
        }
        let error = Errno::last();
        if error != Errno::EINTR {
            return Err(error).context("poll");
        }
    }
}

/// Collect every exited child without blocking. A child that is not one of our
/// tasks is an orphan reparented to us as pid 1 of the step's pid namespace.
fn reap(running: &mut HashSet<Pid>, statuses: &mut Vec<TaskStatus>) -> anyhow::Result<()> {
    loop {
        let mut raw = 0;
        // libc rather than nix: nix's typed status cannot name real-time signals.
        let pid = unsafe { libc::waitpid(-1, &mut raw, libc::WNOHANG) };
        if pid == 0 {
            return Ok(());
        }
        if pid < 0 {
            match Errno::last() {
                Errno::ECHILD => return Ok(()),
                Errno::EINTR => continue,
                error => return Err(error).context("waitpid"),
            }
        }
        let status = if libc::WIFEXITED(raw) {
            TaskStatus::Exited(libc::WEXITSTATUS(raw))
        } else if libc::WIFSIGNALED(raw) {
            TaskStatus::Signaled(libc::WTERMSIG(raw))
        } else {
            continue;
        };
        if running.remove(&Pid::from_raw(pid)) {
            statuses.push(status);
        }
    }
}

fn exit_with(status: TaskStatus) -> ! {
    match status {
        TaskStatus::Exited(code) => std::process::exit(code),
        TaskStatus::Signaled(signal) => die_by(signal),
    }
}

/// Re-raise the tasks' signal so the supervisor sees the wait status it would
/// have seen from a single task.
fn die_by(signal: libc::c_int) -> ! {
    unsafe {
        // A core dump of the launcher would only mislead.
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        // Other held signals may be pending; unblocking must not let one of
        // them win instead.
        for held in HELD_SIGNALS {
            if held as libc::c_int != signal {
                libc::signal(held as libc::c_int, libc::SIG_IGN);
            }
        }
        libc::signal(signal, libc::SIG_DFL);
        let mut all: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::sigprocmask(libc::SIG_UNBLOCK, &all, std::ptr::null_mut());
        libc::raise(signal);
    }
    // Reached as PID 1 of the step's pid namespace, where the kernel ignores a
    // signal a process sends itself; the shell's encoding carries the same
    // number to the supervisor.
    std::process::exit(128 + signal)
}
