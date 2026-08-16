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
//! to handle rotation.

use std::sync::Arc;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Request, Status};

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

fn credential() -> ControllerCredential {
    let Some(key) = SIGNING_KEY.get().filter(|k| !k.is_empty()) else {
        // No key configured: present nothing. Agents in `permissive` still accept the call and log
        // it; agents in `required` refuse, which is the intended outcome for a misconfigured
        // controller — better a clear refusal than silently unauthenticated execution.
        return ControllerCredential::default();
    };
    let header = spur_core::auth::generate_token(
        CONTROLLER_SUBJECT,
        0,
        true,
        key.as_bytes(),
        CREDENTIAL_TTL_SECS,
    )
    .ok()
    .and_then(|t| MetadataValue::try_from(format!("Bearer {t}")).ok());
    ControllerCredential { header }
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
