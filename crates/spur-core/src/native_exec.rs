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
) -> (u32, u64, Vec<(String, u64, u64)>) {
    let Some(a) = alloc else {
        return (0, 0, Vec::new());
    };
    let mut devices: Vec<(String, u64, u64)> = a
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

/// Enforced per-node budget: the controller's allocation wins, the spec is
/// the fallback (every task on this node, and `--mem-per-cpu` when `--mem`
/// is unset). Shared by spurd's cgroup sizing and spur-k8s's pod resource
/// sizing (which uses only the CPU half).
pub fn resolve_cgroup_budget(
    alloc: Option<&spur_proto::proto::ResourceAllocations>,
    spec: &spur_proto::proto::JobSpec,
    tasks_per_node: u32,
) -> (u32, u64) {
    let (alloc_cpus, alloc_mem_mb) = alloc.map(|a| (a.cpus, a.memory_mb)).unwrap_or((0, 0));
    let gpus_here = alloc
        .and_then(|a| a.devices.get("gpu"))
        .map_or(0, |gpus| gpus.devices.len() as u32);
    // Saturating: the spec is caller-supplied, and a wrapped product would set
    // limits from a budget bearing no relation to what the job asked for.
    let cpus = if alloc_cpus > 0 {
        alloc_cpus
    } else {
        let by_task = tasks_per_node
            .max(1)
            .saturating_mul(spec.cpus_per_task.max(1));
        let by_gpu = spec.cpus_per_gpu.unwrap_or(0).saturating_mul(gpus_here);
        by_task.max(by_gpu)
    };
    let memory_mb = if alloc_mem_mb > 0 {
        alloc_mem_mb
    } else if spec.memory_per_node_mb > 0 {
        spec.memory_per_node_mb
    } else if spec.memory_per_gpu_mb > 0 {
        spec.memory_per_gpu_mb.saturating_mul(u64::from(gpus_here))
    } else {
        spec.memory_per_cpu_mb.saturating_mul(cpus as u64)
    };
    (cpus, memory_mb)
}

/// The node-wide memory ceiling a job with no budget of its own gets: the
/// controller's `node_memory_ceiling_mb` when it sent one, else the agent's own
/// detected total memory (0 if that is unreadable too, which leaves the job
/// unbounded exactly as it already is). Shared by LaunchJob and
/// RegisterJobAllocation so the fallback is identical on both paths.
pub fn resolve_memory_ceiling(node_memory_ceiling_mb: u64, detected_memory_mb: u64) -> u64 {
    if node_memory_ceiling_mb > 0 {
        node_memory_ceiling_mb
    } else {
        detected_memory_mb
    }
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
    if kid != cred.key_id {
        return Err(CredentialError::Malformed("kid"));
    }
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
    retain_until: u64,
}

const DEFAULT_ACCEPTANCE_CAPACITY: usize = 8_192;

struct AcceptanceInner {
    entries: HashMap<(u32, u32, u32), AcceptedLaunch>,
    capacity: usize,
}

impl AcceptanceInner {
    fn evict_expired(&mut self, now: u64) {
        self.entries.retain(|_, e| e.retain_until > now);
    }

    fn evict_earliest(&mut self) {
        let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.retain_until)
            .map(|(k, _)| *k)
        else {
            return;
        };
        self.entries.remove(&victim);
    }

    fn insert(&mut self, key: (u32, u32, u32), entry: AcceptedLaunch) {
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict_earliest();
        }
        self.entries.insert(key, entry);
    }
}

/// Per-agent record of accepted execution credentials for idempotency.
pub struct LaunchAcceptance {
    inner: Mutex<AcceptanceInner>,
}

impl Default for LaunchAcceptance {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_ACCEPTANCE_CAPACITY)
    }
}

