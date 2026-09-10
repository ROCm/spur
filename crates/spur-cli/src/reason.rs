// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Display helpers for node drain/down reason provenance.

/// Render the user who set a node reason. `with_uid` appends the numeric uid in
/// parens (sinfo `%U`); otherwise just the name (sinfo `%u`, scontrol). An unset
/// or unresolvable uid falls back to `Unknown`.
pub fn reason_user(uid: Option<u32>, with_uid: bool) -> String {
    match uid {
        Some(uid) => {
            let name = spur_core::auth::username_for_uid(uid).unwrap_or_else(|| "Unknown".into());
            if with_uid {
                format!("{name}({uid})")
            } else {
                name
            }
        }
        None => "Unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_uid_is_unknown() {
        assert_eq!(reason_user(None, false), "Unknown");
        assert_eq!(reason_user(None, true), "Unknown");
    }

    #[test]
    fn unresolvable_uid_without_id_is_unknown() {
        // u32::MAX has no passwd entry (see auth::username_for_uid tests).
        assert_eq!(reason_user(Some(u32::MAX), false), "Unknown");
    }

    #[test]
    fn unresolvable_uid_with_id_keeps_the_number() {
        assert_eq!(reason_user(Some(u32::MAX), true), "Unknown(4294967295)");
    }
}
