// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller channel that attaches the caller's credential.
//!
//! Every CLI subcommand connects through [`connect`]. JWT plugins attach a cached
//! bearer for the channel lifetime. Native `plugin = "spur"` mints a fresh
//! audience-bound credential on every RPC — tonic interceptors are synchronous,
//! so that mint is blocking.

use std::path::PathBuf;
use std::time::Duration;

use spur_core::native_mint::{mint_blocking, resolve_socket_path, MintError};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Request, Status};

/// Environment variable holding a JWT credential, checked before the on-disk token.
pub(crate) const TOKEN_ENV: &str = "SPUR_AUTH_TOKEN";

/// Override `[auth] plugin` without rewriting the config file (tests and login-host debugging).
const PLUGIN_ENV: &str = "SPUR_AUTH_PLUGIN";

/// Cluster name when `plugin = "spur"` and no config file is loaded.
const CLUSTER_ENV: &str = "SPUR_CLUSTER_NAME";

/// Credential file, relative to the user's home directory.
const TOKEN_FILE: &str = ".spur/token";

/// A channel that attaches the caller's credential to every request.
pub type AuthChannel = InterceptedService<Channel, AuthInterceptor>;

#[derive(Clone, Default)]
enum CredAttach {
    #[default]
    None,
    Static(MetadataValue<tonic::metadata::Ascii>),
    Native(NativeMintParams),
    /// Native plugin on a channel that never Pinged; minting with epoch 0 would fail closed later.
    NativeNeedsPing,
}

#[derive(Clone)]
struct NativeMintParams {
    socket: PathBuf,
    audience: String,
    epoch: u64,
    timeout: Duration,
}

#[derive(Clone, Default)]
pub struct AuthInterceptor {
    attach: CredAttach,
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        match &self.attach {
            CredAttach::None => {}
            CredAttach::Static(value) => {
                request
                    .metadata_mut()
                    .insert("authorization", value.clone());
            }
            CredAttach::Native(params) => {
                let token = mint_blocking(
                    &params.socket,
                    &params.audience,
                    params.epoch,
                    params.timeout,
                )
                .map_err(|e| Status::unauthenticated(mint_failure_message(&e)))?;
                let value = MetadataValue::try_from(format!("Bearer {token}")).map_err(|_| {
                    Status::unauthenticated("minted credential is not valid metadata")
                })?;
                request.metadata_mut().insert("authorization", value);
            }
            CredAttach::NativeNeedsPing => {
                return Err(Status::unauthenticated(
                    "native plugin requires Ping handshake (wrap_after_controller_ping / wrap_after_agent_ping)",
                ));
            }
        }
        Ok(request)
    }
}

/// Inside a job container a missing mint socket is the normal state when the job
/// did not ask for it, so say that; elsewhere the mint itself is down.
fn mint_failure_message(error: &MintError) -> String {
    let in_container = std::env::var_os(spur_core::native_mint::IN_CONTAINER_ENV).is_some();
    mint_failure_text(error, in_container)
}

fn mint_failure_text(error: &MintError, in_container: bool) -> String {
    match (error, in_container) {
        (MintError::Unavailable { path }, true) => format!(
            "credential mint not available in this container ({path}): the node's auth socket \
             is mounted only when the job was submitted with --container-auth-socket, and if it \
             was, check that spurauthd is running on the node"
        ),
        (MintError::Unavailable { path }, false) => format!(
            "credential mint not available ({path}): start spurauthd or set SPUR_AUTH_SOCKET"
        ),
        (other, _) => other.to_string(),
    }
}

/// Refuse at submission a `--container-mounts` source that would bind the node's
/// mint socket into a container that was not granted it. The check is lexical and
/// uses this host's socket path, so the agent keeps its own check against the
/// resolved paths on the compute node as a backstop.
pub fn refuse_ungranted_socket_mounts(mounts: &[String], granted: bool) -> anyhow::Result<()> {
    if granted || mounts.is_empty() {
        return Ok(());
    }
    let cluster = native_cluster_name().unwrap_or_default();
    let Ok(socket) = resolve_socket_path(&cluster) else {
        return Ok(());
    };
    for m in mounts {
        let source = m.split(':').next().unwrap_or_default();
        if spur_core::native_mint::mount_source_exposes_socket(source, &socket) {
            anyhow::bail!(
                "--container-mounts entry '{m}' would expose the credential socket {}, which \
                 --container-auth-socket makes available; drop the mount or pass the flag",
                socket.display()
            );
        }
    }
    Ok(())
}

