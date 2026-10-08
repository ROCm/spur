// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller-peer forwarded identity envelopes.
//!
//! A Raft follower authenticates a user RPC once, consumes its nonce, and
//! forwards a signed envelope instead of the original user credential. Only
//! controllers hold the signing key; a node JWT cannot produce one.

use std::sync::Arc;

use rand::RngExt;

use crate::auth::Identity;
use crate::native_cred::{CredentialError, ForwardedIdentity, SignedToken, DIGEST_LEN, NONCE_LEN};
use crate::native_jwks::Ed25519SigningKeySet;
use crate::native_mint::CLOCK_SKEW_SECS;
use crate::native_replay::ReplayCache;

pub const FORWARD_LIFETIME_SECS: u64 = 30;
pub const IDENTITY_HEADER: &str = "x-spur-identity";
pub const FORWARDED_HEADER: &str = "x-spur-forwarded";

/// The method path a request arrived on, carried in extensions so a handler can
/// sign the path it forwards rather than the Rust request type.
#[derive(Debug, Clone)]
pub struct RpcPath(pub String);

/// Body binding from a verified envelope; the handler must call [`Self::require`].
/// `action` is the method path — every `Empty` RPC shares one type and digest.
#[derive(Debug, Clone)]
pub struct ForwardedBinding {
    pub action: String,
    pub request_digest: [u8; DIGEST_LEN],
}

impl ForwardedBinding {
    pub fn require(&self, action: &str, digest: &[u8; DIGEST_LEN]) -> Result<(), CredentialError> {
        if self.action != action {
            return Err(CredentialError::Malformed("action"));
        }
        if &self.request_digest != digest {
            return Err(CredentialError::Malformed("request digest"));
        }
        Ok(())
    }
}

/// Signs and verifies [`ForwardedIdentity`] envelopes for one controller process.
#[derive(Clone)]
pub struct PeerVerifier {
    pub cluster_id: String,
    pub controller_id: u64,
    keys: Arc<Ed25519SigningKeySet>,
    replay: Arc<ReplayCache>,
}

impl PeerVerifier {
    pub fn new(
        cluster_id: impl Into<String>,
        controller_id: u64,
        keys: Arc<Ed25519SigningKeySet>,
    ) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            controller_id,
            keys,
            replay: Arc::new(ReplayCache::new(crate::native_replay::DEFAULT_CAPACITY)),
        }
    }

    pub fn sign(
        &self,
        identity: &Identity,
        dest_leader_id: u64,
        term: u64,
        action: &str,
        request_digest: [u8; DIGEST_LEN],
        now: u64,
    ) -> Result<String, CredentialError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill(&mut nonce);
        let env = ForwardedIdentity {
            cluster_id: self.cluster_id.clone(),
            source_controller_id: self.controller_id,
            dest_leader_id,
            term,
            user: identity.user.clone(),
            uid: identity.uid,
            gid: identity.gid,
            trusted_unix: identity.trusted_unix,
            is_admin: identity.is_admin,
            action: action.to_string(),
            request_digest,
            issued_at: now,
            expires_at: now.saturating_add(FORWARD_LIFETIME_SECS),
            nonce,
            key_id: self.keys.default_kid().to_string(),
        };
        let payload = env.to_signing_bytes()?;
        let (kid, signature) = self
            .keys
            .sign(&payload, now)
            .map_err(|_| CredentialError::BadSignature)?;
        if kid != env.key_id {
            return Err(CredentialError::Malformed("kid"));
        }
        SignedToken {
            key_id: kid,
            payload,
            signature,
        }
        .to_base64url()
    }

    pub fn verify(
        &self,
        token: &str,
        now: u64,
    ) -> Result<(Identity, ForwardedBinding), CredentialError> {
        let signed = SignedToken::from_base64url(token)?;
        self.keys
            .verify(&signed.key_id, &signed.payload, &signed.signature, now)
            .map_err(|_| CredentialError::BadSignature)?;
        let env = ForwardedIdentity::from_signing_bytes(&signed.payload)?;
        if env.key_id != signed.key_id {
            return Err(CredentialError::Malformed("kid"));
        }
        if env.cluster_id != self.cluster_id {
            return Err(CredentialError::ClusterMismatch);
        }
        env.require_destination(self.controller_id)?;
        env.validate_time(now, CLOCK_SKEW_SECS)?;
        self.replay.check_and_insert_raw(
            crate::native_replay::ReplayKey::new(
                env.cluster_id.clone(),
                format!("spur/{}/forward/{}", env.cluster_id, env.dest_leader_id),
                env.term,
                env.key_id.clone(),
                env.nonce,
            ),
            env.expires_at.saturating_add(CLOCK_SKEW_SECS),
            now,
        )?;
        let binding = ForwardedBinding {
            action: env.action,
            request_digest: env.request_digest,
        };
        Ok((
            Identity {
                user: env.user,
                uid: env.uid,
                gid: env.gid,
                is_admin: env.is_admin,
                trusted_unix: env.trusted_unix,
            },
            binding,
        ))
    }
}

