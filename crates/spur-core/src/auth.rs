// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Authentication and authorization.
//!
//! Supports JWT token verification for gRPC and REST APIs.
//! Auth mode configured via SlurmConfig.auth.plugin: "jwt", "none".

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authentication required")]
    NotAuthenticated,
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("token expired")]
    Expired,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("user {user} cannot {action} job owned by {owner}")]
    NotJobOwner {
        user: String,
        owner: String,
        action: String,
    },
    #[error("no such user on this host: {0}")]
    UnknownUser(String),
}

/// Subject under which the controller signs the credentials it presents to node agents.
///
/// An agent that verifies a credential carrying this subject knows the caller is the control plane,
/// not an end user, and gates controller-only RPCs on it. Kept here so the controller (which mints
/// the credential) and the agent (which checks it) cannot drift apart on the value.
pub const CONTROLLER_SUBJECT: &str = "spurctld";

/// Resolve a username to its UNIX credentials through NSS.
///
/// The controller derives uid/gid from the *authenticated* username rather than accepting them from
/// the wire: `TokenClaims` carries no gid at all, and a client-supplied uid is what allowed a job to
/// run as an arbitrary user (see the `allow_root_jobs` guard in spurd). Fails closed — an
/// unresolvable user is an error, never a fallback to uid 0.
pub fn resolve_unix_credentials(user: &str) -> Result<(u32, u32), AuthError> {
    if user.is_empty() {
        return Err(AuthError::UnknownUser("<empty>".into()));
    }
    match nix::unistd::User::from_name(user) {
        Ok(Some(u)) => Ok((u.uid.as_raw(), u.gid.as_raw())),
        Ok(None) => Err(AuthError::UnknownUser(user.to_string())),
        Err(e) => Err(AuthError::UnknownUser(format!("{user}: {e}"))),
    }
}

/// Authenticated identity extracted from a token or peer credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub is_admin: bool,
}

impl Identity {
    /// Create an admin identity (for internal daemon-to-daemon calls).
    pub fn admin() -> Self {
        Self {
            user: "root".into(),
            uid: 0,
            gid: 0,
            is_admin: true,
        }
    }

    /// Check if this identity can cancel a job owned by `owner`.
    pub fn can_cancel_job(&self, owner: &str) -> Result<(), AuthError> {
        if self.is_admin || self.user == owner {
            Ok(())
        } else {
            Err(AuthError::NotJobOwner {
                user: self.user.clone(),
                owner: owner.into(),
                action: "cancel".into(),
            })
        }
    }

    /// Check if this identity can modify a job owned by `owner`.
    pub fn can_modify_job(&self, owner: &str) -> Result<(), AuthError> {
        if self.is_admin || self.user == owner {
            Ok(())
        } else {
            Err(AuthError::NotJobOwner {
                user: self.user.clone(),
                owner: owner.into(),
                action: "modify".into(),
            })
        }
    }

    /// Whether this identity is the cluster controller (its credential's subject).
    ///
    /// Node agents use this to gate controller-only RPCs: a job launch or cancel must arrive from
    /// the control plane, which allocates and accounts for it, not straight from a user's token.
    pub fn is_controller(&self) -> bool {
        self.user == CONTROLLER_SUBJECT
    }

    /// Check if this identity can perform admin operations.
    pub fn require_admin(&self) -> Result<(), AuthError> {
        if self.is_admin {
            Ok(())
        } else {
            Err(AuthError::PermissionDenied(format!(
                "user {} is not an admin",
                self.user
            )))
        }
    }
}

/// Check that a caller is allowed to perform `action` on a job owned by `owner`.
///
/// Access is granted to the job's owner and to an explicitly identified internal/daemon caller
/// (`is_internal` — the controller, or a verified admin). There is deliberately no bypass for an
/// empty `user` or a literal `"root"` string: an internal caller must be named by `is_internal`,
/// which the caller derives from a *verified* identity and never infers from a wire-supplied string
/// an attacker can set. An empty `user` therefore matches no owner and is denied unless
/// `is_internal`, so a job that runs as root (empty owner) stays reachable only by internal callers.
pub fn check_job_owner(
    user: &str,
    is_internal: bool,
    owner: &str,
    action: &str,
) -> Result<(), AuthError> {
    if is_internal || (!user.is_empty() && user == owner) {
        return Ok(());
    }
    Err(AuthError::NotJobOwner {
        user: user.into(),
        owner: owner.into(),
        action: action.into(),
    })
}