/// Read the caller's JWT: `$SPUR_AUTH_TOKEN`, else `~/.spur/token`.
///
/// Unused when `[auth] plugin = "spur"`: that path mints from the local socket
/// and must not reuse a long-lived bearer. A token file with group/other
/// permissions is ignored with a warning rather than used — a bearer credential
/// readable by other users on a shared login node is not a credential.
pub fn load_token() -> Option<String> {
    if let Ok(t) = std::env::var(TOKEN_ENV) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path: PathBuf = dirs_home()?.join(TOKEN_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o077;
        if mode != 0 {
            eprintln!(
                "warning: ignoring {} because it is readable by other users (chmod 600 it)",
                path.display()
            );
            return None;
        }
    }
    let t = std::fs::read_to_string(&path).ok()?.trim().to_string();
    (!t.is_empty()).then_some(t)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Effective `[auth] plugin`: `$SPUR_AUTH_PLUGIN`, else the loaded config (jwt default).
pub fn auth_plugin() -> String {
    if let Ok(p) = std::env::var(PLUGIN_ENV) {
        let p = p.trim();
        if !p.is_empty() {
            return p.to_string();
        }
    }
    crate::spur_config::load_spur_config().auth.plugin
}

pub fn plugin_is_spur() -> bool {
    auth_plugin() == "spur"
}

fn interceptor(_audience: &str) -> AuthInterceptor {
    if plugin_is_spur() {
        return AuthInterceptor {
            attach: CredAttach::NativeNeedsPing,
        };
    }
    let header = load_token().and_then(|t| MetadataValue::try_from(format!("Bearer {t}")).ok());
    AuthInterceptor {
        attach: header.map(CredAttach::Static).unwrap_or(CredAttach::None),
    }
}

fn native_cluster_name() -> anyhow::Result<String> {
    let path_str = std::env::var("SPUR_CONF").unwrap_or_else(|_| "/etc/spur/spur.conf".to_string());
    if let Ok(cfg) = spur_core::config::SlurmConfig::load_from_file(std::path::Path::new(&path_str))
    {
        return Ok(cfg.cluster_name);
    }
    std::env::var(CLUSTER_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "SPUR_CLUSTER_NAME is required when SPUR_AUTH_PLUGIN=spur and no config file is loaded"
            )
        })
}

/// The CLI's own `[spurd]` channel/mint timeouts, read from the same config file
/// `auth_plugin()` uses — so a cluster that tunes these for spurd gets the same
/// values here instead of a constant hardcoded independently of that config.
fn spurd_config() -> spur_core::config::SpurdConfig {
    crate::spur_config::load_spur_config().spurd
}

fn native_interceptor(audience: &str, epoch: u64) -> anyhow::Result<AuthInterceptor> {
    let cluster = native_cluster_name()?;
    let socket = resolve_socket_path(&cluster)
        .unwrap_or_else(|_| PathBuf::from(format!("/run/spur/{cluster}/auth.sock")));
    Ok(AuthInterceptor {
        attach: CredAttach::Native(NativeMintParams {
            socket,
            audience: audience.to_string(),
            epoch,
            timeout: spurd_config().native_mint_timeout(),
        }),
    })
}

/// Wrap an already-established channel, binding JWT credentials if present.
///
/// Native `plugin = "spur"` cannot mint here: the audience and boot epoch come
/// from Ping. Use [`wrap_after_controller_ping`] or [`wrap_after_agent_ping`].
pub fn wrap_with_audience(channel: Channel, audience: &str) -> AuthChannel {
    InterceptedService::new(channel, interceptor(audience))
}

