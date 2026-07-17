// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared naming for the SPUR quota layer.
//!
//! The operator's quota reconciler creates a per-account Namespace and grants each member a
//! per-user ServiceAccount; spurctld mints a scoped kubeconfig into that same namespace/SA. They
//! must agree on the exact names, so the convention lives here in spur-core (both depend on it).

/// Kubernetes namespace for a SPUR account (DNS-1123 label safe).
pub fn account_namespace(account: &str) -> String {
    format!("spur-acct-{}", sanitize_dns_label(account))
}

/// Per-user ServiceAccount name within the account namespace — what `spur k8s kubeconfig --user`
/// mints a token for and what the account RoleBinding grants.
pub fn user_service_account(user: &str) -> String {
    format!("spur-user-{}", sanitize_dns_label(user))
}

/// Lower-case, replace any char that isn't `[a-z0-9-]` with `-`, collapse/trim leading-trailing `-`,
/// and cap at 63 chars — a DNS-1123 label. Empty input yields "x" so a name is always valid.
pub fn sanitize_dns_label(s: &str) -> String {
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
    let capped: String = trimmed.chars().take(63).collect();
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
}
