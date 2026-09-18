// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller-to-agent service credentials and controller-issued node identity.
//!
//! These use the controller Ed25519 key sets. They do not fall back to
//! `auth.jwks` or execution-credential keys. Agents hold only verification
//! material and cannot mint either token.

use std::sync::Arc;

use rand::RngExt;

use crate::auth::{Identity, CONTROLLER_SUBJECT};
use crate::native_cred::{
    ControllerRpcCredential, CredentialError, NodeIdentityCredential, SignedToken, NONCE_LEN,
};
use crate::native_jwks::{Ed25519SigningKeySet, Ed25519VerifyKeySet};
use crate::native_mint::CLOCK_SKEW_SECS;

pub const CONTROLLER_RPC_TTL_SECS: u64 = 300;
pub const NODE_IDENTITY_TTL_SECS: u64 = 7 * 24 * 3600;
pub const CONTROLLER_TO_AGENT_AUDIENCE: &str = "controller-to-agent";

#[derive(Clone)]
pub struct ControllerServiceSigner {
    pub cluster_id: String,
    keys: Arc<Ed25519SigningKeySet>,
}

impl ControllerServiceSigner {
    pub fn new(cluster_id: impl Into<String>, keys: Arc<Ed25519SigningKeySet>) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            keys,
        }
    }

    pub fn mint(&self, now: u64) -> Result<String, CredentialError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill(&mut nonce);
        let cred = ControllerRpcCredential {
            cluster_id: self.cluster_id.clone(),
            audience: CONTROLLER_TO_AGENT_AUDIENCE.into(),
            issued_at: now,
            expires_at: now.saturating_add(CONTROLLER_RPC_TTL_SECS),
            nonce,
            key_id: self.keys.default_kid().to_string(),
        };
        sign_ed25519(&cred.to_signing_bytes()?, &self.keys, now)
    }
}

pub fn verify_controller_rpc(
    token: &str,
    keys: &Ed25519VerifyKeySet,
    cluster_id: &str,
    now: u64,
    replay: &crate::native_replay::ReplayCache,
) -> Result<Identity, CredentialError> {
    let signed = SignedToken::from_base64url(token)?;
    keys.verify(&signed.key_id, &signed.payload, &signed.signature, now)
        .map_err(|_| CredentialError::BadSignature)?;
    let cred = ControllerRpcCredential::from_signing_bytes(&signed.payload)?;
    if cred.key_id != signed.key_id {
        return Err(CredentialError::Malformed("kid"));
    }
    if cred.cluster_id != cluster_id {
        return Err(CredentialError::ClusterMismatch);
    }
    if cred.audience != CONTROLLER_TO_AGENT_AUDIENCE {
        return Err(CredentialError::AudienceMismatch);
    }
    cred.validate_time(now, CLOCK_SKEW_SECS)?;
    replay.check_and_insert_raw(
        crate::native_replay::ReplayKey::new(
            cred.cluster_id.clone(),
            cred.audience.clone(),
            0,
            cred.key_id.clone(),
            cred.nonce,
        ),
        cred.expires_at.saturating_add(CLOCK_SKEW_SECS),
        now,
    )?;
    Ok(Identity::posix(CONTROLLER_SUBJECT, 0, 0, true))
}

#[derive(Clone)]
pub struct NodeIdentitySigner {
    pub cluster_id: String,
    keys: Arc<Ed25519SigningKeySet>,
}

impl NodeIdentitySigner {
    pub fn new(cluster_id: impl Into<String>, keys: Arc<Ed25519SigningKeySet>) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            keys,
        }
    }

    pub fn mint(&self, hostname: &str, now: u64) -> Result<String, CredentialError> {
        if hostname.is_empty() {
            return Err(CredentialError::Malformed("hostname"));
        }
        let cred = NodeIdentityCredential {
            cluster_id: self.cluster_id.clone(),
            hostname: hostname.to_string(),
            audience: format!("spur/{}/controller", self.cluster_id),
            issued_at: now,
            expires_at: now.saturating_add(NODE_IDENTITY_TTL_SECS),
            key_id: self.keys.default_kid().to_string(),
        };
        sign_ed25519(&cred.to_signing_bytes()?, &self.keys, now)
    }

    pub fn verify(
        &self,
        token: &str,
        expected_host: &str,
        now: u64,
    ) -> Result<NodeIdentityCredential, CredentialError> {
        verify_node_identity(token, &self.keys, &self.cluster_id, expected_host, now)
    }
}

