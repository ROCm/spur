// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared naming for the SPUR quota layer.
//!
//! The operator's quota reconciler creates a per-account Namespace and grants each member a
//! per-user ServiceAccount; spurctld mints a scoped kubeconfig into that same namespace/SA. They
//! must agree on the exact names, so the convention lives here in spur-core (both depend on it).

/// Max length of a DNS-1123 label (k8s namespace / ServiceAccount name limit).
const DNS_LABEL_MAX: usize = 63;
const NS_PREFIX: &str = "spur-acct-";
const SA_PREFIX: &str = "spur-user-";

/// Kubernetes namespace for a SPUR account (DNS-1123 label safe, prefix included in the length cap).
pub fn account_namespace(account: &str) -> String {
    prefixed_dns_label(NS_PREFIX, account)
}

/// Per-user ServiceAccount name within the account namespace — what `spur k8s kubeconfig --user`
/// mints a token for and what the account RoleBinding grants.
pub fn user_service_account(user: &str) -> String {
    prefixed_dns_label(SA_PREFIX, user)
}

/// `<prefix><sanitized>` capped at 63 chars total, so the returned name is always a valid DNS-1123
/// label. The sanitized part is truncated to leave room for the prefix (which is itself DNS-safe).
fn prefixed_dns_label(prefix: &str, s: &str) -> String {
    let budget = DNS_LABEL_MAX.saturating_sub(prefix.len());
    format!("{prefix}{}", sanitize_dns_label_capped(s, budget))
}

/// Lower-case, replace any char that isn't `[a-z0-9-]` with `-`, collapse/trim leading-trailing `-`,
/// and cap at 63 chars — a DNS-1123 label. Empty input yields "x" so a name is always valid.
pub fn sanitize_dns_label(s: &str) -> String {
    sanitize_dns_label_capped(s, DNS_LABEL_MAX)
}

fn sanitize_dns_label_capped(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(max).collect();
    let capped = capped.trim_matches('-').to_string();
    if capped.is_empty() {
        "x".to_string()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_safe() {
        assert_eq!(account_namespace("Physics_Lab"), "spur-acct-physics-lab");
        assert_eq!(user_service_account("Alice.Smith"), "spur-user-alice-smith");
        assert_eq!(account_namespace("__weird__"), "spur-acct-weird");
        assert_eq!(sanitize_dns_label(""), "x");
        assert!(sanitize_dns_label(&"a".repeat(200)).len() <= 63);
    }

    #[test]
    fn distinct_names_can_collide_after_sanitization() {
        // Known M1 limitation: the sanitizer is not injective, so account names differing only in
        // non-DNS characters map to the same namespace. Documented here so a change is deliberate.
        assert_eq!(
            account_namespace("physics_lab"),
            account_namespace("physics.lab")
        );
        assert_eq!(
            account_namespace("Physics-Lab"),
            account_namespace("physics-lab")
        );
    }

    #[test]
    fn prefixed_names_stay_within_dns_limit() {
        // The prefix must count toward the 63-char cap, and the result stays a valid label.
        let ns = account_namespace(&"a".repeat(200));
        assert!(ns.len() <= 63, "namespace too long: {} chars", ns.len());
        assert!(ns.starts_with("spur-acct-"));
        assert!(!ns.ends_with('-'));
        let sa = user_service_account(&"b".repeat(200));
        assert!(sa.len() <= 63, "sa too long: {} chars", sa.len());
        assert!(sa.starts_with("spur-user-"));
    }
}
