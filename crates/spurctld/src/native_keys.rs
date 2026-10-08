// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process-wide native signing keys for forwarding, node identity, execution
//! credentials, and controller-to-agent identity.

use std::sync::{Arc, OnceLock};

use crate::raft::RaftHandle;
use spur_core::native_cred::{CredentialKind, ExecutionCredential, CREDENTIAL_ID_LEN};
use spur_core::native_exec::{command_digest, sign_execution, slice_from_alloc};
use spur_core::native_jwks::{
    path_from_env_or, Ed25519SigningKeySet, CONTROLLER_SIGNING_JWKS_PATH, CRED_SIGNING_JWKS_PATH,
    NODE_SIGNING_JWKS_PATH,
};
use spur_core::native_peer::PeerVerifier;
use spur_core::native_service::{ControllerServiceSigner, NodeIdentitySigner};
use spur_core::resource::ResourceAllocations;

static PEER: OnceLock<Option<Arc<PeerVerifier>>> = OnceLock::new();
static EXEC: OnceLock<Option<Arc<Ed25519SigningKeySet>>> = OnceLock::new();
static NODE: OnceLock<Option<Arc<NodeIdentitySigner>>> = OnceLock::new();
static CONTROLLER_RPC: OnceLock<Option<Arc<ControllerServiceSigner>>> = OnceLock::new();
static RAFT: OnceLock<Option<Arc<RaftHandle>>> = OnceLock::new();

pub fn install(
    cluster_id: &str,
    raft: Arc<RaftHandle>,
    plugin: &str,
) -> Result<(), spur_core::native_jwks::JwksError> {
    let controller_id = raft.node_id;
    let _ = RAFT.set(Some(Arc::clone(&raft)));
    if plugin != "spur" {
        let _ = PEER.set(None);
        let _ = EXEC.set(None);
        let _ = NODE.set(None);
        let _ = CONTROLLER_RPC.set(None);
        return Ok(());
    }
    let now = spur_core::native_mint::unix_now().unwrap_or(0);
    let peer_keys = Arc::new(Ed25519SigningKeySet::from_path(
        &path_from_env_or("SPUR_CONTROLLER_SIGNING_JWKS", CONTROLLER_SIGNING_JWKS_PATH),
        now,
    )?);
    let _ = PEER.set(Some(Arc::new(PeerVerifier::new(
        cluster_id,
        controller_id,
        Arc::clone(&peer_keys),
    ))));
    let _ = CONTROLLER_RPC.set(Some(Arc::new(ControllerServiceSigner::new(
        cluster_id, peer_keys,
    ))));
    let _ = EXEC.set(Some(Arc::new(Ed25519SigningKeySet::from_path(
        &path_from_env_or("SPUR_CRED_SIGNING_JWKS", CRED_SIGNING_JWKS_PATH),
        now,
    )?)));
    let _ = NODE.set(Some(Arc::new(NodeIdentitySigner::new(
        cluster_id,
        Arc::new(Ed25519SigningKeySet::from_path(
            &path_from_env_or("SPUR_NODE_SIGNING_JWKS", NODE_SIGNING_JWKS_PATH),
            now,
        )?),
    ))));
    Ok(())
}

pub fn peer() -> Option<Arc<PeerVerifier>> {
    PEER.get().cloned().flatten()
}

pub fn leader_and_term() -> (u64, u64) {
    RAFT.get()
        .cloned()
        .flatten()
        .map(|r| {
            let m = r.raft.metrics().borrow().clone();
            (m.current_leader.unwrap_or(r.node_id), m.current_term)
        })
        .unwrap_or((0, 0))
}

pub fn node_signer() -> Option<Arc<NodeIdentitySigner>> {
    NODE.get().cloned().flatten()
}

pub fn controller_rpc_signer() -> Option<Arc<ControllerServiceSigner>> {
    CONTROLLER_RPC.get().cloned().flatten()
}