/// Ownership gate for user-initiated RPCs that may carry a Unix uid (e.g. `RunStep`).
///
/// When a verified identity is present, only that subject (or an admin/internal caller) may
/// act on the job — a matching uid alone cannot bypass a mismatched JWT. When unauthenticated
/// (`auth.mode = permissive` without a credential), the owner username or a matching `caller_uid`
/// against the job's submit-time uid is accepted (Slurm Munge-like same-session semantics).
pub fn check_job_caller(
    user: &str,
    caller_uid: Option<u32>,
    is_internal: bool,
    owner: &str,
    owner_uid: u32,
    identity: Option<&Identity>,
    action: &str,
) -> Result<(), AuthError> {
    if is_internal {
        return Ok(());
    }
    if let Some(id) = identity {
        if id.is_admin || id.user == owner {
            return Ok(());
        }
        return Err(AuthError::NotJobOwner {
            user: id.user.clone(),
            owner: owner.into(),
            action: action.into(),
        });
    }
    if !user.is_empty() && user == owner {
        return Ok(());
    }
    // Proto defaults and RPCs without a uid field (keepalive) send 0; treat that as
    // absent rather than a root-caller match.
    if let Some(uid) = caller_uid.filter(|&u| u != 0) {
        if uid == owner_uid {
            return Ok(());
        }
    }
    Err(AuthError::NotJobOwner {
        user: user.into(),
        owner: owner.into(),
        action: action.into(),
    })
}

/// JWT token claims.
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Subject (username).
    pub sub: String,
    /// User ID.
    pub uid: u32,
    /// Expiration (unix timestamp).
    pub exp: u64,
    /// Issued at (unix timestamp).
    pub iat: u64,
    /// Admin flag.
    #[serde(default)]
    pub admin: bool,
}

/// Generate a JWT token for a user.
pub fn generate_token(
    user: &str,
    uid: u32,
    is_admin: bool,
    secret: &[u8],
    ttl_secs: u64,
) -> Result<String, AuthError> {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = TokenClaims {
        sub: user.into(),
        uid,
        exp: now + ttl_secs,
        iat: now,
        admin: is_admin,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .map_err(|e| AuthError::InvalidToken(e.to_string()))
}

/// Verify a JWT token and return the identity.
pub fn verify_token(token: &str, secret: &[u8]) -> Result<Identity, AuthError> {
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let data = decode::<TokenClaims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
        _ => AuthError::InvalidToken(e.to_string()),
    })?;

    Ok(Identity {
        user: data.claims.sub,
        uid: data.claims.uid,
        gid: 0,
        is_admin: data.claims.admin,
    })
}

/// What to do with one request's `Authorization` header.
///
/// Shared by both daemons so the controller and the agent cannot drift apart on a security
/// decision: they wrap this in their own Tower layer, but the ruling itself lives here.
#[derive(Debug)]
pub enum BearerOutcome {
    /// Verified; carry this identity to the handler.
    Authenticated(Box<Identity>),
    /// No credential presented, and the mode tolerates that.
    Anonymous,
    /// Refuse the request with this message.
    Reject(String),
}

/// Rule the `Authorization` header against the configured mode.
///
/// Deliberate properties:
/// * an INVALID credential is rejected in every mode that verifies — `permissive` tolerates the
///   *absence* of a credential, never a bad one, or forging would beat sending none;
/// * a malformed header is rejected rather than silently downgraded to anonymous;
/// * `disabled` ignores even a valid token, so it cannot be quietly stricter than it claims.
pub fn authenticate_bearer(
    mode: crate::config::AuthMode,
    jwt_key: &[u8],
    header: Option<&str>,
    missing_credential_hint: &str,
) -> BearerOutcome {
    use crate::config::AuthMode;

    let token = match header {
        Some(h) => match h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
        {
            Some(t) if !t.trim().is_empty() => t.trim(),
            _ => {
                return BearerOutcome::Reject(
                    "malformed authorization header: expected 'Bearer <token>'".into(),
                )
            }
        },
        None => {
            return match mode {
                AuthMode::Required => BearerOutcome::Reject(format!(
                    "authentication required: {missing_credential_hint}"
                )),
                _ => BearerOutcome::Anonymous,
            }
        }
    };

    if mode == AuthMode::Disabled {
        return BearerOutcome::Anonymous;
    }
    if jwt_key.is_empty() {
        return BearerOutcome::Reject(
            "a token was presented but no auth.jwt_key is configured".into(),
        );
    }
    match verify_token(token, jwt_key) {
        Ok(identity) => BearerOutcome::Authenticated(Box::new(identity)),
        Err(e) => BearerOutcome::Reject(format!("invalid credential: {e}")),
    }
}

