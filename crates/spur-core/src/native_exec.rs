// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Sign and verify job/step execution credentials.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use rand::RngExt;
use sha2::{Digest, Sha256};

use crate::native_cred::{
    CredentialError, CredentialKind, ExecutionCredential, NodeResourceSlice, SignedToken,
    CREDENTIAL_ID_LEN, DIGEST_LEN,
};
use crate::native_jwks::{Ed25519SigningKeySet, Ed25519VerifyKeySet};
use crate::native_mint::CLOCK_SKEW_SECS;
use crate::resource::ResourceAllocations;

pub const JOB_CREDENTIAL_TTL_SECS: u64 = 24 * 3600;
pub const STEP_CREDENTIAL_TTL_SECS: u64 = 3600;

pub fn command_digest(script: &str, argv: &[String], container_image: &str) -> [u8; DIGEST_LEN] {
    let mut h = Sha256::new();
    h.update(script.as_bytes());
    h.update([0]);
    for a in argv {
        h.update(a.as_bytes());
        h.update([0]);
    }
    h.update(container_image.as_bytes());
    h.finalize().into()
}

pub fn container_digest(image: &str) -> Option<[u8; DIGEST_LEN]> {
    if image.is_empty() {
        None
    } else {
        let mut h = Sha256::new();
        h.update(image.as_bytes());
        Some(h.finalize().into())
    }
}

/// Flatten a proto allocation into the (cpus, memory, devices) tuple
/// [`ExecutionCredential::require_slice`] expects.
pub fn proto_slice_devices(
    alloc: &Option<spur_proto::proto::ResourceAllocations>,
) -> (u32, u64, Vec<(String, u32, u64)>) {
    let Some(a) = alloc else {
        return (0, 0, Vec::new());
    };
    let mut devices: Vec<(String, u32, u64)> = a
        .devices
        .iter()
        .flat_map(|(name, list)| {
            list.devices
                .iter()
                .map(|d| (name.clone(), d.device_id, d.count))
        })
        .collect();
    devices.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    (a.cpus, a.memory_mb, devices)
}

pub fn slice_from_alloc(node: &str, alloc: &ResourceAllocations) -> NodeResourceSlice {
    let mut devices: Vec<_> = alloc
        .devices
        .iter()
        .flat_map(|(name, list)| {
            list.iter().map(|d| crate::native_cred::DeviceSlice {
                name: name.clone(),
                device_id: d.device_id,
                count: d.count,
            })
        })
        .collect();
    devices.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.device_id.cmp(&b.device_id))
            .then(a.count.cmp(&b.count))
    });
    NodeResourceSlice {
        node: node.to_string(),
        cpus: alloc.cpus,
        memory_mb: alloc.memory_mb,
        devices,
    }
}

pub fn sign_execution(
    mut cred: ExecutionCredential,
    keys: &Ed25519SigningKeySet,
    now: u64,
) -> Result<String, CredentialError> {
    if cred.credential_id == [0u8; CREDENTIAL_ID_LEN] {
        rand::rng().fill(&mut cred.credential_id);
    }
    cred.key_id = keys.default_kid().to_string();
    if cred.issued_at == 0 {
        cred.issued_at = now;
        cred.not_before = now;
    }
    if cred.expires_at == 0 {
        let ttl = if cred.kind == CredentialKind::Step {
            STEP_CREDENTIAL_TTL_SECS
        } else {
            JOB_CREDENTIAL_TTL_SECS
        };
        cred.expires_at = now.saturating_add(ttl);
    }
    let payload = cred.to_signing_bytes()?;
    let (kid, signature) = keys
        .sign(&payload, now)
        .map_err(|_| CredentialError::BadSignature)?;
    SignedToken {
        key_id: kid,
        payload,
        signature,
    }
    .to_base64url()
}

pub fn verify_execution(
    token: &str,
    keys: &Ed25519VerifyKeySet,
    cluster_id: &str,
    now: u64,
) -> Result<ExecutionCredential, CredentialError> {
    let signed = SignedToken::from_base64url(token)?;
    keys.verify(&signed.key_id, &signed.payload, &signed.signature, now)
        .map_err(|_| CredentialError::BadSignature)?;
    let cred = ExecutionCredential::from_signing_bytes(&signed.payload)?;
    if cred.key_id != signed.key_id {
        return Err(CredentialError::Malformed("kid"));
    }
    cred.require_cluster(cluster_id)?;
    cred.validate_time(now, CLOCK_SKEW_SECS)?;
    Ok(cred)
}

#[derive(Clone, PartialEq, Eq)]
struct AcceptedLaunch {
    credential_id: [u8; CREDENTIAL_ID_LEN],
    digest: [u8; DIGEST_LEN],
    cancelled: bool,
}

/// Per-agent record of accepted execution credentials for idempotency.
#[derive(Default)]
pub struct LaunchAcceptance {
    inner: Mutex<HashMap<(u32, u32, u32), AcceptedLaunch>>,
}

impl LaunchAcceptance {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(u32, u32, u32), AcceptedLaunch>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// First presentation records the credential. An exact duplicate is ok.
    /// A different digest or id for the same attempt is rejected. Cancelled
    /// attempts stay rejected until a newer `run_attempt`.
    pub fn accept(&self, cred: &ExecutionCredential) -> Result<bool, CredentialError> {
        let key = (cred.job_id, cred.step_id, cred.run_attempt);
        let mut inner = self.lock();
        match inner.get(&key) {
            None => {
                inner.insert(
                    key,
                    AcceptedLaunch {
                        credential_id: cred.credential_id,
                        digest: cred.command_digest,
                        cancelled: false,
                    },
                );
                Ok(true)
            }
            Some(prev) if prev.cancelled => Err(CredentialError::WrongAttempt),
            Some(prev)
                if prev.credential_id == cred.credential_id
                    && prev.digest == cred.command_digest =>
            {
                Ok(false)
            }
            Some(_) => Err(CredentialError::IdempotencyConflict),
        }
    }

