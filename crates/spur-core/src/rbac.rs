// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixed Spur roles. Sites assign them; they cannot invent new ones.

use crate::auth::Identity;
use crate::config::AuthConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    User = 1,
    Coordinator = 2,
    Operator = 3,
    Administrator = 4,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Coordinator => "Coordinator",
            Self::Operator => "Operator",
            Self::Administrator => "Administrator",
        }
    }

    pub fn operates_jobs(self) -> bool {
        self >= Self::Operator
    }

    pub fn administers_cluster(self) -> bool {
        self == Self::Administrator
    }
}

/// Highest role from the verified identity, accounting level, config maps, and NSS groups.
///
/// Accounting-derived Operator/Administrator fail closed when `accounting_loaded` is false.
pub fn resolve_role(
    identity: &Identity,
    auth: &AuthConfig,
    accounting_level: Option<&str>,
    accounting_loaded: bool,
    nss_groups: &[String],
    coordinator: bool,
) -> Role {
    if is_administrator(
        identity,
        auth,
        accounting_level,
        accounting_loaded,
        nss_groups,
    ) {
        return Role::Administrator;
    }
    if is_operator(
        identity,
        auth,
        accounting_level,
        accounting_loaded,
        nss_groups,
    ) {
        return Role::Operator;
    }
    if coordinator {
        Role::Coordinator
    } else {
        Role::User
    }
}

fn name_listed(names: &[String], user: &str) -> bool {
    names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(user) && !n.is_empty())
}

fn group_listed(configured: &[String], nss_groups: &[String]) -> bool {
    configured.iter().any(|want| {
        nss_groups
            .iter()
            .any(|have| have.eq_ignore_ascii_case(want) && !want.is_empty())
    })
}

fn is_administrator(
    identity: &Identity,
    auth: &AuthConfig,
    accounting_level: Option<&str>,
    accounting_loaded: bool,
    nss_groups: &[String],
) -> bool {
    if identity.is_admin {
        return true;
    }
    if name_listed(&auth.cluster_admins, &identity.user) {
        return true;
    }
    if group_listed(&auth.admin_groups, nss_groups) {
        return true;
    }
    if auth.allow_uid_zero_administrator && identity.uid == 0 && identity.trusted_unix {
        return true;
    }
    accounting_loaded
        && accounting_level
            .is_some_and(|lvl| matches!(crate_admin_level(lvl), Some("Administrator")))
}

fn is_operator(
    identity: &Identity,
    auth: &AuthConfig,
    accounting_level: Option<&str>,
    accounting_loaded: bool,
    nss_groups: &[String],
) -> bool {
    if group_listed(&auth.operator_groups, nss_groups) {
        return true;
    }
    let _ = identity;
    accounting_loaded
        && accounting_level.is_some_and(|lvl| {
            matches!(
                crate_admin_level(lvl),
                Some("Operator") | Some("Administrator")
            )
        })
}

fn crate_admin_level(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "none" => Some("None"),
        "operator" => Some("Operator"),
        "admin" | "administrator" | "superuser" => Some("Administrator"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(user: &str, uid: u32, admin: bool) -> Identity {
        Identity {
            user: user.into(),
            uid,
            gid: uid,
            is_admin: admin,
            trusted_unix: true,
        }
    }

    fn auth() -> AuthConfig {
        AuthConfig {
            cluster_admins: vec!["erin".into()],
            admin_groups: vec!["gpu-admins".into()],
            operator_groups: vec!["acct-ops".into()],
            allow_uid_zero_administrator: true,
            ..Default::default()
        }
    }

    #[test]
    fn jwt_admin_is_administrator() {
        assert_eq!(
            resolve_role(&id("alice", 1000, true), &auth(), None, false, &[], false),
            Role::Administrator
        );
    }

    #[test]
    fn operator_group_is_operator_not_administrator() {
        let role = resolve_role(
            &id("bob", 1000, false),
            &auth(),
            None,
            true,
            &["acct-ops".into()],
            false,
        );
        assert_eq!(role, Role::Operator);
        assert!(role.operates_jobs());
        assert!(!role.administers_cluster());
    }

    #[test]
    fn accounting_operator_fails_closed_before_cache_load() {
        assert_eq!(
            resolve_role(
                &id("carol", 1000, false),
                &AuthConfig::default(),
                Some("Operator"),
                false,
                &[],
                false,
            ),
            Role::User
        );
        assert_eq!(
            resolve_role(
                &id("carol", 1000, false),
                &AuthConfig::default(),
                Some("Operator"),
                true,
                &[],
                false,
            ),
            Role::Operator
        );
    }

    #[test]
    fn uid_zero_administrator_is_opt_in() {
        let mut cfg = auth();
        assert_eq!(
            resolve_role(&id("root", 0, false), &cfg, None, true, &[], false),
            Role::Administrator
        );
        cfg.allow_uid_zero_administrator = false;
        assert_eq!(
            resolve_role(&id("root", 0, false), &cfg, None, true, &[], false),
            Role::User
        );
    }

    #[test]
    fn coordinator_is_below_operator() {
        assert_eq!(
            resolve_role(&id("dave", 1000, false), &auth(), None, true, &[], true),
            Role::Coordinator
        );
    }
}