pub fn request_digest(bytes: &[u8]) -> [u8; DIGEST_LEN] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    fn keys() -> Arc<Ed25519SigningKeySet> {
        let seed = [7u8; 32];
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let public = pair.public_key();
        let doc = json!({
            "keys": [{
                "alg": "EdDSA",
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": "peer1",
                "use": "default",
                "d": crate::native_cred::payload_to_base64url(&seed),
                "x": crate::native_cred::payload_to_base64url(public.as_ref()),
            }]
        });
        Arc::new(
            Ed25519SigningKeySet::from_bytes(doc.to_string().as_bytes(), 1_700_000_000).unwrap(),
        )
    }

    fn identity() -> Identity {
        Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: true,
            trusted_unix: true,
        }
    }

    #[test]
    fn envelope_round_trip_and_replay() {
        let peer = PeerVerifier::new("cluster-a", 2, keys());
        let digest = [3u8; DIGEST_LEN];
        let now = 1_700_000_000;
        let token = peer
            .sign(&identity(), 2, 9, "SubmitJobRequest", digest, now)
            .unwrap();
        let (got, bind) = peer.verify(&token, now).unwrap();
        assert_eq!(got.user, "alice");
        assert_eq!(got.uid, 1000);
        assert!(got.trusted_unix);
        assert!(got.is_admin);
        bind.require("SubmitJobRequest", &digest).unwrap();
        assert!(peer.verify(&token, now).is_err());
    }

    #[test]
    fn wrong_destination_and_digest_are_rejected() {
        let signer = PeerVerifier::new("cluster-a", 1, keys());
        let digest = [4u8; DIGEST_LEN];
        let now = 1_700_000_000;
        let token = signer
            .sign(&identity(), 2, 1, "CancelJobRequest", digest, now)
            .unwrap();
        let leader = PeerVerifier::new("cluster-a", 2, keys());
        let (_, bind) = leader.verify(&token, now).unwrap();
        bind.require("CancelJobRequest", &digest).unwrap();
        let other = PeerVerifier::new("cluster-a", 3, keys());
        assert!(matches!(
            other.verify(&token, now),
            Err(CredentialError::AudienceMismatch)
        ));
        let leader2 = PeerVerifier::new("cluster-a", 2, keys());
        let token2 = signer
            .sign(&identity(), 2, 1, "CancelJobRequest", digest, now + 1)
            .unwrap();
        let (_, bind2) = leader2.verify(&token2, now + 1).unwrap();
        let wrong = [9u8; DIGEST_LEN];
        assert!(bind2.require("CancelJobRequest", &wrong).is_err());
        assert!(bind2.require("SubmitJobRequest", &digest).is_err());
    }

    #[test]
    fn node_jwt_material_cannot_be_confused_with_an_envelope() {
        let peer = PeerVerifier::new("cluster-a", 1, keys());
        assert!(peer.verify("not-an-envelope", 1_700_000_000).is_err());
    }
}
