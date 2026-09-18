// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Agent → controller gRPC credentials.
//!
//! Under `[auth] plugin = "spur"`, every controller RPC (register, heartbeat,
//! completion) presents a fresh user-RPC credential from the local mint after
//! an unauthenticated Ping. Without that, `auth.mode = "required"` refuses the
//! call before join/node tokens in the request body are examined.

use std::path::PathBuf;
use std::sync::OnceLock;

use spur_core::native_mint::{mint_blocking, resolve_socket_path};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Request, Status};

pub type AuthChannel = InterceptedService<Channel, AgentControllerInterceptor>;
pub type ControllerClient =
    spur_proto::proto::slurm_controller_client::SlurmControllerClient<AuthChannel>;

#[derive(Clone)]
struct NativeMintParams {
    socket: PathBuf,
    audience: String,
    epoch: u64,
}

#[derive(Clone, Default)]
pub struct AgentControllerInterceptor {
    mint: Option<NativeMintParams>,
}

impl tonic::service::Interceptor for AgentControllerInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let Some(params) = &self.mint else {
            return Ok(request);
        };
        let token = mint_blocking(&params.socket, &params.audience, params.epoch)
            .map_err(|e| Status::unauthenticated(e.to_string()))?;
        let value = MetadataValue::try_from(format!("Bearer {token}"))
            .map_err(|_| Status::unauthenticated("minted credential is not valid metadata"))?;
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }
}

static NATIVE_SOCKET: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn install(plugin: &str, cluster_name: &str) {
    let socket = (plugin == "spur").then(|| {
        resolve_socket_path(cluster_name)
            .unwrap_or_else(|_| PathBuf::from(format!("/run/spur/{cluster_name}/auth.sock")))
    });
    let _ = NATIVE_SOCKET.set(socket);
}

/// Dial the controller and attach a native bearer when the plugin is `spur`.
pub async fn connect(endpoints: &str) -> Result<ControllerClient, ConnectAuthError> {
    let channel = spur_client::connect_channel(endpoints)
        .await
        .map_err(ConnectAuthError::Transport)?;
    wrap(channel).await.map_err(ConnectAuthError::Status)
}

pub async fn wrap(channel: Channel) -> Result<ControllerClient, Status> {
    let interceptor = match NATIVE_SOCKET.get().cloned().flatten() {
        None => AgentControllerInterceptor::default(),
        Some(socket) => {
            let mut raw = spur_proto::controller_client(channel.clone());
            let ping = raw
                .ping(())
                .await
                .map_err(|e| Status::unauthenticated(format!("native auth handshake (Ping): {e}")))?
                .into_inner();
            if ping.auth_audience.is_empty() {
                return Err(Status::unauthenticated(
                    "controller did not advertise a native auth audience; \
                     spurctld must run with [auth] plugin = \"spur\"",
                ));
            }
            AgentControllerInterceptor {
                mint: Some(NativeMintParams {
                    socket,
                    audience: ping.auth_audience,
                    epoch: ping.auth_epoch,
                }),
            }
        }
    };
    Ok(spur_proto::controller_client(InterceptedService::new(
        channel,
        interceptor,
    )))
}

#[derive(Debug)]
pub enum ConnectAuthError {
    Transport(tonic::transport::Error),
    Status(Status),
}

impl std::fmt::Display for ConnectAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Status(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectAuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use nix::unistd::{Uid, User};
    use spur_core::native_jwks::HmacKeySet;
    use spur_core::native_mint::{bind_socket, open_minted, serve, unix_now, CredentialMint};
    use tonic::service::Interceptor;
    use tonic::Code;

    #[test]
    fn jwt_plugin_adds_no_header() {
        let mut i = AgentControllerInterceptor::default();
        let req = i.call(Request::new(())).unwrap();
        assert!(req.metadata().get("authorization").is_none());
    }

    #[test]
    fn native_interceptor_fails_closed_when_the_mint_is_down() {
        let mut i = AgentControllerInterceptor {
            mint: Some(NativeMintParams {
                socket: PathBuf::from("/no/such/auth.sock"),
                audience: "spur/c/controller/h".into(),
                epoch: 1,
            }),
        };
        let err = i.call(Request::new(())).unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
    }

    fn hmac_set() -> HmacKeySet {
        let doc = serde_json::json!({
            "keys": [{
                "alg": "HS256",
                "kty": "oct",
                "kid": "k1",
                "k": "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI",
                "use": "default"
            }]
        });
        HmacKeySet::from_bytes(doc.to_string().as_bytes(), unix_now().unwrap()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_interceptor_mints_a_fresh_credential_per_rpc() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("auth.sock");
        let keys = Arc::new(hmac_set());
        let server = Arc::new(CredentialMint::new("cluster-a", Arc::clone(&keys), 30).unwrap());
        let listener = bind_socket(&sock).await.unwrap();
        tokio::spawn(serve(listener, server));

        let mut i = AgentControllerInterceptor {
            mint: Some(NativeMintParams {
                socket: sock,
                audience: "spur/cluster-a/controller/ctld".into(),
                epoch: 7,
            }),
        };
        let first = i
            .call(Request::new(()))
            .unwrap()
            .metadata()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let second = i
            .call(Request::new(()))
            .unwrap()
            .metadata()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(first, second);
        for header in [&first, &second] {
            let token = header.strip_prefix("Bearer ").unwrap();
            let cred = open_minted(token, &keys, unix_now().unwrap()).unwrap();
            assert_eq!(cred.audience, "spur/cluster-a/controller/ctld");
            assert_eq!(cred.audience_epoch, 7);
            let uid = nix::unistd::getuid().as_raw();
            assert_eq!(cred.uid, uid);
            let name = User::from_uid(Uid::from_raw(uid)).unwrap().unwrap().name;
            assert_eq!(cred.user, name);
        }
    }
}