/// Connect to the controller, attaching the caller's credential if one is available.
///
/// Native `plugin = "spur"` first calls unauthenticated Ping to learn the
/// verifier's audience and boot epoch, then mints against those values.
pub async fn connect(endpoints: &str) -> anyhow::Result<AuthChannel> {
    let cfg = spurd_config();
    let timeouts = spur_client::ChannelTimeouts {
        connect: cfg.controller_connect_timeout(),
        keep_alive_interval: cfg.controller_keepalive_interval(),
        keep_alive_timeout: cfg.controller_keepalive_timeout(),
    };
    let (channel, _) =
        spur_client::connect_channel_tracked_with_timeouts(endpoints, timeouts).await?;
    if plugin_is_spur() {
        return wrap_after_controller_ping(channel).await;
    }
    Ok(InterceptedService::new(channel, interceptor(endpoints)))
}

pub async fn wrap_after_controller_ping(channel: Channel) -> anyhow::Result<AuthChannel> {
    let mut client =
        spur_proto::proto::slurm_controller_client::SlurmControllerClient::new(channel.clone());
    let ping = client
        .ping(())
        .await
        .map_err(|e| anyhow::anyhow!("native auth handshake (Ping): {e}"))?
        .into_inner();
    if ping.auth_audience.is_empty() {
        anyhow::bail!(
            "controller did not advertise a native auth audience; \
             spurctld must run with [auth] plugin = \"spur\""
        );
    }
    Ok(InterceptedService::new(
        channel,
        native_interceptor(&ping.auth_audience, ping.auth_epoch)?,
    ))
}

