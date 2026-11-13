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

/// The current leader (0 if none, or if this node's Raft core has stopped) and term.
pub fn leader_and_term() -> (u64, u64) {
    RAFT.get()
        .cloned()
        .flatten()
        .map(|r| (r.current_leader().unwrap_or(0), r.current_term()))
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

/// A step started by someone the owner shared the job with runs as that user.
#[derive(Debug, Clone, Copy)]
pub struct StepRunsAs<'a> {
    pub user: &'a str,
    pub uid: u32,
    pub gid: u32,
}

pub fn mints_step_credentials() -> bool {
    EXEC.get().cloned().flatten().is_some()
}

pub fn sign_step_credential(
    cluster_id: &str,
    job: &spur_core::job::Job,
    step_id: u32,
    command: &[String],
    nodes: &[String],
    container_image: &str,
    runs_as: Option<StepRunsAs<'_>>,
) -> Result<String, spur_core::native_cred::CredentialError> {
    if !mints_step_credentials() {
        return Ok(String::new());
    }
    sign_job_credential(step_credential(
        cluster_id,
        job,
        step_id,
        command,
        nodes,
        container_image,
        runs_as,
    ))
}

fn step_credential(
    cluster_id: &str,
    job: &spur_core::job::Job,
    step_id: u32,
    command: &[String],
    nodes: &[String],
    container_image: &str,
    runs_as: Option<StepRunsAs<'_>>,
) -> ExecutionCredential {
    let mut resources_by_node: Vec<_> = nodes
        .iter()
        .map(|name| {
            let alloc = job.per_node_alloc.get(name).cloned().unwrap_or_default();
            slice_from_alloc(name, &alloc)
        })
        .collect();
    resources_by_node.sort_by(|a, b| a.node.cmp(&b.node));
    ExecutionCredential {
        kind: CredentialKind::Step,
        cluster_id: cluster_id.to_string(),
        key_id: String::new(),
        job_id: job.job_id,
        step_id,
        run_attempt: job.run_attempt,
        user: runs_as.map_or_else(|| job.spec.user.clone(), |r| r.user.to_string()),
        uid: runs_as.map_or(job.spec.uid, |r| r.uid),
        gid: runs_as.map_or(job.spec.gid, |r| r.gid),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> spur_core::job::Job {
        spur_core::job::Job::new(
            7,
            spur_core::job::JobSpec {
                user: "alice".into(),
                uid: 1000,
                gid: 1000,
                account: Some("research".into()),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_step_credential_runs_as_the_owner_unless_a_joiner_started_it() {
        let job = job();
        let nodes = ["n1".to_string()];
        let owner = step_credential("c", &job, 2, &[], &nodes, "", None);
        assert_eq!(
            (owner.user.as_str(), owner.uid, owner.gid),
            ("alice", 1000, 1000)
        );

        let joiner = StepRunsAs {
            user: "bob",
            uid: 1001,
            gid: 2001,
        };
        let joined = step_credential("c", &job, 2, &[], &nodes, "", Some(joiner));
        assert_eq!(
            (joined.user.as_str(), joined.uid, joined.gid),
            ("bob", 1001, 2001)
        );
        assert_eq!(joined.kind, CredentialKind::Step);
        assert_eq!((joined.job_id, joined.step_id), (7, 2));
        assert_eq!(
            joined.account, "research",
            "the job's account still bounds the step"
        );
    }
}
