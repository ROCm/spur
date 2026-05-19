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
