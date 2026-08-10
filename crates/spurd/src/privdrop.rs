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

impl PrivDrop {
    /// Resolve credentials if privilege drop is needed. Returns None if
    /// spurd is not root or the job user is already root.
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

    /// Return nsenter --setuid/--setgid args for the namespace path.
    pub fn nsenter_args(&self) -> Vec<String> {
        vec![
            format!("--setuid={}", self.uid),
            format!("--setgid={}", self.gid),
        ]
    }
}
