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

/// Check that `user` is allowed to perform `action` on a job owned by `owner`.
///
/// Allows the owner, root, or an empty `user` (daemon calls and clients
/// predating the identity field). Jobs with an empty owner are restricted to
/// root only, since they run as root and granting access to any caller would
/// be a privilege escalation.
pub fn check_job_owner(user: &str, owner: &str, action: &str) -> Result<(), AuthError> {
    if user.is_empty() || user == "root" || user == owner {
        return Ok(());
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
    fn test_check_job_owner_allows_owner_root_and_daemon() {
        assert!(check_job_owner("alice", "alice", "exec").is_ok());
        assert!(check_job_owner("root", "alice", "exec").is_ok());
        assert!(check_job_owner("", "alice", "exec").is_ok());
    }

    #[test]
    fn test_check_job_owner_rejects_other_user() {
        let err = check_job_owner("bob", "alice", "exec").expect_err("bob must be denied");
        assert!(matches!(err, AuthError::NotJobOwner { .. }));
        assert_eq!(
            err.to_string(),
            "user bob cannot exec job owned by alice",
            "message names the requester, action, and owner"
        );
    }

    /// Jobs with an empty owner run as root, so only root and daemon (empty
    /// user) are allowed. A regular user must be denied.
    #[test]
    fn test_check_job_owner_empty_owner_restricts_to_root() {
        assert!(check_job_owner("root", "", "exec").is_ok());
        assert!(check_job_owner("", "", "exec").is_ok());
        assert!(
            check_job_owner("alice", "", "exec").is_err(),
            "empty-owner jobs run as root; granting access is a privilege escalation"
        );
    }

    /// A non-empty placeholder owner matches no caller, so it restricts the job
    /// to root. Asserted so that introducing such a placeholder cannot silently
    /// lock users out of their own jobs.
    #[test]
    fn test_check_job_owner_placeholder_owner_restricts_to_root() {
        assert!(check_job_owner("root", "k8s", "exec").is_ok());
        assert!(
            check_job_owner("alice", "k8s", "exec").is_err(),
            "a placeholder owner denies every named user; record the real \
             submitter or leave the owner empty instead"
        );
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
}