pub fn sign_job_credential(
    cred: ExecutionCredential,
) -> Result<String, spur_core::native_cred::CredentialError> {
    let keys = EXEC.get().cloned().flatten().ok_or(
        spur_core::native_cred::CredentialError::UnknownKeyId("cred-signing".into()),
    )?;
    let now = spur_core::native_mint::unix_now().unwrap_or(0);
    sign_execution(cred, keys.as_ref(), now)
}

/// Empty when the jwt plugin is in use (no execution signing keys).
pub fn sign_dispatch_credential(
    cluster_id: &str,
    job_id: u32,
    run_attempt: u32,
    spec: &spur_core::job::JobSpec,
    per_node_allocs: &std::collections::HashMap<String, ResourceAllocations>,
    nodes: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<String, spur_core::native_cred::CredentialError> {
    if EXEC.get().cloned().flatten().is_none() {
        return Ok(String::new());
    }
    let mut resources_by_node: Vec<_> = nodes
        .into_iter()
        .map(|n| {
            let name = n.as_ref();
            let alloc = per_node_allocs.get(name).cloned().unwrap_or_default();
            slice_from_alloc(name, &alloc)
        })
        .collect();
    resources_by_node.sort_by(|a, b| a.node.cmp(&b.node));
    sign_job_credential(ExecutionCredential {
        kind: CredentialKind::Job,
        cluster_id: cluster_id.to_string(),
        key_id: String::new(),
        job_id,
        step_id: spur_core::step::STEP_BATCH,
        run_attempt,
        user: spec.user.clone(),
        uid: spec.uid,
        gid: spec.gid,
        supplementary_gids: Vec::new(),
        account: spec.account.clone().unwrap_or_default(),
        partition: spec.partition.clone().unwrap_or_default(),
        qos: spec.qos.clone().unwrap_or_default(),
        resources_by_node,
        command_digest: command_digest(
            spec.script.as_deref().unwrap_or(""),
            &spec.argv,
            spec.container_image.as_deref().unwrap_or(""),
        ),
        container_digest: spur_core::native_exec::container_digest(
            spec.container_image.as_deref().unwrap_or(""),
        ),
        issued_at: 0,
        not_before: 0,
        expires_at: 0,
        credential_id: [0u8; CREDENTIAL_ID_LEN],
    })
}

pub fn sign_step_credential(
    cluster_id: &str,
    job: &spur_core::job::Job,
    step_id: u32,
    command: &[String],
    nodes: &[String],
    container_image: &str,
) -> Result<String, spur_core::native_cred::CredentialError> {
    if EXEC.get().cloned().flatten().is_none() {
        return Ok(String::new());
    }
    let mut resources_by_node: Vec<_> = nodes
        .iter()
        .map(|name| {
            let alloc = job.per_node_alloc.get(name).cloned().unwrap_or_default();
            slice_from_alloc(name, &alloc)
        })
        .collect();
    resources_by_node.sort_by(|a, b| a.node.cmp(&b.node));
    sign_job_credential(ExecutionCredential {
        kind: CredentialKind::Step,
        cluster_id: cluster_id.to_string(),
        key_id: String::new(),
        job_id: job.job_id,
        step_id,
        run_attempt: job.run_attempt,
        user: job.spec.user.clone(),
        uid: job.spec.uid,
        gid: job.spec.gid,
        supplementary_gids: Vec::new(),
        account: job.spec.account.clone().unwrap_or_default(),
        partition: job.spec.partition.clone().unwrap_or_default(),
        qos: job.spec.qos.clone().unwrap_or_default(),
        resources_by_node,
        command_digest: command_digest("", command, container_image),
        container_digest: spur_core::native_exec::container_digest(container_image),
        issued_at: 0,
        not_before: 0,
        expires_at: 0,
        credential_id: [0u8; CREDENTIAL_ID_LEN],
    })
}
