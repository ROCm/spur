// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A supervisor's claim on its node while it runs teardown work the agent must
//! not cut short. The agent defers its force-reclaim while the lease is
//! unexpired; without one, the supervisor is reclaimed on the usual window.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const LEASE_FILE: &str = "teardown.lease";

/// Headroom past a bounded phase for SPANK exit hooks, rootfs and spool
/// removal, and the completion report. Also the agent's reclaim window, so no
/// lease grants less time than a supervisor without one already gets.
pub(crate) const LEASE_SLACK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TeardownPhase {
    /// The cgroup reap, then rootfs and spool removal.
    Cleanup,
    /// The node epilog, then the SPANK exit hooks.
    Epilog,
}

/// A hook's leader, identified by start time as well as pid so a recycled pid
/// is never mistaken for it. The hook leads its own process group, so the pid
/// is also the group id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HookProcess {
    pub pid: u32,
    pub start_ticks: u64,
}

impl HookProcess {
    pub(crate) fn of(pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            start_ticks: crate::stepd::process_start_ticks(pid)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TeardownLease {
    pub phase: TeardownPhase,
    /// On [`boot_ms`]'s clock, which both processes share and which a
    /// wall-clock step cannot move.
    pub deadline_boot_ms: u64,
    #[serde(default)]
    pub hook: Option<HookProcess>,
}

impl TeardownLease {
    pub(crate) fn new(phase: TeardownPhase, window: Duration) -> io::Result<Self> {
        Ok(Self {
            phase,
            deadline_boot_ms: boot_ms()?.saturating_add(millis(window)),
            hook: None,
        })
    }

    pub(crate) fn with_hook(mut self, hook: HookProcess) -> Self {
        self.hook = Some(hook);
        self
    }
}

/// Milliseconds since boot, counting suspend. A reboot also ends the
/// supervisor, so the clock restarting with it never matters.
pub(crate) fn boot_ms() -> io::Result<u64> {
    let now = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)?;
    Ok(millis(Duration::from(now)))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The cleanup lease's window. `remaining` is what is left of the reap's
/// budget, or `None` when there is no cgroup left to wait for.
pub(crate) fn cleanup_window(remaining: Option<Duration>) -> Duration {
    remaining.unwrap_or_default().saturating_add(LEASE_SLACK)
}

/// The epilog lease's window. Without a script it covers only the SPANK exit
/// hooks and the report, which the slack already budgets for.
pub(crate) fn epilog_window(runs_script: bool, timeout: Duration) -> Duration {
    let script = if runs_script {
        spur_core::hooks::bounded_hook_ceiling(timeout)
    } else {
        Duration::ZERO
    };
    script.saturating_add(LEASE_SLACK)
}

/// The longest a supervisor's leases can legitimately defer its reclaim, since
/// the agent does not know the epilog timeout the supervisor started with.
/// Anything longer is a corrupt or runaway lease.
pub(crate) fn longest_hold(cleanup_timeout: Duration) -> Duration {
    let max_epilog_timeout = Duration::from_secs(spur_core::config::MAX_EPILOG_TIMEOUT_SECS.into());
    cleanup_window(Some(cleanup_timeout)).saturating_add(epilog_window(true, max_epilog_timeout))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReclaimDecision {
    Reclaim,
    Defer(Duration),
}

/// Whether the agent may force-reclaim a supervisor that holds `lease`.
pub(crate) fn reclaim_decision(lease: Option<&TeardownLease>, now_boot_ms: u64) -> ReclaimDecision {
    match lease {
        Some(lease) if lease.deadline_boot_ms > now_boot_ms => {
            ReclaimDecision::Defer(Duration::from_millis(lease.deadline_boot_ms - now_boot_ms))
        }
        _ => ReclaimDecision::Reclaim,
    }
}

/// Replaces any earlier lease in one step, so a phase change never leaves a
/// moment the agent would read as unleased.
pub(crate) fn grant(session_dir: &Path, lease: &TeardownLease) -> io::Result<()> {
    crate::stepd::publish_private(session_dir, LEASE_FILE, &serde_json::to_vec(lease)?)
}

pub(crate) fn read(session_dir: &Path) -> io::Result<Option<TeardownLease>> {
    match fs::read(session_dir.join(LEASE_FILE)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn clear(session_dir: &Path) -> io::Result<()> {
    match fs::remove_file(session_dir.join(LEASE_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// SIGKILLs what is left of the process group of a hook whose supervisor is
/// gone, including children that outlived the hook itself. Returns whether
/// any process received the signal.
pub(crate) fn kill_orphaned_hook(hook: &HookProcess) -> bool {
    // Pid 0 and 1 would address the caller's own group and init. The pid
    // comes from the lease file, not from a process this function spawned.
    if hook.pid <= 1 || !names_the_hooks_group(hook) {
        return false;
    }
    let Ok(pgid) = i32::try_from(hook.pid) else {
        return false;
    };
    match nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pgid),
        nix::sys::signal::Signal::SIGKILL,
    ) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(error) => {
            tracing::warn!(pid = hook.pid, %error, "failed to kill an orphaned epilog process group");
            false
        }
    }
}

/// Whether the hook's pid still identifies its process group. Linux does not
/// hand out a pid while a group of that id has members, so once the leader is
/// gone the id can only name the hook's leftover children, or nothing.
fn names_the_hooks_group(hook: &HookProcess) -> bool {
    match crate::stepd::process_start_ticks(hook.pid) {
        Ok(start_ticks) => start_ticks == hook.start_ticks,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    fn lease_expiring_at(deadline_boot_ms: u64) -> TeardownLease {
        TeardownLease {
            phase: TeardownPhase::Epilog,
            deadline_boot_ms,
            hook: None,
        }
    }

    #[test]
    fn an_unexpired_lease_defers_the_reclaim_for_the_time_left() {
        let lease = lease_expiring_at(10_000);
        assert_eq!(
            reclaim_decision(Some(&lease), 4_000),
            ReclaimDecision::Defer(Duration::from_millis(6_000))
        );
    }

    #[test]
    fn an_expired_or_absent_lease_permits_the_reclaim() {
        let lease = lease_expiring_at(10_000);
        for now in [10_000, 10_001] {
            assert_eq!(
                reclaim_decision(Some(&lease), now),
                ReclaimDecision::Reclaim
            );
        }
        assert_eq!(reclaim_decision(None, 0), ReclaimDecision::Reclaim);
    }

    #[test]
    fn a_new_lease_expires_one_window_from_now_since_boot() {
        let before = boot_ms().unwrap();
        let lease = TeardownLease::new(TeardownPhase::Cleanup, Duration::from_secs(60)).unwrap();
        let after = boot_ms().unwrap();
        assert!((before + 60_000..=after + 60_000).contains(&lease.deadline_boot_ms));
    }

    #[test]
    fn the_epilog_window_budgets_a_script_only_when_one_runs() {
        let timeout = Duration::from_secs(120);
        assert_eq!(epilog_window(false, timeout), Duration::from_secs(30));
        // 120s timeout, 5s per kill signal, 1s output drain, 30s slack.
        assert_eq!(epilog_window(true, timeout), Duration::from_secs(161));
    }

    #[test]
    fn the_cleanup_window_keeps_its_slack_when_the_cgroup_is_already_gone() {
        assert_eq!(
            cleanup_window(Some(Duration::from_secs(40))),
            Duration::from_secs(70)
        );
        assert_eq!(cleanup_window(None), Duration::from_secs(30));
    }

    #[test]
    fn the_longest_hold_covers_a_full_reap_and_the_longest_allowed_epilog() {
        let cleanup_timeout = Duration::from_secs(60);
        let max_epilog_timeout =
            Duration::from_secs(spur_core::config::MAX_EPILOG_TIMEOUT_SECS.into());
        assert_eq!(
            longest_hold(cleanup_timeout),
            cleanup_window(Some(cleanup_timeout)) + epilog_window(true, max_epilog_timeout)
        );
    }

    #[test]
    fn a_granted_lease_reads_back_and_a_new_grant_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);

        let cleanup = TeardownLease::new(TeardownPhase::Cleanup, Duration::from_secs(60)).unwrap();
        grant(dir.path(), &cleanup).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(cleanup.clone()));

        let epilog = TeardownLease::new(TeardownPhase::Epilog, Duration::from_secs(300))
            .unwrap()
            .with_hook(HookProcess {
                pid: 4242,
                start_ticks: 7,
            });
        grant(dir.path(), &epilog).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(epilog));
    }

    #[test]
    fn clearing_removes_the_lease_and_tolerates_none() {
        let dir = tempfile::tempdir().unwrap();
        clear(dir.path()).unwrap();
        grant(dir.path(), &lease_expiring_at(1)).unwrap();
        clear(dir.path()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_lease_without_a_recorded_hook_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(LEASE_FILE),
            br#"{"phase":"epilog","deadline_boot_ms":5}"#,
        )
        .unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(lease_expiring_at(5)));
    }

    fn spawn_group_leader(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
        std::os::unix::process::ExitStatusExt::signal(&status) == Some(libc::SIGKILL)
    }

    #[test]
    fn killing_an_orphaned_hook_ends_its_whole_group() {
        let mut leader = spawn_group_leader("exec sleep 300");
        let hook = HookProcess::of(leader.id()).unwrap();
        assert!(kill_orphaned_hook(&hook));
        assert!(killed_by_sigkill(leader.wait().unwrap()));
    }

    #[test]
    fn a_hook_whose_leader_exited_still_has_its_children_killed() {
        let mut leader = spawn_group_leader("sleep 300 &");
        let hook = HookProcess::of(leader.id()).unwrap();
        let mut held_by_child = leader.stdout.take().unwrap();
        leader.wait().unwrap();

        assert!(kill_orphaned_hook(&hook));
        // The backgrounded sleep inherited stdout, so EOF means it died.
        let mut rest = Vec::new();
        held_by_child.read_to_end(&mut rest).unwrap();
    }

    #[test]
    fn a_recycled_pid_is_not_mistaken_for_the_hook() {
        let mut stranger = spawn_group_leader("exec sleep 300");
        let hook = HookProcess {
            start_ticks: HookProcess::of(stranger.id()).unwrap().start_ticks + 1,
            ..HookProcess::of(stranger.id()).unwrap()
        };
        assert!(!kill_orphaned_hook(&hook));
        assert!(stranger.try_wait().unwrap().is_none());
        stranger.kill().unwrap();
        stranger.wait().unwrap();
    }

    #[test]
    fn a_hook_whose_whole_group_is_gone_is_not_signalled() {
        let mut leader = spawn_group_leader("exec sleep 300");
        let hook = HookProcess::of(leader.id()).unwrap();
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(!kill_orphaned_hook(&hook));
    }
}
