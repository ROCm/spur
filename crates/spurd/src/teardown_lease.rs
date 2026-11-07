// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A supervisor's claim on its node while it runs teardown work the agent must
//! not cut short.
//!
//! The agent force-reclaims a supervisor that outlives a cancel by a fixed
//! window, and reads a supervisor busy in a long cgroup reap or node epilog the
//! same as a frozen one. The lease is how the supervisor says "busy, until
//! this instant": the agent defers the reclaim while it is unexpired. A
//! supervisor that never writes one, or that stays up after its lease is gone,
//! is reclaimed on that same window.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const LEASE_FILE: &str = "teardown.lease";

/// Headroom beyond a bounded phase for the work around it: the SPANK exit
/// hooks, rootfs and spool removal, and the completion report. The agent's
/// force-reclaim window is this same duration, so a supervisor that writes
/// no lease is not reclaimed sooner than one whose lease just ran out, and
/// a lease never grants less headroom than that window.
pub(crate) const LEASE_SLACK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TeardownPhase {
    /// Waiting for the job cgroup to empty. Cut short, the cgroup cleanup
    /// record still quarantines the node on its own.
    Cleanup,
    /// The node epilog and SPANK exit hooks. Cut short, the epilog may be
    /// orphaned or never have run, and nothing else would drain the node.
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
    /// Wall clock, because the agent reads it from another process and across
    /// its own restarts.
    pub deadline_unix_ms: u64,
    #[serde(default)]
    pub hook: Option<HookProcess>,
}

impl TeardownLease {
    pub(crate) fn new(phase: TeardownPhase, window: Duration) -> io::Result<Self> {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        Ok(Self {
            phase,
            deadline_unix_ms: crate::cgroup_cleanup::unix_ms()?.saturating_add(window_ms),
            hook: None,
        })
    }

    pub(crate) fn with_hook(mut self, hook: HookProcess) -> Self {
        self.hook = Some(hook);
        self
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReclaimDecision {
    Reclaim,
    Defer(Duration),
}

/// Whether the agent may force-reclaim a supervisor that holds `lease`.
pub(crate) fn reclaim_decision(lease: Option<&TeardownLease>, now_unix_ms: u64) -> ReclaimDecision {
    match lease {
        Some(lease) if lease.deadline_unix_ms > now_unix_ms => {
            ReclaimDecision::Defer(Duration::from_millis(lease.deadline_unix_ms - now_unix_ms))
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

/// SIGKILLs the process group of a hook whose supervisor is gone. Returns
/// whether the signal was delivered. A leader that has exited, or whose pid
/// now belongs to something else, is left alone.
pub(crate) fn kill_orphaned_hook(hook: &HookProcess) -> bool {
    // Pid 0 and 1 would address the caller's own group and init. The pid
    // comes from the lease file, not from a process this function spawned.
    if hook.pid <= 1 || !crate::stepd::process_is_live(hook.pid, hook.start_ticks) {
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
        Err(error) => {
            tracing::warn!(pid = hook.pid, %error, "failed to kill an orphaned epilog process group");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    fn lease_expiring_at(deadline_unix_ms: u64) -> TeardownLease {
        TeardownLease {
            phase: TeardownPhase::Epilog,
            deadline_unix_ms,
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
            br#"{"phase":"epilog","deadline_unix_ms":5}"#,
        )
        .unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(lease_expiring_at(5)));
    }

    fn spawn_group_leader() -> Child {
        Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn killing_an_orphaned_hook_ends_its_whole_group() {
        let mut leader = spawn_group_leader();
        let hook = HookProcess::of(leader.id()).unwrap();
        assert!(kill_orphaned_hook(&hook));
        let status = leader.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
    }

    #[test]
    fn a_recycled_pid_is_not_mistaken_for_the_hook() {
        let mut stranger = spawn_group_leader();
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
    fn a_hook_that_already_exited_is_not_signalled() {
        let mut leader = spawn_group_leader();
        let hook = HookProcess::of(leader.id()).unwrap();
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(!kill_orphaned_hook(&hook));
    }
}