/// "none" auth — always returns an identity based on UNIX user.
pub fn auth_none() -> Identity {
    Identity {
        user: whoami::username().unwrap_or_else(|_| "unknown".into()),
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        is_admin: nix::unistd::getuid().as_raw() == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &[u8] = b"test-secret-key-for-jwt";

    #[test]
    fn test_generate_and_verify() {
        let token = generate_token("alice", 1000, false, TEST_SECRET, 3600).unwrap();
        let id = verify_token(&token, TEST_SECRET).unwrap();
        assert_eq!(id.user, "alice");
        assert_eq!(id.uid, 1000);
        assert!(!id.is_admin);
    }

    #[test]
    fn test_admin_token() {
        let token = generate_token("root", 0, true, TEST_SECRET, 3600).unwrap();
        let id = verify_token(&token, TEST_SECRET).unwrap();
        assert!(id.is_admin);
    }

    #[test]
    fn test_wrong_secret() {
        let token = generate_token("alice", 1000, false, TEST_SECRET, 3600).unwrap();
        let result = verify_token(&token, b"wrong-secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_can_cancel_own_job() {
        let id = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(id.can_cancel_job("alice").is_ok());
        assert!(id.can_cancel_job("bob").is_err());
    }

    #[test]
    fn test_admin_can_cancel_any() {
        let id = Identity::admin();
        assert!(id.can_cancel_job("alice").is_ok());
        assert!(id.can_cancel_job("bob").is_ok());
    }

    #[test]
    fn test_check_job_owner_allows_owner_and_internal() {
        // The owner reaches their own job; an explicitly internal caller reaches any job.
        assert!(check_job_owner("alice", false, "alice", "exec").is_ok());
        assert!(check_job_owner("", true, "alice", "exec").is_ok());
        assert!(check_job_owner("spurctld", true, "alice", "exec").is_ok());
    }

    /// The empty-user and literal-"root" bypasses are gone: only `is_internal` grants a non-owner,
    /// and it is never inferred from the (attacker-controllable) `user` string.
    #[test]
    fn test_check_job_owner_no_empty_or_root_string_bypass() {
        assert!(
            check_job_owner("", false, "alice", "exec").is_err(),
            "an empty user must not be treated as a daemon caller"
        );
        assert!(
            check_job_owner("root", false, "alice", "exec").is_err(),
            "a literal \"root\" username must not bypass the ownership check"
        );
    }

    #[test]
    fn test_check_job_owner_rejects_other_user() {
        let err = check_job_owner("bob", false, "alice", "exec").expect_err("bob must be denied");
        assert!(matches!(err, AuthError::NotJobOwner { .. }));
        assert_eq!(
            err.to_string(),
            "user bob cannot exec job owned by alice",
            "message names the requester, action, and owner"
        );
    }

    /// Jobs with an empty owner run as root, so only an internal caller is allowed — a named user is
    /// denied, and an empty user no longer slips through as a daemon.
    #[test]
    fn test_check_job_owner_empty_owner_restricts_to_internal() {
        assert!(check_job_owner("", true, "", "exec").is_ok());
        assert!(
            check_job_owner("", false, "", "exec").is_err(),
            "an empty non-internal caller must not match an empty owner"
        );
        assert!(
            check_job_owner("alice", false, "", "exec").is_err(),
            "empty-owner jobs run as root; granting access is a privilege escalation"
        );
    }

    /// A non-empty placeholder owner matches no caller, so it restricts the job
    /// to internal callers. Asserted so that introducing such a placeholder
    /// cannot silently lock users out of their own jobs.
    #[test]
    fn test_check_job_owner_placeholder_owner_restricts_to_internal() {
        assert!(check_job_owner("", true, "k8s", "exec").is_ok());
        assert!(
            check_job_owner("alice", false, "k8s", "exec").is_err(),
            "a placeholder owner denies every named user; record the real \
             submitter or leave the owner empty instead"
        );
    }

    #[test]
    fn check_job_caller_uid_fallback_when_unauthenticated() {
        assert!(check_job_caller(
            "localname",
            Some(1000),
            false,
            "jwt-subject",
            1000,
            None,
            "run a step in"
        )
        .is_ok());
    }

    #[test]
    fn check_job_caller_jwt_subject_must_match_owner() {
        let id = Identity {
            user: "jwt-subject".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(check_job_caller(
            "jwt-subject",
            Some(1000),
            false,
            "jwt-subject",
            1000,
            Some(&id),
            "run a step in"
        )
        .is_ok());
        let other = Identity {
            user: "other-jwt-user".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(
            check_job_caller(
                "localname",
                Some(1000),
                false,
                "jwt-subject",
                1000,
                Some(&other),
                "run a step in"
            )
            .is_err(),
            "uid alone must not bypass a JWT for a different owner"
        );
    }

    #[test]
    fn check_job_caller_rejects_mismatched_unauthenticated_user_and_uid() {
        assert!(
            check_job_caller("bob", Some(2000), false, "alice", 1000, None, "attach to").is_err()
        );
    }

    #[test]
    fn check_job_caller_uid_zero_does_not_bypass_username_check() {
        assert!(
            check_job_caller("bob", Some(0), false, "alice", 0, None, "run a step in").is_err()
        );
    }

    #[test]
    fn test_is_controller_only_matches_the_controller_subject() {
        let controller = Identity {
            user: CONTROLLER_SUBJECT.into(),
            uid: 0,
            gid: 0,
            is_admin: true,
        };
        assert!(controller.is_controller());
        let user = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(!user.is_controller());
    }

    #[test]
    fn test_require_admin() {
        let user = Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        };
        assert!(user.require_admin().is_err());
        assert!(Identity::admin().require_admin().is_ok());
    }

    // --- authenticate_bearer ---
    //
    // The function is the shared ruling used by both the controller and the agent. Testing it
    // directly (not just through the middleware wrappers) ensures the contract holds at the source
    // so neither daemon can silently diverge.

    fn bearer(key: &[u8]) -> String {
        format!(
            "Bearer {}",
            generate_token("alice", 1000, false, key, 3600).unwrap()
        )
    }

    #[test]
    fn required_rejects_missing_credential() {
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Required, TEST_SECRET, None, "hint"),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn permissive_allows_missing_credential() {
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Permissive,
                TEST_SECRET,
                None,
                "hint"
            ),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn disabled_allows_missing_credential() {
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Disabled, TEST_SECRET, None, "hint"),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn valid_token_is_authenticated_in_required_mode() {
        let h = bearer(TEST_SECRET);
        match authenticate_bearer(
            crate::config::AuthMode::Required,
            TEST_SECRET,
            Some(&h),
            "hint",
        ) {
            BearerOutcome::Authenticated(id) => {
                assert_eq!(id.user, "alice");
                assert_eq!(id.uid, 1000);
            }
            other => panic!("expected Authenticated, got {other:?}"),
        }
    }

    #[test]
    fn forged_token_rejected_in_permissive_mode() {
        // permissive tolerates absence of a credential, never a bad one.
        let forged = bearer(b"attacker-key");
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Permissive,
                TEST_SECRET,
                Some(&forged),
                "hint"
            ),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn malformed_header_always_rejected() {
        for bad in &["token-without-bearer-prefix", "Bearer ", "bearer", ""] {
            assert!(
                matches!(
                    authenticate_bearer(
                        crate::config::AuthMode::Permissive,
                        TEST_SECRET,
                        Some(bad),
                        "hint"
                    ),
                    BearerOutcome::Reject(_)
                ),
                "header {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn disabled_ignores_a_valid_token() {
        // disabled must not silently verify — that would make `disabled` secretly stricter.
        let h = bearer(TEST_SECRET);
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Disabled,
                TEST_SECRET,
                Some(&h),
                "hint"
            ),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn token_presented_but_no_key_configured_is_rejected() {
        let h = bearer(TEST_SECRET);
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Required, b"", Some(&h), "hint"),
            BearerOutcome::Reject(_)
        ));
    }
}
