// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local privilege gate for administrative CLI actions.
//!
//! This is a client-side guardrail, not a security boundary: the controller
//! does not authenticate callers, so it only stops ordinary users from running
//! privileged commands through the CLI. Real enforcement belongs server-side
//! behind an authenticated identity (future RBAC work).

use anyhow::{bail, Context, Result};

/// Unix groups whose members are treated as privileged, covering the
/// Debian/Ubuntu (`sudo`) and RHEL/CentOS (`wheel`) conventions.
const PRIVILEGED_GROUPS: &[&str] = &["sudo", "wheel"];

/// Pure decision core, split from the syscalls so it is unit-testable.
fn is_privileged(euid: u32, caller_groups: &[String], privileged_groups: &[&str]) -> bool {
    euid == 0
        || caller_groups
            .iter()
            .any(|g| privileged_groups.contains(&g.as_str()))
}

/// Resolve the invoking process's group names (supplementary groups plus the
/// effective gid). Returns `Err` on any lookup failure so callers fail closed.
fn caller_group_names() -> Result<Vec<String>> {
    use nix::unistd::{getegid, getgroups, Group};

    let mut gids = getgroups().context("failed to read process groups")?;
    gids.push(getegid());

    let mut names = Vec::new();
    for gid in gids {
        // A gid without a matching group entry cannot be privileged; skip it.
        if let Some(group) = Group::from_gid(gid).context("failed to resolve group name")? {
            names.push(group.name);
        }
    }
    Ok(names)
}

/// Gate a privileged CLI action. Fails closed: any identity/group lookup error
/// denies the action rather than allowing it.
pub fn require_privileged(action: &str) -> Result<()> {
    let euid = nix::unistd::geteuid().as_raw();
    // Root is always privileged; short-circuit so a group-lookup failure can
    // never deny root.
    if euid == 0 {
        return Ok(());
    }
    let groups = caller_group_names()?;
    if is_privileged(euid, &groups, PRIVILEGED_GROUPS) {
        return Ok(());
    }
    bail!(
        "insufficient privileges to {action}: requires root or membership in the 'sudo' or 'wheel' group"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIV: &[&str] = &["sudo", "wheel"];

    #[test]
    fn root_is_privileged_regardless_of_groups() {
        assert!(is_privileged(0, &[], PRIV));
    }

    #[test]
    fn sudo_member_is_privileged() {
        assert!(is_privileged(1000, &["users".into(), "sudo".into()], PRIV));
    }

    #[test]
    fn wheel_member_is_privileged() {
        assert!(is_privileged(1000, &["wheel".into()], PRIV));
    }

    #[test]
    fn plain_user_is_denied() {
        assert!(!is_privileged(
            1000,
            &["users".into(), "docker".into()],
            PRIV
        ));
    }

    #[test]
    fn empty_groups_are_denied() {
        // Simulates a lookup that resolved no privileged groups.
        assert!(!is_privileged(1000, &[], PRIV));
    }
}