pub fn verify_node_identity(
    token: &str,
    keys: &Ed25519SigningKeySet,
    cluster_id: &str,
    expected_host: &str,
    now: u64,
) -> Result<NodeIdentityCredential, CredentialError> {
    let signed = SignedToken::from_base64url(token)?;
    keys.verify(&signed.key_id, &signed.payload, &signed.signature, now)
        .map_err(|_| CredentialError::BadSignature)?;
    let cred = NodeIdentityCredential::from_signing_bytes(&signed.payload)?;
    if cred.key_id != signed.key_id {
        return Err(CredentialError::Malformed("kid"));
    }
    if cred.cluster_id != cluster_id {
        return Err(CredentialError::ClusterMismatch);
    }
    if cred.hostname != expected_host {
        return Err(CredentialError::AudienceMismatch);
    }
    cred.validate_time(now, CLOCK_SKEW_SECS)?;
    Ok(cred)
}

fn sign_ed25519(
    payload: &[u8],
    keys: &Ed25519SigningKeySet,
    now: u64,
) -> Result<String, CredentialError> {
    let (kid, signature) = keys
        .sign(payload, now)
        .map_err(|_| CredentialError::BadSignature)?;
    SignedToken {
        key_id: kid,
        payload: payload.to_vec(),
        signature,
    }
    .to_base64url()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    fn sign_set() -> Arc<Ed25519SigningKeySet> {
        let seed = [11u8; 32];
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let doc = json!({
            "keys": [{
                "alg": "EdDSA",
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": "c1",
                "use": "default",
                "d": crate::native_cred::payload_to_base64url(&seed),
                "x": crate::native_cred::payload_to_base64url(pair.public_key().as_ref()),
            }]
        });
        Arc::new(Ed25519SigningKeySet::from_bytes(doc.to_string().as_bytes(), 10).unwrap())
    }

    fn verify_set(sign: &Ed25519SigningKeySet) -> Ed25519VerifyKeySet {
        let seed = [11u8; 32];
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let doc = json!({
            "keys": [{
                "alg": "EdDSA",
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": sign.default_kid(),
                "use": "default",
                "x": crate::native_cred::payload_to_base64url(pair.public_key().as_ref()),
            }]
        });
        Ed25519VerifyKeySet::from_bytes(doc.to_string().as_bytes(), 10).unwrap()
    }

    #[test]
    fn controller_rpc_verifies_as_controller_not_as_user() {
        let keys = sign_set();
        let signer = ControllerServiceSigner::new("cluster-a", Arc::clone(&keys));
        let token = signer.mint(100).unwrap();
        let replay = crate::native_replay::ReplayCache::new(16);
        let id =
            verify_controller_rpc(&token, &verify_set(&keys), "cluster-a", 100, &replay).unwrap();
        assert!(id.is_controller());
        assert_eq!(id.user, CONTROLLER_SUBJECT);
        assert!(
            verify_controller_rpc(&token, &verify_set(&keys), "cluster-a", 100, &replay).is_err()
        );
        let replay2 = crate::native_replay::ReplayCache::new(16);
        assert!(verify_controller_rpc(&token, &verify_set(&keys), "other", 100, &replay2).is_err());
    }

    #[test]
    fn node_identity_is_bound_to_hostname_and_cluster() {
        let keys = sign_set();
        let signer = NodeIdentitySigner::new("cluster-a", keys);
        let token = signer.mint("gpu01", 100).unwrap();
        let cred = signer.verify(&token, "gpu01", 100).unwrap();
        assert_eq!(cred.hostname, "gpu01");
        assert!(signer.verify(&token, "gpu02", 100).is_err());
    }

    #[test]
    fn hmac_user_token_is_not_a_controller_or_node_credential() {
        let keys = sign_set();
        let replay = crate::native_replay::ReplayCache::new(8);
        assert!(
            verify_controller_rpc("aaaa", &verify_set(&keys), "cluster-a", 1, &replay).is_err()
        );
        let signer = NodeIdentitySigner::new("cluster-a", keys);
        assert!(signer.verify("bbbb", "gpu01", 1).is_err());
    }
}
