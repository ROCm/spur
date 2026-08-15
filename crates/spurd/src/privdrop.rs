// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use nix::unistd::{Gid, Uid};

/// Pre-resolved credentials for privilege drop. Resolve in the parent
/// (where allocation is safe), apply in the child (async-signal-safe only).
pub(crate) struct PrivDrop {
    uid: Uid,
    gid: Gid,
    groups: Vec<Gid>,
}

/// Whether this spurd process runs as root — the production input to
/// [`check_root_execution_allowed`].
pub(crate) fn spurd_runs_as_root() -> bool {
    nix::unistd::geteuid().is_root()
}


/// Refuse to execute work as uid 0 unless the operator opted in.
///
/// `PrivDrop::resolve_if_needed` returns `None` for uid 0, which means "no privilege drop" — so a
/// request carrying `uid: 0` does not fail, it runs with spurd's own (root) credentials. Since the
/// uid arrives on the wire as part of the job spec and no RPC authenticates its caller, that turns
/// reachability of the agent into root on the node. Callers must run this check BEFORE spawning any
/// child, on every execution path (batch jobs, steps, container jobs).
///
/// `spurd_is_root` is a parameter rather than a `geteuid()` call inside, so the refusal branch is
/// testable on an unprivileged CI runner — otherwise the security-critical `Err` path would only
/// execute when the test process happened to be root, i.e. never where it normally runs.
///
/// Returns `Err` with an operator-facing message when the request must be refused.
pub(crate) fn check_root_execution_allowed(
    uid: u32,
    allow_root_jobs: bool,
    spurd_is_root: bool,
) -> Result<(), String> {
    if uid != 0 || allow_root_jobs || !spurd_is_root {
        return Ok(());
    }
    Err(
        "refusing to execute as uid 0: the job requested root and spurd is running as root, but \
         [auth] allow_root_jobs is false. Submit as an unprivileged user, or set \
         allow_root_jobs = true if every submitter on this cluster is trusted with root."
            .to_string(),
    )
}

impl PrivDrop {
    /// Resolve credentials if privilege drop is needed. Returns None if
    /// spurd is not root or the job user is already root.
    ///
    /// SECURITY: `None` here means "run with spurd's credentials". For uid 0 that is root, so every
    /// caller must first gate on [`check_root_execution_allowed`].
    pub fn resolve_if_needed(uid: u32, gid: u32) -> Option<Self> {
        if uid == 0 || !nix::unistd::geteuid().is_root() {
            return None;
        }
        let gid_nix = Gid::from_raw(gid);
        let groups = nix::unistd::User::from_uid(Uid::from_raw(uid))
            .ok()
            .flatten()
            .and_then(|u| std::ffi::CString::new(u.name).ok())
            .and_then(|name| nix::unistd::getgrouplist(&name, gid_nix).ok())
            .unwrap_or_else(|| {
                tracing::warn!(
                    uid,
                    gid,
                    "user not found in /etc/passwd; falling back to primary gid only"
                );
                vec![gid_nix]
            });

        Some(Self {
            uid: Uid::from_raw(uid),
            gid: gid_nix,
            groups,
        })
    }

    /// Apply inside a pre_exec closure (async-signal-safe: setgroups+setgid+setuid).
    pub fn apply(&self) -> nix::Result<()> {
        nix::unistd::setgroups(&self.groups)?;
        nix::unistd::setgid(self.gid)?;
        nix::unistd::setuid(self.uid)?;
        Ok(())
    }

    /// Args to drop privilege *after* namespace entry, initialising the full
    /// supplementary group set. nsenter's --setuid/--setgid call only
    /// setuid/setgid (never setgroups), so groups gated on /dev/kfd (render,
    /// video) are lost; `setpriv --init-groups` resolves them via NSS inside
    /// the target namespace, mirroring the batch wrapper.
    pub fn setpriv_prefix(&self) -> Vec<String> {
        vec![
            "setpriv".into(),
            format!("--reuid={}", self.uid),
            format!("--regid={}", self.gid),
            "--init-groups".into(),
            "--".into(),
        ]
    }
}

#[cfg(test)]
impl PrivDrop {
    /// Construct with explicit credentials, bypassing the root/NSS resolution
    /// in `resolve_if_needed` so arg-construction can be tested off-host.
    pub(crate) fn for_test(uid: u32, gid: u32) -> Self {
        Self {
            uid: Uid::from_raw(uid),
            gid: Gid::from_raw(gid),
            groups: vec![Gid::from_raw(gid)],
        }
    }
}

#[cfg(test)]
mod root_execution_tests {
    use super::check_root_execution_allowed;

    #[test]
    fn non_root_uid_is_always_allowed() {
        assert!(check_root_execution_allowed(1000, false, true).is_ok());
        assert!(check_root_execution_allowed(1000, true, true).is_ok());
    }

    #[test]
    fn opt_in_allows_root() {
        assert!(check_root_execution_allowed(0, true, true).is_ok());
    }

    // The security-critical branch, exercised regardless of whether the test runner is root.
    #[test]
    fn uid_zero_is_refused_by_default_when_spurd_is_root() {
        let msg = check_root_execution_allowed(0, false, true)
            .expect_err("uid 0 must be refused by default on a root spurd");
        assert!(msg.contains("allow_root_jobs"), "actionable message: {msg}");
    }

    #[test]
    fn a_non_root_spurd_cannot_escalate_so_the_guard_is_a_no_op() {
        assert!(check_root_execution_allowed(0, false, false).is_ok());
    }

    #[test]
    fn opt_in_permits_root_even_on_a_root_spurd() {
        assert!(check_root_execution_allowed(0, true, true).is_ok());
    }

    #[test]
    fn a_normal_uid_is_never_refused() {
        for is_root in [true, false] {
            for allow in [true, false] {
                assert!(check_root_execution_allowed(1000, allow, is_root).is_ok());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setpriv_prefix_emits_init_groups() {
        let pd = PrivDrop::for_test(1000, 1000);
        assert_eq!(
            pd.setpriv_prefix(),
            vec![
                "setpriv",
                "--reuid=1000",
                "--regid=1000",
                "--init-groups",
                "--"
            ]
        );
    }
}