pub async fn wrap_after_agent_ping(channel: Channel) -> anyhow::Result<AuthChannel> {
    let mut client = spur_proto::proto::slurm_agent_client::SlurmAgentClient::new(channel.clone());
    let ping = client
        .ping(())
        .await
        .map_err(|e| anyhow::anyhow!("native auth handshake (agent Ping): {e}"))?
        .into_inner();
    if ping.auth_audience.is_empty() {
        anyhow::bail!(
            "agent did not advertise a native auth audience; \
             spurd must run with [auth] plugin = \"spur\""
        );
    }
    Ok(InterceptedService::new(
        channel,
        native_interceptor(&ping.auth_audience, ping.auth_epoch)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use nix::unistd::{Uid, User};
    use spur_core::native_jwks::HmacKeySet;
    use spur_core::native_mint::{bind_socket, open_minted, serve, unix_now, CredentialMint};
    use tonic::service::Interceptor;
    use tonic::Code;

    /// Both env cases live in ONE test on purpose: the variable is process-global, so two tests
    /// mutating it run concurrently under the default test harness and race each other.
    #[test]
    fn env_token_is_trimmed_and_blank_is_not_a_credential() {
        // SAFETY: this is the only test that touches TOKEN_ENV, so no other thread reads it here.
        unsafe { std::env::set_var(TOKEN_ENV, "  abc123\n") };
        assert_eq!(
            load_token().as_deref(),
            Some("abc123"),
            "the env credential wins and is trimmed"
        );

        unsafe { std::env::set_var(TOKEN_ENV, "   ") };
        // Blank falls through to the file rather than sending a literal "Bearer " with no token.
        let blank = load_token();
        unsafe { std::env::remove_var(TOKEN_ENV) };
        assert!(
            blank.as_deref() != Some(""),
            "a blank env value must not become an empty credential"
        );
    }

    #[test]
    fn no_credential_yields_an_interceptor_that_adds_no_header() {
        let mut i = AuthInterceptor::default();
        assert!(matches!(i.attach, CredAttach::None));
        let req = i.call(Request::new(())).unwrap();
        assert!(req.metadata().get("authorization").is_none());
    }

    #[test]
    fn native_interceptor_fails_closed_when_the_mint_is_down() {
        let mut i = AuthInterceptor {
            attach: CredAttach::Native(NativeMintParams {
                socket: PathBuf::from("/no/such/auth.sock"),
                audience: "http://127.0.0.1:6817".into(),
                epoch: 0,
                timeout: Duration::from_secs(5),
            }),
        };
        let err = i.call(Request::new(())).unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
        assert!(
            err.message()
                .contains("start spurauthd or set SPUR_AUTH_SOCKET"),
            "{}",
            err.message()
        );
        assert!(!err.message().contains("container"));
    }

    #[test]
    fn wrap_with_audience_native_needs_ping() {
        let mut i = AuthInterceptor {
            attach: CredAttach::NativeNeedsPing,
        };
        let err = i.call(Request::new(())).unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
        assert!(err.message().contains("Ping handshake"));
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

        let mut i = AuthInterceptor {
            attach: CredAttach::Native(NativeMintParams {
                socket: sock,
                audience: "http://controller:6817".into(),
                epoch: 0,
                timeout: Duration::from_secs(5),
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
            assert_eq!(cred.audience, "http://controller:6817");
            assert_eq!(cred.audience_epoch, 0);
            let uid = nix::unistd::getuid().as_raw();
            assert_eq!(cred.uid, uid);
            let name = User::from_uid(Uid::from_raw(uid)).unwrap().unwrap().name;
            assert_eq!(cred.user, name);
        }
    }

    /// One test because `$SPUR_AUTH_SOCKET` is process-global; serial because other tests
    /// clear every `SPUR_*` variable.
    #[test]
    #[serial_test::serial(env_injection)]
    fn ungranted_mounts_of_the_socket_are_refused_at_submission() {
        let prev = std::env::var_os("SPUR_AUTH_SOCKET");
        unsafe { std::env::set_var("SPUR_AUTH_SOCKET", "/run/spur/c1/auth.sock") };
        let mounts = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let refused = refuse_ungranted_socket_mounts(&mounts(&["/run/spur:/run/spur"]), false);
        let socket_ro =
            refuse_ungranted_socket_mounts(&mounts(&["/run/spur/c1/auth.sock:/s:ro"]), false);
        let granted = refuse_ungranted_socket_mounts(&mounts(&["/run/spur:/run/spur"]), true);
        let unrelated = refuse_ungranted_socket_mounts(&mounts(&["/data:/data"]), false);
        let sbatch = crate::sbatch::build_sbatch_job_spec(
            <crate::sbatch::SbatchArgs as clap::Parser>::parse_from([
                "sbatch",
                "--container-image=img.sqsh",
                "--container-mounts=/run/spur:/run/spur",
                "--wrap=true",
            ]),
            None,
            "sbatch",
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("SPUR_AUTH_SOCKET", v),
                None => std::env::remove_var("SPUR_AUTH_SOCKET"),
            }
        }
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("--container-auth-socket"));
        assert!(socket_ro.is_err());
        assert!(granted.is_ok(), "the flag allows the mount");
        assert!(unrelated.is_ok());
        assert!(sbatch.is_err(), "sbatch refuses it before submitting");
    }

    #[test]
    fn mint_failure_text_depends_on_where_the_caller_runs() {
        let err = MintError::Unavailable {
            path: "/run/spur/c1/auth.sock".into(),
        };
        let inside = mint_failure_text(&err, true);
        assert!(
            inside.contains("--container-auth-socket") && inside.contains("spurauthd is running")
        );
        let outside = mint_failure_text(&err, false);
        assert!(outside.contains("start spurauthd or set SPUR_AUTH_SOCKET"));
        assert!(!outside.contains("container"));
    }

    /// Both env cases live in one test: `$SPUR_CONF` / `$SPUR_CLUSTER_NAME` are process-global.
    #[test]
    #[serial_test::serial(env_injection)]
    fn native_cluster_name_requires_env_without_config() {
        let prev_conf = std::env::var("SPUR_CONF").ok();
        let prev_cluster = std::env::var(CLUSTER_ENV).ok();
        unsafe {
            std::env::set_var("SPUR_CONF", "/no/such/spur.conf");
            std::env::remove_var(CLUSTER_ENV);
        }
        let missing = native_cluster_name();
        unsafe {
            std::env::set_var(CLUSTER_ENV, "cluster-a");
        }
        let got = native_cluster_name();
        unsafe {
            match prev_conf {
                Some(v) => std::env::set_var("SPUR_CONF", v),
                None => std::env::remove_var("SPUR_CONF"),
            }
            match prev_cluster {
                Some(v) => std::env::set_var(CLUSTER_ENV, v),
                None => std::env::remove_var(CLUSTER_ENV),
            }
        }
        assert!(
            missing.is_err(),
            "missing cluster name must fail closed: {missing:?}"
        );
        assert_eq!(got.unwrap(), "cluster-a");
    }
}
