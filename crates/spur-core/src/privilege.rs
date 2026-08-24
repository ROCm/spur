// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Who counts as a privileged operator: root, or a member of `sudo`/`wheel`.
//!
//! Shared by the CLI and the controller so the two cannot drift. The CLI asks about the invoking
//! process, the controller about a username through NSS — which is what makes the rule enforceable
//! against a caller that never went through the CLI. Both fail closed.

use crate::auth::AuthError;

/// Unix groups whose members are treated as privileged, covering the Debian/Ubuntu (`sudo`) and
/// RHEL/CentOS (`wheel`) conventions.
pub const PRIVILEGED_GROUPS: &[&str] = &["sudo", "wheel"];

/// The requirement, worded once so the CLI's pre-check and the controller's denial read the same.
pub const PRIVILEGE_REQUIREMENT: &str =
    "requires root or membership in the 'sudo' or 'wheel' group";

/// Pure decision core, split from the syscalls so it is unit-testable.
pub fn is_privileged(uid: u32, group_names: &[String], privileged_groups: &[&str]) -> bool {
    uid == 0
        || group_names
            .iter()
            .any(|g| privileged_groups.contains(&g.as_str()))
}

/// Group names of the *invoking process*: supplementary groups plus the effective gid.
pub fn current_process_groups() -> Result<Vec<String>, AuthError> {
    use nix::unistd::{getegid, getgroups};

    let mut gids = getgroups()
        .map_err(|e| AuthError::PermissionDenied(format!("read process groups: {e}")))?;
    gids.push(getegid());
    Ok(resolve_group_names(&gids))
}

/// Whether `user` is privileged on *this host*, resolved through NSS — so it holds for any caller,
/// including one that bypassed the CLI. Assumes the controller shares a user directory with the
/// login nodes; a name it cannot resolve is an error, never an allow.
pub fn named_user_is_privileged(user: &str) -> Result<bool, AuthError> {
    let (uid, gid) = crate::auth::resolve_unix_credentials(user)?;
    if uid == 0 {
        return Ok(true);
    }
    let groups = user_group_names(user, gid)?;
    Ok(is_privileged(uid, &groups, PRIVILEGED_GROUPS))
}

/// Group names of a named user. Uses `getgrouplist` rather than reading `/etc/group` so
/// directory-provided supplementary groups (LDAP/sssd) count.
fn user_group_names(user: &str, gid: u32) -> Result<Vec<String>, AuthError> {
    let name = std::ffi::CString::new(user)
        .map_err(|_| AuthError::UnknownUser(format!("{user}: embedded NUL")))?;
    let gids = nix::unistd::getgrouplist(&name, nix::unistd::Gid::from_raw(gid))
        .map_err(|e| AuthError::PermissionDenied(format!("read groups for '{user}': {e}")))?;
    Ok(resolve_group_names(&gids))
}

/// A gid that does not resolve is dropped rather than failing the lookup: it cannot be privileged,
/// and one flaky directory entry among a user's groups must not deny an operator who is in `sudo`.
fn resolve_group_names(gids: &[nix::unistd::Gid]) -> Vec<String> {
    gids.iter()
        .filter_map(|gid| nix::unistd::Group::from_gid(*gid).ok().flatten())
        .map(|group| group.name)
        .collect()
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

    /// On the wire an empty username is what an old or hand-written client sends; treating it as
    /// trusted is how "no user" becomes "root".
    #[test]
    fn empty_user_is_an_error_not_an_allow() {
        let err = named_user_is_privileged("").expect_err("empty user must not resolve");
        assert!(matches!(err, AuthError::UnknownUser(_)));
    }

    /// A NUL in the name cannot reach `getgrouplist`; it must be an error rather than a panic.
    #[test]
    fn user_name_with_nul_is_rejected() {
        assert!(matches!(
            user_group_names("al\0ice", 1000),
            Err(AuthError::UnknownUser(_))
        ));
    }
}
