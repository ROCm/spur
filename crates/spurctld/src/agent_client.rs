// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Agent connections that carry the controller's credential.
//!
//! The agent authenticates its callers (see spurd's `auth_middleware`), so every controller → agent
//! call goes through here rather than `SlurmAgentClient::connect` directly. That keeps a new call
//! site from silently becoming an unauthenticated one.
//!
//! The credential is a short-lived token signed with the cluster's `[auth] jwt_key` — the same key
//! the node-admission path already uses — so no new secret is introduced. It is minted per
//! connection rather than cached: the TTL is short, connections are not hot, and a cache would have
//! to handle rotation. This design assumes connections are short-lived (one RPC per connect); a
//! long-lived connection would hold a credential that expires mid-session and get rejected on the
//! next call without an obvious reason.

use std::sync::Arc;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Request, Status};
use tracing::error;

use spur_proto::proto::slurm_agent_client::SlurmAgentClient;

/// How long a controller credential is valid. Short because it is minted per connection; long
/// enough to tolerate clock skew between the controller and a node.
const CREDENTIAL_TTL_SECS: u64 = 300;

/// Subject the agent sees for controller-issued credentials.
const CONTROLLER_SUBJECT: &str = "spurctld";

/// An agent channel that presents the controller's credential.
pub type AgentChannel = InterceptedService<Channel, ControllerCredential>;

#[derive(Clone, Default)]
pub struct ControllerCredential {
    header: Option<MetadataValue<tonic::metadata::Ascii>>,
}

impl tonic::service::Interceptor for ControllerCredential {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(value) = &self.header {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        Ok(request)
    }
}

/// The controller's signing key, set once at startup.
///
/// Held globally because agent connections are made from many places (the scheduler loop, the k0s
/// reconciler, PMIx dispatch) that have no natural path to thread config through, and it is a single
/// process-wide fact rather than per-call state.
static SIGNING_KEY: std::sync::OnceLock<Arc<String>> = std::sync::OnceLock::new();

/// Install the signing key. Called once from `main`; later calls are ignored.
pub fn set_signing_key(key: String) {
    let _ = SIGNING_KEY.set(Arc::new(key));
}

/// Build a credential from an explicit key string. Extracted from `credential()` for testability —
/// the global `OnceLock` cannot be reset between tests.
fn credential_with_key(key: &str) -> ControllerCredential {
    if key.is_empty() {
        // No key configured: present nothing. Agents in `permissive` still accept the call and log
        // it; agents in `required` refuse, which is the intended outcome for a misconfigured
        // controller — better a clear refusal than silently unauthenticated execution.
        return ControllerCredential::default();
    }
    let header = match spur_core::auth::generate_token(
        CONTROLLER_SUBJECT,
        0,
        true,
        key.as_bytes(),
        CREDENTIAL_TTL_SECS,
    ) {
        Ok(t) => MetadataValue::try_from(format!("Bearer {t}")).ok(),
        Err(e) => {
            // Token minting failed despite the key being present. This should not happen in
            // practice (HMAC-SHA256 signing has no failure mode for a non-empty key), but if it
            // does the controller will silently become unauthenticated, and agents in `required`
            // mode will refuse every launch. Logging here makes that diagnosable.
            error!(
                "failed to mint controller credential for agent calls: {e}; \
                 connections will carry no credential — agents in `required` mode will refuse them"
            );
            None
        }
    };
    ControllerCredential { header }
}

fn credential() -> ControllerCredential {
    let key = SIGNING_KEY
        .get()
        .filter(|k| !k.is_empty())
        .map(|k| k.as_str().to_string())
        .unwrap_or_default();
    credential_with_key(&key)
}

/// Connect to an agent, presenting the controller's credential.
///
/// Same signature shape as `SlurmAgentClient::connect`, so call sites only change which function
/// they call.
pub async fn connect(
    endpoint: String,
) -> Result<SlurmAgentClient<AgentChannel>, tonic::transport::Error> {
    let channel = tonic::transport::Endpoint::from_shared(endpoint)?
        .connect()
        .await?;
    Ok(SlurmAgentClient::new(InterceptedService::new(
        channel,
        credential(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::auth::{generate_token, verify_token};

    const TEST_KEY: &str = "test-cluster-key";

    #[test]
    fn empty_key_produces_no_credential() {
        let cred = credential_with_key("");
        assert!(
            cred.header.is_none(),
            "an empty key must not produce a credential"
        );
    }

    #[test]
    fn valid_key_produces_a_verifiable_credential() {
        let cred = credential_with_key(TEST_KEY);
        let header = cred.header.expect("a non-empty key must produce a credential");
        let header_str = header.to_str().expect("header must be valid ASCII");
        let token = header_str
            .strip_prefix("Bearer ")
            .expect("header must be 'Bearer <token>'");
        let identity = verify_token(token, TEST_KEY.as_bytes())
            .expect("token must be verifiable with the same key");
        assert_eq!(identity.user, CONTROLLER_SUBJECT);
        assert!(identity.is_admin);
    }

    #[test]
    fn credential_signed_with_wrong_key_is_rejected_by_verifier() {
        let cred = credential_with_key(TEST_KEY);
        let header = cred.header.unwrap();
        let token = header
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap();
        assert!(
            verify_token(token, b"attacker-key").is_err(),
            "a credential signed with one key must not verify against a different key"
        );
    }

    #[test]
    fn credential_token_subject_identifies_the_controller() {
        // The agent does not enforce the subject today, but the field is there so a future
        // per-principal policy can distinguish controller calls from user calls.
        let cred = credential_with_key(TEST_KEY);
        let token = cred
            .header
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_string();
        // generate_token for comparison — confirm our subject constant matches what we mint
        let reference = generate_token(CONTROLLER_SUBJECT, 0, true, TEST_KEY.as_bytes(), 300)
            .unwrap();
        // Both tokens are signed with the same key and subject; verify both decode consistently.
        let id1 = verify_token(&token, TEST_KEY.as_bytes()).unwrap();
        let id2 = verify_token(&reference, TEST_KEY.as_bytes()).unwrap();
        assert_eq!(id1.user, id2.user);
        assert_eq!(id1.is_admin, id2.is_admin);
    }
}
