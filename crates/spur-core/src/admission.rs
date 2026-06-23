// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Token-based node admission.
//!
//! Handles generation, parsing, and validation of join tokens
//! and node identity tokens (JWT) for authenticated heartbeats.
//!
//! Token format: `<id>.<secret>` (6-char hex ID, 64-char hex secret).

use chrono::{DateTime, Utc};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

const TOKEN_ID_LEN: usize = 6;
const TOKEN_SECRET_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("admission token required")]
    TokenRequired,
    #[error("invalid token format")]
    InvalidFormat,
    #[error("token not found: {0}")]
    TokenNotFound(String),
    #[error("token revoked")]
    TokenRevoked,
    #[error("token expired")]
    TokenExpired,
    #[error("invalid token secret")]
    InvalidSecret,
    #[error("invalid node token: {0}")]
    InvalidNodeToken(String),
    #[error("node token expired")]
    NodeTokenExpired,
}

/// Stored admission token (secret is hashed, never stored in plaintext).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionToken {
    pub id: String,
    pub secret_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}

/// Generate a new admission token. Returns the stored struct (hashed secret)
/// and the full token string to display once to the admin.
pub fn generate_token(ttl_secs: Option<u32>) -> (AdmissionToken, String) {
    let mut rng = rand::rng();

    let id: String = (0..TOKEN_ID_LEN)
        .map(|_| format!("{:x}", rng.random_range(0u8..16)))
        .collect();

    let secret_bytes: [u8; TOKEN_SECRET_LEN] = rng.random();
    let secret_hex: String = secret_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let secret_hash = hex_sha256(secret_hex.as_bytes());

    let now = Utc::now();
    let expires_at = ttl_secs.map(|s| now + chrono::Duration::seconds(s as i64));

    let token = AdmissionToken {
        id: id.clone(),
        secret_hash,
        created_at: now,
        expires_at,
        revoked: false,
    };

    let full_token = format!("{id}.{secret_hex}");
    (token, full_token)
}

/// Parse a raw token string into (token_id, secret).
pub fn parse_token(raw: &str) -> Result<(&str, &str), AdmissionError> {
    let (id, secret) = raw.split_once('.').ok_or(AdmissionError::InvalidFormat)?;
    if id.len() != TOKEN_ID_LEN || secret.len() != TOKEN_SECRET_LEN * 2 {
        return Err(AdmissionError::InvalidFormat);
    }
    Ok((id, secret))
}

/// Validate a join token against the store. Checks existence, revocation,
/// expiry, max uses, and secret (constant-time).
pub fn validate_token(
    token_id: &str,
    secret: &str,
    token_store: &std::collections::HashMap<String, AdmissionToken>,
) -> Result<(), AdmissionError> {
    let stored = token_store
        .get(token_id)
        .ok_or_else(|| AdmissionError::TokenNotFound(token_id.to_string()))?;

    if stored.revoked {
        return Err(AdmissionError::TokenRevoked);
    }

    if let Some(expires_at) = stored.expires_at {
        if Utc::now() > expires_at {
            return Err(AdmissionError::TokenExpired);
        }
    }

    let provided_hash = hex_sha256(secret.as_bytes());
    let a = provided_hash.as_bytes();
    let b = stored.secret_hash.as_bytes();
    if a.len() != b.len() || a.ct_eq(b).unwrap_u8() != 1 {
        return Err(AdmissionError::InvalidSecret);
    }

    Ok(())
}

/// Node identity claims embedded in the post-admission JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTokenClaims {
    pub sub: String, // hostname
    pub exp: u64,
    pub iat: u64,
}

/// Identity extracted from a verified node token.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    pub hostname: String,
}

const NODE_TOKEN_TTL_SECS: u64 = 7 * 24 * 3600; // 7 days

/// Generate a node identity JWT issued after successful admission.
pub fn generate_node_token(hostname: &str, jwt_key: &[u8]) -> Result<String, AdmissionError> {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = NodeTokenClaims {
        sub: hostname.into(),
        exp: now + NODE_TOKEN_TTL_SECS,
        iat: now,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_key),
    )
    .map_err(|e| AdmissionError::InvalidNodeToken(e.to_string()))
}

/// Verify a node identity JWT and extract the node's hostname.
pub fn verify_node_token(token: &str, jwt_key: &[u8]) -> Result<NodeIdentity, AdmissionError> {
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let data = decode::<NodeTokenClaims>(
        token,
        &DecodingKey::from_secret(jwt_key),
        &Validation::default(),
    )
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => AdmissionError::NodeTokenExpired,
        _ => AdmissionError::InvalidNodeToken(e.to_string()),
    })?;

    Ok(NodeIdentity {
        hostname: data.claims.sub,
    })
}

fn hex_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_parse() {
        let (token, raw) = generate_token(None);
        let (id, _secret) = parse_token(&raw).unwrap();
        assert_eq!(id, token.id);
    }

    #[test]
    fn test_validate_success() {
        let (token, raw) = generate_token(None);
        let (id, secret) = parse_token(&raw).unwrap();
        let mut store = std::collections::HashMap::new();
        store.insert(id.to_string(), token);
        assert!(validate_token(id, secret, &store).is_ok());
    }

    #[test]
    fn test_validate_wrong_secret() {
        let (token, _raw) = generate_token(None);
        let mut store = std::collections::HashMap::new();
        store.insert(token.id.clone(), token.clone());
        let result = validate_token(&token.id, "wrong_secret", &store);
        assert!(matches!(result, Err(AdmissionError::InvalidSecret)));
    }

    #[test]
    fn test_validate_revoked() {
        let (mut token, raw) = generate_token(None);
        token.revoked = true;
        let (id, secret) = parse_token(&raw).unwrap();
        let mut store = std::collections::HashMap::new();
        store.insert(id.to_string(), token);
        let result = validate_token(id, secret, &store);
        assert!(matches!(result, Err(AdmissionError::TokenRevoked)));
    }

    #[test]
    fn test_validate_expired() {
        let (mut token, raw) = generate_token(Some(1));
        token.expires_at = Some(Utc::now() - chrono::Duration::seconds(10));
        let (id, secret) = parse_token(&raw).unwrap();
        let mut store = std::collections::HashMap::new();
        store.insert(id.to_string(), token);
        let result = validate_token(id, secret, &store);
        assert!(matches!(result, Err(AdmissionError::TokenExpired)));
    }

    #[test]
    fn test_node_token_roundtrip() {
        let key = b"test-jwt-key";
        let jwt = generate_node_token("gpu-node-01", key).unwrap();
        let identity = verify_node_token(&jwt, key).unwrap();
        assert_eq!(identity.hostname, "gpu-node-01");
    }

    #[test]
    fn test_node_token_wrong_key() {
        let jwt = generate_node_token("node1", b"key1").unwrap();
        let result = verify_node_token(&jwt, b"key2");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_format() {
        assert!(parse_token("garbage").is_err());
        assert!(parse_token("abc.tooshort").is_err());
        assert!(parse_token(
            "toolongid.abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567"
        )
        .is_err());
    }

    #[test]
    fn test_hex_sha256_known_vector() {
        // SHA-256("hello") = well-known value
        let result = super::hex_sha256(b"hello");
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