impl LaunchAcceptance {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(AcceptanceInner {
                entries: HashMap::new(),
                capacity: capacity.max(1),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, AcceptanceInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now() -> u64 {
        crate::native_mint::unix_now().unwrap_or(0)
    }

    fn retain_until(cred: &ExecutionCredential, now: u64) -> u64 {
        let exp = if cred.expires_at == 0 {
            now.saturating_add(JOB_CREDENTIAL_TTL_SECS)
        } else {
            cred.expires_at
        };
        exp.saturating_add(CLOCK_SKEW_SECS)
    }

    /// First presentation records the credential. An exact duplicate is ok.
    /// A different digest or id for the same attempt is rejected. Cancelled
    /// attempts stay rejected until a newer `run_attempt`.
    pub fn accept(&self, cred: &ExecutionCredential) -> Result<bool, CredentialError> {
        let key = (cred.job_id, cred.step_id, cred.run_attempt);
        let now = Self::now();
        let mut inner = self.lock();
        inner.evict_expired(now);
        match inner.entries.get(&key) {
            None => {
                inner.insert(
                    key,
                    AcceptedLaunch {
                        credential_id: cred.credential_id,
                        digest: cred.command_digest,
                        cancelled: false,
                        retain_until: Self::retain_until(cred, now),
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
        let now = Self::now();
        let mut inner = self.lock();
        inner.evict_expired(now);
        let key = (job_id, step_id, run_attempt);
        if let Some(e) = inner.entries.get_mut(&key) {
            e.cancelled = true;
            return;
        }
        inner.insert(
            key,
            AcceptedLaunch {
                credential_id: [0u8; CREDENTIAL_ID_LEN],
                digest: [0u8; DIGEST_LEN],
                cancelled: true,
                retain_until: now.saturating_add(JOB_CREDENTIAL_TTL_SECS + CLOCK_SKEW_SECS),
            },
        );
    }

    /// A cancel tombstone is recorded with or without execution credentials.
    pub fn is_cancelled(&self, job_id: u32, step_id: u32, run_attempt: u32) -> bool {
        self.lock()
            .entries
            .get(&(job_id, step_id, run_attempt))
            .is_some_and(|e| e.cancelled)
    }

    pub fn cancel_attempt(&self, job_id: u32, run_attempt: u32) {
        let now = Self::now();
        let mut inner = self.lock();
        inner.evict_expired(now);
        for ((j, _, a), entry) in inner.entries.iter_mut() {
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
        let now = Self::now();
        let mut inner = self.lock();
        inner.evict_expired(now);
        inner.insert(
            (job_id, step_id, run_attempt),
            AcceptedLaunch {
                credential_id,
                digest,
                cancelled: false,
                retain_until: now.saturating_add(JOB_CREDENTIAL_TTL_SECS + CLOCK_SKEW_SECS),
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
    fn is_cancelled_reflects_only_the_cancelled_tombstone() {
        let cache = LaunchAcceptance::new();
        cache.cancel(1, 2, 0);
        assert!(cache.is_cancelled(1, 2, 0));
        assert!(!cache.is_cancelled(1, 3, 0));
    }

    #[test]
    fn acceptance_capacity_evicts_the_soonest_expiry() {
        let now = crate::native_mint::unix_now().unwrap();
        let cache = LaunchAcceptance::with_capacity(2);
        let mut a = cred();
        a.job_id = 1;
        a.expires_at = now + 10;
        a.credential_id = [1u8; CREDENTIAL_ID_LEN];
        let mut b = a.clone();
        b.job_id = 2;
        b.expires_at = now + 50;
        b.credential_id = [2u8; CREDENTIAL_ID_LEN];
        let mut c = a.clone();
        c.job_id = 3;
        c.expires_at = now + 80;
        c.credential_id = [3u8; CREDENTIAL_ID_LEN];
        assert!(cache.accept(&a).unwrap());
        assert!(cache.accept(&b).unwrap());
        assert!(cache.accept(&c).unwrap());
        assert!(cache.accept(&a).unwrap());
    }

    #[test]
    fn proto_slice_devices_sorts_and_defaults_empty() {
        assert_eq!(proto_slice_devices(&None), (0, 0, Vec::new()));
        let alloc = spur_proto::proto::ResourceAllocations {
            fabric_binding: None,
            gpu_binding: None,
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
            generation: 0,
            reserved_cpus: 0,
        };
        let (cpus, mem, devices) = proto_slice_devices(&Some(alloc));
        assert_eq!(cpus, 4);
        assert_eq!(mem, 512);
        assert_eq!(devices, vec![("gpu".into(), 1, 1), ("gpu".into(), 2, 1)]);
    }

    fn job_spec(cpus_per_task: u32, mem_node: u64, mem_per_cpu: u64) -> spur_proto::proto::JobSpec {
        spur_proto::proto::JobSpec {
            cpus_per_task,
            memory_per_node_mb: mem_node,
            memory_per_cpu_mb: mem_per_cpu,
            ..Default::default()
        }
    }

    fn alloc_with_gpus(
        cpus: u32,
        memory_mb: u64,
        gpu_count: usize,
    ) -> spur_proto::proto::ResourceAllocations {
        let devices = (0..gpu_count as u64)
            .map(|id| spur_proto::proto::AllocatedDevice {
                device_id: id,
                count: 1,
            })
            .collect();
        spur_proto::proto::ResourceAllocations {
            cpus,
            memory_mb,
            devices: [(
                "gpu".to_string(),
                spur_proto::proto::DeviceAllocations { devices },
            )]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_cgroup_budget_allocation_wins_when_present() {
        // Spec asks 2 cpus / 1024 MB per task; the node allocation grants 8/32G.
        let alloc = alloc_with_gpus(8, 32_768, 0);
        assert_eq!(
            resolve_cgroup_budget(Some(&alloc), &job_spec(2, 1024, 0), 4),
            (8, 32_768)
        );
    }

    #[test]
    fn resolve_cgroup_budget_spec_fallback_counts_every_task_on_the_node() {
        // The undercount: 4 tasks x 2 cpus is 8 cores, not 2.
        assert_eq!(
            resolve_cgroup_budget(None, &job_spec(2, 1024, 0), 4),
            (8, 1024)
        );
    }

    #[test]
    fn resolve_cgroup_budget_spec_fallback_honors_mem_per_cpu() {
        // --mem-per-cpu=512 with 8 cores on this node.
        assert_eq!(
            resolve_cgroup_budget(None, &job_spec(2, 0, 512), 4),
            (8, 4096)
        );
    }

    #[test]
    fn resolve_cgroup_budget_saturates_instead_of_wrapping() {
        // A spec reaches the agent unvalidated, and a wrapped product would
        // hand limits_for a budget unrelated to the request.
        let (cpus, memory_mb) = resolve_cgroup_budget(None, &job_spec(100_000, 0, 0), 100_000);
        assert_eq!(cpus, u32::MAX);
        assert_eq!(memory_mb, 0, "no memory request stays unbounded");

        let (cpus, memory_mb) = resolve_cgroup_budget(None, &job_spec(u32::MAX, 0, u64::MAX), 2);
        assert_eq!(cpus, u32::MAX);
        assert_eq!(memory_mb, u64::MAX);
    }

    #[test]
    fn resolve_cgroup_budget_floors_cpus_at_one() {
        assert_eq!(resolve_cgroup_budget(None, &job_spec(0, 0, 0), 0), (1, 0));
    }

    #[test]
    fn resolve_cgroup_budget_sizes_per_gpu_options_by_this_nodes_gpus() {
        let mut per_gpu = job_spec(1, 0, 0);
        per_gpu.cpus_per_gpu = Some(8);
        per_gpu.memory_per_gpu_mb = 1000;
        let alloc = alloc_with_gpus(0, 0, 2);
        assert_eq!(resolve_cgroup_budget(Some(&alloc), &per_gpu, 1), (16, 2000));
    }

    #[test]
    fn resolve_cgroup_budget_for_a_nic_only_job_ignores_cpus_per_gpu() {
        let mut per_gpu = job_spec(1, 0, 0);
        per_gpu.cpus_per_gpu = Some(8);
        per_gpu.memory_per_gpu_mb = 1000;
        let mut alloc = alloc_with_gpus(0, 0, 2);
        let nics = alloc.devices.remove("gpu").unwrap();
        alloc.devices.insert("vnic".into(), nics);
        assert_eq!(resolve_cgroup_budget(Some(&alloc), &per_gpu, 1), (1, 0));
    }

    #[test]
    fn resolve_memory_ceiling_prefers_the_controllers_value() {
        assert_eq!(resolve_memory_ceiling(16_384, 8192), 16_384);
    }

    #[test]
    fn resolve_memory_ceiling_falls_back_to_detected_memory_for_an_older_controller() {
        assert_eq!(resolve_memory_ceiling(0, 8192), 8192);
    }

    #[test]
    fn resolve_memory_ceiling_is_zero_when_neither_is_known() {
        assert_eq!(resolve_memory_ceiling(0, 0), 0);
    }
}