    pub fn cancel(&self, job_id: u32, step_id: u32, run_attempt: u32) {
        let mut inner = self.lock();
        inner
            .entry((job_id, step_id, run_attempt))
            .and_modify(|e| e.cancelled = true)
            .or_insert(AcceptedLaunch {
                credential_id: [0u8; CREDENTIAL_ID_LEN],
                digest: [0u8; DIGEST_LEN],
                cancelled: true,
            });
    }

    pub fn cancel_attempt(&self, job_id: u32, run_attempt: u32) {
        let mut inner = self.lock();
        for ((j, _, a), entry) in inner.iter_mut() {
            if *j == job_id && *a == run_attempt {
                entry.cancelled = true;
            }
        }
    }

    /// Rehydrate an accepted launch from supervisor metadata after agent restart.
    pub fn restore(
        &self,
        job_id: u32,
        step_id: u32,
        run_attempt: u32,
        credential_id: [u8; CREDENTIAL_ID_LEN],
        digest: [u8; DIGEST_LEN],
    ) {
        let mut inner = self.lock();
        inner.insert(
            (job_id, step_id, run_attempt),
            AcceptedLaunch {
                credential_id,
                digest,
                cancelled: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    fn sets() -> (Ed25519SigningKeySet, Ed25519VerifyKeySet) {
        let seed = [13u8; 32];
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let x = crate::native_cred::payload_to_base64url(pair.public_key().as_ref());
        let d = crate::native_cred::payload_to_base64url(&seed);
        let sign = json!({"keys":[{
            "alg":"EdDSA","kty":"OKP","crv":"Ed25519","kid":"e1","use":"default","d":d,"x":x
        }]});
        let verify = json!({"keys":[{
            "alg":"EdDSA","kty":"OKP","crv":"Ed25519","kid":"e1","use":"default","x":x
        }]});
        (
            Ed25519SigningKeySet::from_bytes(sign.to_string().as_bytes(), 1).unwrap(),
            Ed25519VerifyKeySet::from_bytes(verify.to_string().as_bytes(), 1).unwrap(),
        )
    }

    fn cred() -> ExecutionCredential {
        ExecutionCredential {
            kind: CredentialKind::Job,
            cluster_id: "cluster-a".into(),
            key_id: String::new(),
            job_id: 42,
            step_id: 0,
            run_attempt: 1,
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            supplementary_gids: vec![],
            account: "research".into(),
            partition: "gpu".into(),
            qos: String::new(),
            resources_by_node: vec![NodeResourceSlice {
                node: "gpu01".into(),
                cpus: 8,
                memory_mb: 1024,
                devices: vec![],
            }],
            command_digest: [1u8; DIGEST_LEN],
            container_digest: None,
            issued_at: 0,
            not_before: 0,
            expires_at: 0,
            credential_id: [0u8; CREDENTIAL_ID_LEN],
        }
    }

    #[test]
    fn sign_after_claims_verify_on_the_named_node() {
        let (sign, verify) = sets();
        let token = sign_execution(cred(), &sign, 100).unwrap();
        let got = verify_execution(&token, &verify, "cluster-a", 100).unwrap();
        got.require_kind(CredentialKind::Job).unwrap();
        got.require_node("gpu01").unwrap();
        got.require_run_attempt(1).unwrap();
        got.require_unix(1000, 1000).unwrap();
        got.require_slice("gpu01", 8, 1024, &[]).unwrap();
        assert!(got.require_node("gpu02").is_err());
        assert!(got.require_run_attempt(2).is_err());
        assert!(verify_execution(&token, &verify, "other", 100).is_err());
    }

    #[test]
    fn idempotent_duplicate_and_changed_digest() {
        let cache = LaunchAcceptance::new();
        let mut a = cred();
        a.credential_id = [9u8; CREDENTIAL_ID_LEN];
        assert!(cache.accept(&a).unwrap());
        assert!(!cache.accept(&a).unwrap());
        let mut b = a.clone();
        b.command_digest = [2u8; DIGEST_LEN];
        assert!(matches!(
            cache.accept(&b),
            Err(CredentialError::IdempotencyConflict)
        ));
        cache.cancel(42, 0, 1);
        assert!(matches!(
            cache.accept(&a),
            Err(CredentialError::WrongAttempt)
        ));
        let mut next = a;
        next.run_attempt = 2;
        assert!(cache.accept(&next).unwrap());
    }

    #[test]
    fn proto_slice_devices_sorts_and_defaults_empty() {
        assert_eq!(proto_slice_devices(&None), (0, 0, Vec::new()));
        let alloc = spur_proto::proto::ResourceAllocations {
            cpus: 4,
            memory_mb: 512,
            devices: [(
                "gpu".into(),
                spur_proto::proto::DeviceAllocations {
                    devices: vec![
                        spur_proto::proto::AllocatedDevice {
                            device_id: 2,
                            count: 1,
                        },
                        spur_proto::proto::AllocatedDevice {
                            device_id: 1,
                            count: 1,
                        },
                    ],
                },
            )]
            .into(),
        };
        let (cpus, mem, devices) = proto_slice_devices(&Some(alloc));
        assert_eq!(cpus, 4);
        assert_eq!(mem, 512);
        assert_eq!(devices, vec![("gpu".into(), 1, 1), ("gpu".into(), 2, 1)]);
    }
}
