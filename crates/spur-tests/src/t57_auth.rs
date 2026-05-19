// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T57: Authentication and authorization tests.

#[cfg(test)]
mod tests {
    use spur_core::auth::*;

    const SECRET: &[u8] = b"test-secret-for-jwt-signing-key";

    // ── T57.1: Token generation and verification ─────────────────

    #[test]
    fn t57_1_generate_verify_token() {
        let token = generate_token("alice", 1000, false, SECRET, 3600).unwrap();
        let id = verify_token(&token, SECRET).unwrap();
        assert_eq!(id.user, "alice");
        assert_eq!(id.uid, 1000);
        assert!(!id.is_admin);
    }

    #[test]
    fn t57_2_admin_token() {
        let token = generate_token("root", 0, true, SECRET, 3600).unwrap();
        let id = verify_token(&token, SECRET).unwrap();
        assert!(id.is_admin);
        assert_eq!(id.uid, 0);
    }

    #[test]
    fn t57_3_wrong_secret_rejected() {
        let token = generate_token("alice", 1000, false, SECRET, 3600).unwrap();
        let result = verify_token(&token, b"wrong-secret");
        assert!(result.is_err());
    }

    #[test]
    fn t57_4_expired_token_rejected() {
        // Generate a token that expired 100 seconds ago by manipulating claims directly
        use jsonwebtoken::{encode, EncodingKey, Header};
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = spur_core::auth::TokenClaims {
            sub: "alice".into(),
            uid: 1000,
            exp: now - 100, // 100 seconds in the past
            iat: now - 200,
            admin: false,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap();
        let result = verify_token(&token, SECRET);
        assert!(matches!(result, Err(AuthError::Expired)));
    }

    // ── T57.5: Authorization checks ──────────────────────────────

    #[test]
    fn t57_5_user_can_cancel_own_job() {
        let id = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(id.can_cancel_job("alice").is_ok());
    }

    #[test]
    fn t57_6_user_cannot_cancel_others_job() {
        let id = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        let result = id.can_cancel_job("bob");
        assert!(result.is_err());
        assert!(matches!(result, Err(AuthError::NotJobOwner { .. })));
    }

    #[test]
    fn t57_7_admin_can_cancel_any_job() {
        let id = Identity::admin();
        assert!(id.can_cancel_job("alice").is_ok());
        assert!(id.can_cancel_job("bob").is_ok());
    }

    #[test]
    fn t57_8_user_can_modify_own_job() {
        let id = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(id.can_modify_job("alice").is_ok());
        assert!(id.can_modify_job("bob").is_err());
    }

    #[test]
    fn t57_9_require_admin() {
        let user = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(user.require_admin().is_err());
        assert!(Identity::admin().require_admin().is_ok());
    }

    // ── T57.10: Auth none mode ───────────────────────────────────

    #[test]
    fn t57_10_auth_none_returns_identity() {
        let id = auth_none();
        assert!(!id.user.is_empty());
        assert!(id.uid > 0 || id.user == "root");
    }
}
