// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Authentication and authorization.
//!
//! Supports JWT token verification for gRPC and REST APIs.
//! Auth mode configured via SlurmConfig.auth.plugin: "jwt", "spur", "none".

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authentication required")]
    NotAuthenticated,
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("token expired")]
    Expired,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("user {user} cannot {action} job owned by {owner}")]
    NotJobOwner {
        user: String,
        owner: String,
        action: String,
    },
    #[error("no such user on this host: {0}")]
    UnknownUser(String),
}

/// Bind a submitted spec to the authenticated caller.
///
/// Overwrites `user`/`uid`/`gid` from the verified identity rather than trusting
/// what the client sent. JWT identities re-resolve uid/gid through NSS on this
/// host (the token's uid is untrusted and it carries no gid). Native identities
/// already carry mint-host uid/gid and must not be looked up again.
/// Unauthenticated callers are left as-is.
pub fn bind_job_spec(
    spec: &mut crate::job::JobSpec,
    identity: Option<&Identity>,
) -> Result<(), AuthError> {
    let Some(id) = identity else {
        return Ok(());
    };
    let (uid, gid) = if id.trusted_unix {
        (id.uid, id.gid)
    } else {
        resolve_unix_credentials(&id.user)?
    };
    spec.user = id.user.clone();
    spec.uid = uid;
    spec.gid = gid;
    Ok(())
}

/// Subject under which the controller signs the credentials it presents to node agents.
///
/// An agent that verifies a credential carrying this subject knows the caller is the control plane,
/// not an end user, and gates controller-only RPCs on it. Kept here so the controller (which mints
/// the credential) and the agent (which checks it) cannot drift apart on the value.
pub const CONTROLLER_SUBJECT: &str = "spurctld";

/// Resolve a username to its UNIX credentials through NSS.
///
/// The controller derives uid/gid from the *authenticated* username rather than accepting them from
/// the wire: `TokenClaims` carries no gid at all, and a client-supplied uid is what allowed a job to
/// run as an arbitrary user (see the `allow_root_jobs` guard in spurd). Fails closed — an
/// unresolvable user is an error, never a fallback to uid 0.
pub fn resolve_unix_credentials(user: &str) -> Result<(u32, u32), AuthError> {
    if user.is_empty() {
        return Err(AuthError::UnknownUser("<empty>".into()));
    }
    match nix::unistd::User::from_name(user) {
        Ok(Some(u)) => Ok((u.uid.as_raw(), u.gid.as_raw())),
        Ok(None) => Err(AuthError::UnknownUser(user.to_string())),
        Err(e) => Err(AuthError::UnknownUser(format!("{user}: {e}"))),
    }
}

/// Reverse-resolve a UID to its username via NSS. `None` when the UID has no
/// passwd entry, mirroring how Slurm's `uid_to_string` falls back for display.
pub fn username_for_uid(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Authenticated identity extracted from a token or peer credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub is_admin: bool,
    /// Native mint signed uid/gid on the caller's host. The verifier must not
    /// `getpwuid` / NSS-resolve them again.
    #[serde(default)]
    pub trusted_unix: bool,
}

impl Identity {
    pub fn posix(user: impl Into<String>, uid: u32, gid: u32, is_admin: bool) -> Self {
        Self {
            user: user.into(),
            uid,
            gid,
            is_admin,
            trusted_unix: false,
        }
    }

    pub fn from_native_rpc(cred: &crate::native_cred::UserRpcCredential) -> Self {
        Self {
            user: cred.user.clone(),
            uid: cred.uid,
            gid: cred.gid,
            is_admin: false,
            trusted_unix: true,
        }
    }

    /// Create an admin identity (for internal daemon-to-daemon calls).
    pub fn admin() -> Self {
        Self {
            user: "root".into(),
            uid: 0,
            gid: 0,
            is_admin: true,
            trusted_unix: false,
        }
    }

    /// Check if this identity can cancel a job owned by `owner`.
    pub fn can_cancel_job(&self, owner: &str) -> Result<(), AuthError> {
        if self.is_admin || self.user == owner {
            Ok(())
        } else {
            Err(AuthError::NotJobOwner {
                user: self.user.clone(),
                owner: owner.into(),
                action: "cancel".into(),
            })
        }
    }

    /// Check if this identity can modify a job owned by `owner`.
    pub fn can_modify_job(&self, owner: &str) -> Result<(), AuthError> {
        if self.is_admin || self.user == owner {
            Ok(())
        } else {
            Err(AuthError::NotJobOwner {
                user: self.user.clone(),
                owner: owner.into(),
                action: "modify".into(),
            })
        }
    }

    /// Whether this identity is the cluster controller (its credential's subject).
    ///
    /// Node agents use this to gate controller-only RPCs: a job launch or cancel must arrive from
    /// the control plane, which allocates and accounts for it, not straight from a user's token.
    pub fn is_controller(&self) -> bool {
        self.user == CONTROLLER_SUBJECT
    }

    /// Check if this identity can perform admin operations.
    pub fn require_admin(&self) -> Result<(), AuthError> {
        if self.is_admin {
            Ok(())
        } else {
            Err(AuthError::PermissionDenied(format!(
                "user {} is not an admin",
                self.user
            )))
        }
    }
}

/// Check that a caller is allowed to perform `action` on a job owned by `owner`.
///
/// Access is granted to the job's owner and to an explicitly identified internal/daemon caller
/// (`is_internal` — the controller, or a verified admin). There is deliberately no bypass for an
/// empty `user` or a literal `"root"` string: an internal caller must be named by `is_internal`,
/// which the caller derives from a *verified* identity and never infers from a wire-supplied string
/// an attacker can set. An empty `user` therefore matches no owner and is denied unless
/// `is_internal`, so a job that runs as root (empty owner) stays reachable only by internal callers.
pub fn check_job_owner(
    user: &str,
    is_internal: bool,
    owner: &str,
    action: &str,
) -> Result<(), AuthError> {
    if is_internal || (!user.is_empty() && user == owner) {
        return Ok(());
    }
    Err(AuthError::NotJobOwner {
        user: user.into(),
        owner: owner.into(),
        action: action.into(),
    })
}

/// Ownership gate for user-initiated RPCs that may carry a Unix uid (e.g. `RunStep`).
///
/// When a verified identity is present, only that subject (or an admin/internal caller) may
/// act on the job — a matching uid alone cannot bypass a mismatched JWT. When unauthenticated
/// (`auth.mode = permissive` without a credential), the owner username or a matching `caller_uid`
/// against the job's submit-time uid is accepted (Slurm Munge-like same-session semantics).
pub fn check_job_caller(
    user: &str,
    caller_uid: Option<u32>,
    is_internal: bool,
    owner: &str,
    owner_uid: u32,
    identity: Option<&Identity>,
    action: &str,
) -> Result<(), AuthError> {
    if is_internal {
        return Ok(());
    }
    if let Some(id) = identity {
        if id.is_admin || id.user == owner {
            return Ok(());
        }
        return Err(AuthError::NotJobOwner {
            user: id.user.clone(),
            owner: owner.into(),
            action: action.into(),
        });
    }
    if !user.is_empty() && user == owner {
        return Ok(());
    }
    // Proto defaults and RPCs without a uid field (keepalive) send 0; treat that as
    // absent rather than a root-caller match.
    if let Some(uid) = caller_uid.filter(|&u| u != 0) {
        if uid == owner_uid {
            return Ok(());
        }
    }
    Err(AuthError::NotJobOwner {
        user: user.into(),
        owner: owner.into(),
        action: action.into(),
    })
}

/// JWT token claims.
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Subject (username).
    pub sub: String,
    /// User ID.
    pub uid: u32,
    /// Expiration (unix timestamp).
    pub exp: u64,
    /// Issued at (unix timestamp).
    pub iat: u64,
    /// Admin flag.
    #[serde(default)]
    pub admin: bool,
}

/// Generate a JWT token for a user.
pub fn generate_token(
    user: &str,
    uid: u32,
    is_admin: bool,
    secret: &[u8],
    ttl_secs: u64,
) -> Result<String, AuthError> {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = TokenClaims {
        sub: user.into(),
        uid,
        exp: now + ttl_secs,
        iat: now,
        admin: is_admin,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .map_err(|e| AuthError::InvalidToken(e.to_string()))
}

/// Verify a JWT token and return the identity.
pub fn verify_token(token: &str, secret: &[u8]) -> Result<Identity, AuthError> {
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let data = decode::<TokenClaims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
        _ => AuthError::InvalidToken(e.to_string()),
    })?;

    Ok(Identity::posix(
        data.claims.sub,
        data.claims.uid,
        0,
        data.claims.admin,
    ))
}

/// What to do with one request's `Authorization` header.
///
/// Shared by both daemons so the controller and the agent cannot drift apart on a security
/// decision: they wrap this in their own Tower layer, but the ruling itself lives here.
#[derive(Debug)]
pub enum BearerOutcome {
    /// Verified; carry this identity to the handler.
    Authenticated(Box<Identity>),
    /// No credential presented, and the mode tolerates that.
    Anonymous,
    /// Refuse the request with this message.
    Reject(String),
}

/// Which daemon is verifying native user credentials. Selects the audience id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierKind {
    Controller,
    Agent,
}

pub fn controller_audience(cluster: &str, hostname: &str) -> String {
    format!("spur/{cluster}/controller/{hostname}")
}

pub fn agent_audience(cluster: &str, hostname: &str) -> String {
    format!("spur/{cluster}/agent/{hostname}")
}

pub fn verifier_hostname() -> String {
    whoami::hostname().unwrap_or_else(|_| "unknown".into())
}

/// gRPC paths that advertise native audience/epoch and must not require a credential.
pub fn is_unauthenticated_auth_handshake(path: &str) -> bool {
    path == "/slurm.SlurmController/Ping" || path == "/slurm.SlurmAgent/Ping"
}

/// Native user-RPC verification (HMAC JWKS). When set, JWTs are not accepted.
#[derive(Clone)]
pub struct NativeAuth {
    pub cluster_id: String,
    pub keys: std::sync::Arc<crate::native_jwks::HmacKeySet>,
    pub skew_secs: u64,
    pub audience: String,
    pub epoch: u64,
    pub replay: std::sync::Arc<crate::native_replay::ReplayCache>,
    /// Agent-only: public keys for controller-to-agent identity.
    pub controller_keys: Option<std::sync::Arc<crate::native_jwks::Ed25519VerifyKeySet>>,
    /// Agent-only: public keys for job/step credentials.
    pub cred_keys: Option<std::sync::Arc<crate::native_jwks::Ed25519VerifyKeySet>>,
}

impl NativeAuth {
    pub fn new(
        cluster_id: impl Into<String>,
        keys: std::sync::Arc<crate::native_jwks::HmacKeySet>,
        audience: impl Into<String>,
        epoch: u64,
    ) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            keys,
            skew_secs: crate::native_mint::CLOCK_SKEW_SECS,
            audience: audience.into(),
            epoch,
            replay: std::sync::Arc::new(crate::native_replay::ReplayCache::new(
                crate::native_replay::DEFAULT_CAPACITY,
            )),
            controller_keys: None,
            cred_keys: None,
        }
    }

    pub fn for_kind(
        cluster_id: impl Into<String>,
        keys: std::sync::Arc<crate::native_jwks::HmacKeySet>,
        kind: VerifierKind,
    ) -> Self {
        let cluster_id = cluster_id.into();
        let host = verifier_hostname();
        let audience = match kind {
            VerifierKind::Controller => controller_audience(&cluster_id, &host),
            VerifierKind::Agent => agent_audience(&cluster_id, &host),
        };
        let epoch: u64 = rand::RngExt::random(&mut rand::rng());
        Self::new(cluster_id, keys, audience, epoch)
    }

    /// Load `/etc/spur/auth.jwks` (or `$SPUR_AUTH_JWKS`) when `plugin = "spur"`.
    pub fn from_config(
        config: &crate::config::SlurmConfig,
        kind: VerifierKind,
    ) -> Result<Option<Self>, crate::native_jwks::JwksError> {
        if config.auth.plugin != "spur" {
            return Ok(None);
        }
        Self::load_jwks(config.cluster_name.clone(), kind).map(Some)
    }

    /// Same JWKS load when `$SPUR_AUTH_PLUGIN=spur` and no config file is present.
    pub fn from_plugin_env(
        kind: VerifierKind,
    ) -> Result<Option<Self>, crate::native_jwks::JwksError> {
        if std::env::var("SPUR_AUTH_PLUGIN").unwrap_or_default() != "spur" {
            return Ok(None);
        }
        let cluster_name = std::env::var("SPUR_CLUSTER_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or(crate::native_jwks::JwksError::MissingClusterName)?;
        Self::load_jwks(cluster_name, kind).map(Some)
    }

    fn load_jwks(
        cluster_name: String,
        kind: VerifierKind,
    ) -> Result<Self, crate::native_jwks::JwksError> {
        let path = std::env::var("SPUR_AUTH_JWKS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| crate::native_jwks::AUTH_JWKS_PATH.to_string());
        let now = crate::native_mint::unix_now().unwrap_or(0);
        let keys = crate::native_jwks::HmacKeySet::from_path(std::path::Path::new(&path), now)?;
        let mut native = Self::for_kind(cluster_name, std::sync::Arc::new(keys), kind);
        if kind == VerifierKind::Agent {
            native.controller_keys = Some(std::sync::Arc::new(
                crate::native_jwks::Ed25519VerifyKeySet::from_path(
                    &crate::native_jwks::path_from_env_or(
                        "SPUR_CONTROLLER_VERIFICATION_JWKS",
                        crate::native_jwks::CONTROLLER_VERIFICATION_JWKS_PATH,
                    ),
                    now,
                )?,
            ));
            native.cred_keys = Some(std::sync::Arc::new(
                crate::native_jwks::Ed25519VerifyKeySet::from_path(
                    &crate::native_jwks::path_from_env_or(
                        "SPUR_CRED_VERIFICATION_JWKS",
                        crate::native_jwks::CRED_VERIFICATION_JWKS_PATH,
                    ),
                    now,
                )?,
            ));
        }
        Ok(native)
    }
}

/// Shared verifier used by controller, agent, REST, and k8s operator middleware.
#[derive(Clone)]
pub struct BearerAuth {
    pub mode: crate::config::AuthMode,
    pub jwt_key: Vec<u8>,
    pub native: Option<NativeAuth>,
}

impl BearerAuth {
    pub fn jwt(mode: crate::config::AuthMode, jwt_key: &[u8]) -> Self {
        Self {
            mode,
            jwt_key: jwt_key.to_vec(),
            native: None,
        }
    }

    pub fn from_config(
        config: &crate::config::SlurmConfig,
        jwt_key: &[u8],
        kind: VerifierKind,
    ) -> Result<Self, crate::native_jwks::JwksError> {
        Ok(Self {
            mode: config.auth.mode,
            jwt_key: jwt_key.to_vec(),
            native: NativeAuth::from_config(config, kind)?,
        })
    }

    /// JWT key plus optional native JWKS from `$SPUR_AUTH_PLUGIN` when no config file exists.
    pub fn from_env_or_jwt(
        mode: crate::config::AuthMode,
        jwt_key: &[u8],
        kind: VerifierKind,
    ) -> Result<Self, crate::native_jwks::JwksError> {
        Ok(Self {
            mode,
            jwt_key: jwt_key.to_vec(),
            native: NativeAuth::from_plugin_env(kind)?,
        })
    }

    /// Audience and boot epoch advertised on Ping. Empty audience when this
    /// process is not verifying native user credentials.
    pub fn advertised_handshake(&self) -> (String, u64) {
        self.native
            .as_ref()
            .map(|n| (n.audience.clone(), n.epoch))
            .unwrap_or_default()
    }

    pub fn authenticate(
        &self,
        header: Option<&str>,
        missing_credential_hint: &str,
    ) -> BearerOutcome {
        authenticate_bearer_inner(self, header, missing_credential_hint)
    }
}

/// Rule the `Authorization` header against the configured mode.
///
/// Deliberate properties:
/// * an INVALID credential is rejected in every mode that verifies — `permissive` tolerates the
///   *absence* of a credential, never a bad one, or forging would beat sending none;
/// * a malformed header is rejected rather than silently downgraded to anonymous;
/// * `disabled` ignores even a valid token, so it cannot be quietly stricter than it claims;
/// * `plugin = "spur"` verifies native credentials only (never JWT user tokens).
pub fn authenticate_bearer(
    mode: crate::config::AuthMode,
    jwt_key: &[u8],
    header: Option<&str>,
    missing_credential_hint: &str,
) -> BearerOutcome {
    BearerAuth::jwt(mode, jwt_key).authenticate(header, missing_credential_hint)
}

fn authenticate_bearer_inner(
    auth: &BearerAuth,
    header: Option<&str>,
    missing_credential_hint: &str,
) -> BearerOutcome {
    use crate::config::AuthMode;

    let token = match header {
        Some(h) => match h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
        {
            Some(t) if !t.trim().is_empty() => t.trim(),
            _ => {
                return BearerOutcome::Reject(
                    "malformed authorization header: expected 'Bearer <token>'".into(),
                )
            }
        },
        None => {
            return match auth.mode {
                AuthMode::Required => BearerOutcome::Reject(format!(
                    "authentication required: {missing_credential_hint}"
                )),
                _ => BearerOutcome::Anonymous,
            }
        }
    };

    if auth.mode == AuthMode::Disabled {
        return BearerOutcome::Anonymous;
    }

    if let Some(native) = &auth.native {
        return match verify_native_bearer(token, native) {
            Ok(identity) => {
                crate::native_metrics::inc_verify_ok();
                BearerOutcome::Authenticated(Box::new(identity))
            }
            Err(e) => {
                if e.to_string().contains("nonce already consumed") {
                    crate::native_metrics::inc_replay_reject();
                }
                crate::native_metrics::inc_verify_fail();
                BearerOutcome::Reject(format!("invalid credential: {e}"))
            }
        };
    }

    if crate::native_cred::SignedToken::from_base64url(token).is_ok() {
        return BearerOutcome::Reject(
            "native credential presented but [auth] plugin is not \"spur\"".into(),
        );
    }
    if auth.jwt_key.is_empty() {
        return BearerOutcome::Reject(
            "a token was presented but no auth.jwt_key is configured".into(),
        );
    }
    match verify_token(token, &auth.jwt_key) {
        Ok(identity) => BearerOutcome::Authenticated(Box::new(identity)),
        Err(e) => BearerOutcome::Reject(format!("invalid credential: {e}")),
    }
}

fn verify_native_bearer(token: &str, native: &NativeAuth) -> Result<Identity, AuthError> {
    let now =
        crate::native_mint::unix_now().map_err(|_| AuthError::InvalidToken("clock".into()))?;
    let signed = crate::native_cred::SignedToken::from_base64url(token)
        .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
    let kind = crate::native_cred::peek_kind(&signed.payload)
        .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
    match kind {
        crate::native_cred::CredentialKind::ControllerRpc => {
            let keys = native.controller_keys.as_ref().ok_or_else(|| {
                AuthError::InvalidToken("controller identity is not configured".into())
            })?;
            crate::native_service::verify_controller_rpc(
                token,
                keys.as_ref(),
                &native.cluster_id,
                now,
                native.replay.as_ref(),
            )
            .map_err(|e| AuthError::InvalidToken(e.to_string()))
        }
        crate::native_cred::CredentialKind::UserRpc => {
            let cred = crate::native_mint::verify_user_rpc(
                token,
                native.keys.as_ref(),
                &native.cluster_id,
                now,
                native.skew_secs,
            )
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
            cred.require_audience(&native.audience, native.epoch)
                .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
            native
                .replay
                .check_and_insert(&cred, now, native.skew_secs)
                .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
            Ok(Identity::from_native_rpc(&cred))
        }
        other => Err(AuthError::InvalidToken(format!(
            "credential kind {other} is not a bearer identity"
        ))),
    }
}

/// "none" auth — always returns an identity based on UNIX user.
pub fn auth_none() -> Identity {
    Identity::posix(
        whoami::username().unwrap_or_else(|_| "unknown".into()),
        nix::unistd::getuid().as_raw(),
        nix::unistd::getgid().as_raw(),
        nix::unistd::getuid().as_raw() == 0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &[u8] = b"test-secret-key-for-jwt";

    #[test]
    fn test_generate_and_verify() {
        let token = generate_token("alice", 1000, false, TEST_SECRET, 3600).unwrap();
        let id = verify_token(&token, TEST_SECRET).unwrap();
        assert_eq!(id.user, "alice");
        assert_eq!(id.uid, 1000);
        assert!(!id.is_admin);
    }

    #[test]
    fn bind_job_spec_uses_trusted_unix_without_nss() {
        let mut spec = crate::job::JobSpec {
            user: "claimed".into(),
            uid: 1,
            gid: 1,
            ..Default::default()
        };
        let id = Identity {
            user: "ghost".into(),
            uid: 4242,
            gid: 4243,
            is_admin: false,
            trusted_unix: true,
        };
        bind_job_spec(&mut spec, Some(&id)).unwrap();
        assert_eq!(spec.user, "ghost");
        assert_eq!(spec.uid, 4242);
        assert_eq!(spec.gid, 4243);
    }

    /// Both env cases live in one test: `$SPUR_AUTH_PLUGIN` / `$SPUR_CLUSTER_NAME`
    /// are process-global and parallel tests would race.
    #[test]
    fn from_plugin_env_reads_plugin_and_requires_cluster_name() {
        let prev_plugin = std::env::var("SPUR_AUTH_PLUGIN").ok();
        let prev_cluster = std::env::var("SPUR_CLUSTER_NAME").ok();

        std::env::remove_var("SPUR_AUTH_PLUGIN");
        let unset = NativeAuth::from_plugin_env(VerifierKind::Controller);

        std::env::set_var("SPUR_AUTH_PLUGIN", "spur");
        std::env::remove_var("SPUR_CLUSTER_NAME");
        let missing_cluster = NativeAuth::from_plugin_env(VerifierKind::Agent);

        match prev_plugin {
            Some(v) => std::env::set_var("SPUR_AUTH_PLUGIN", v),
            None => std::env::remove_var("SPUR_AUTH_PLUGIN"),
        }
        match prev_cluster {
            Some(v) => std::env::set_var("SPUR_CLUSTER_NAME", v),
            None => std::env::remove_var("SPUR_CLUSTER_NAME"),
        }

        assert!(unset.unwrap().is_none());
        match missing_cluster {
            Err(crate::native_jwks::JwksError::MissingClusterName) => {}
            Err(e) => panic!("expected MissingClusterName, got {e:?}"),
            Ok(_) => panic!("expected MissingClusterName, got Ok"),
        }
    }

    #[test]
    fn username_for_uid_is_none_for_unknown_uid() {
        // A uid with no passwd entry resolves to None, mirroring how Slurm's
        // display falls back when reason_uid can't be named.
        assert_eq!(username_for_uid(u32::MAX), None);
    }

    #[test]
    fn test_admin_token() {
        let token = generate_token("root", 0, true, TEST_SECRET, 3600).unwrap();
        let id = verify_token(&token, TEST_SECRET).unwrap();
        assert!(id.is_admin);
    }

    #[test]
    fn test_wrong_secret() {
        let token = generate_token("alice", 1000, false, TEST_SECRET, 3600).unwrap();
        let result = verify_token(&token, b"wrong-secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_can_cancel_own_job() {
        let id = Identity::posix("alice", 1000, 1000, false);
        assert!(id.can_cancel_job("alice").is_ok());
        assert!(id.can_cancel_job("bob").is_err());
    }

    #[test]
    fn test_admin_can_cancel_any() {
        let id = Identity::admin();
        assert!(id.can_cancel_job("alice").is_ok());
        assert!(id.can_cancel_job("bob").is_ok());
    }

    #[test]
    fn test_check_job_owner_allows_owner_and_internal() {
        // The owner reaches their own job; an explicitly internal caller reaches any job.
        assert!(check_job_owner("alice", false, "alice", "exec").is_ok());
        assert!(check_job_owner("", true, "alice", "exec").is_ok());
        assert!(check_job_owner("spurctld", true, "alice", "exec").is_ok());
    }

    /// The empty-user and literal-"root" bypasses are gone: only `is_internal` grants a non-owner,
    /// and it is never inferred from the (attacker-controllable) `user` string.
    #[test]
    fn test_check_job_owner_no_empty_or_root_string_bypass() {
        assert!(
            check_job_owner("", false, "alice", "exec").is_err(),
            "an empty user must not be treated as a daemon caller"
        );
        assert!(
            check_job_owner("root", false, "alice", "exec").is_err(),
            "a literal \"root\" username must not bypass the ownership check"
        );
    }

    #[test]
    fn test_check_job_owner_rejects_other_user() {
        let err = check_job_owner("bob", false, "alice", "exec").expect_err("bob must be denied");
        assert!(matches!(err, AuthError::NotJobOwner { .. }));
        assert_eq!(
            err.to_string(),
            "user bob cannot exec job owned by alice",
            "message names the requester, action, and owner"
        );
    }

    /// Jobs with an empty owner run as root, so only an internal caller is allowed — a named user is
    /// denied, and an empty user no longer slips through as a daemon.
    #[test]
    fn test_check_job_owner_empty_owner_restricts_to_internal() {
        assert!(check_job_owner("", true, "", "exec").is_ok());
        assert!(
            check_job_owner("", false, "", "exec").is_err(),
            "an empty non-internal caller must not match an empty owner"
        );
        assert!(
            check_job_owner("alice", false, "", "exec").is_err(),
            "empty-owner jobs run as root; granting access is a privilege escalation"
        );
    }

    /// A non-empty placeholder owner matches no caller, so it restricts the job
    /// to internal callers. Asserted so that introducing such a placeholder
    /// cannot silently lock users out of their own jobs.
    #[test]
    fn test_check_job_owner_placeholder_owner_restricts_to_internal() {
        assert!(check_job_owner("", true, "k8s", "exec").is_ok());
        assert!(
            check_job_owner("alice", false, "k8s", "exec").is_err(),
            "a placeholder owner denies every named user; record the real \
             submitter or leave the owner empty instead"
        );
    }

    #[test]
    fn check_job_caller_uid_fallback_when_unauthenticated() {
        assert!(check_job_caller(
            "localname",
            Some(1000),
            false,
            "jwt-subject",
            1000,
            None,
            "run a step in"
        )
        .is_ok());
    }

    #[test]
    fn check_job_caller_jwt_subject_must_match_owner() {
        let id = Identity::posix("jwt-subject", 1000, 1000, false);
        assert!(check_job_caller(
            "jwt-subject",
            Some(1000),
            false,
            "jwt-subject",
            1000,
            Some(&id),
            "run a step in"
        )
        .is_ok());
        let other = Identity::posix("other-jwt-user", 1000, 1000, false);
        assert!(
            check_job_caller(
                "localname",
                Some(1000),
                false,
                "jwt-subject",
                1000,
                Some(&other),
                "run a step in"
            )
            .is_err(),
            "uid alone must not bypass a JWT for a different owner"
        );
    }

    #[test]
    fn check_job_caller_rejects_mismatched_unauthenticated_user_and_uid() {
        assert!(
            check_job_caller("bob", Some(2000), false, "alice", 1000, None, "attach to").is_err()
        );
    }

    #[test]
    fn check_job_caller_uid_zero_does_not_bypass_username_check() {
        assert!(
            check_job_caller("bob", Some(0), false, "alice", 0, None, "run a step in").is_err()
        );
    }

    #[test]
    fn test_is_controller_only_matches_the_controller_subject() {
        let controller = Identity::posix(CONTROLLER_SUBJECT, 0, 0, true);
        assert!(controller.is_controller());
        let user = Identity::posix("alice", 1000, 1000, false);
        assert!(!user.is_controller());
    }

    #[test]
    fn test_require_admin() {
        let user = Identity::posix("alice", 1000, 1000, false);
        assert!(user.require_admin().is_err());
        assert!(Identity::admin().require_admin().is_ok());
    }

    // --- authenticate_bearer ---
    //
    // The function is the shared ruling used by both the controller and the agent. Testing it
    // directly (not just through the middleware wrappers) ensures the contract holds at the source
    // so neither daemon can silently diverge.

    fn bearer(key: &[u8]) -> String {
        format!(
            "Bearer {}",
            generate_token("alice", 1000, false, key, 3600).unwrap()
        )
    }

    #[test]
    fn required_rejects_missing_credential() {
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Required, TEST_SECRET, None, "hint"),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn permissive_allows_missing_credential() {
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Permissive,
                TEST_SECRET,
                None,
                "hint"
            ),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn disabled_allows_missing_credential() {
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Disabled, TEST_SECRET, None, "hint"),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn valid_token_is_authenticated_in_required_mode() {
        let h = bearer(TEST_SECRET);
        match authenticate_bearer(
            crate::config::AuthMode::Required,
            TEST_SECRET,
            Some(&h),
            "hint",
        ) {
            BearerOutcome::Authenticated(id) => {
                assert_eq!(id.user, "alice");
                assert_eq!(id.uid, 1000);
            }
            other => panic!("expected Authenticated, got {other:?}"),
        }
    }

    #[test]
    fn forged_token_rejected_in_permissive_mode() {
        // permissive tolerates absence of a credential, never a bad one.
        let forged = bearer(b"attacker-key");
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Permissive,
                TEST_SECRET,
                Some(&forged),
                "hint"
            ),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn malformed_header_always_rejected() {
        for bad in &["token-without-bearer-prefix", "Bearer ", "bearer", ""] {
            assert!(
                matches!(
                    authenticate_bearer(
                        crate::config::AuthMode::Permissive,
                        TEST_SECRET,
                        Some(bad),
                        "hint"
                    ),
                    BearerOutcome::Reject(_)
                ),
                "header {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn disabled_ignores_a_valid_token() {
        // disabled must not silently verify — that would make `disabled` secretly stricter.
        let h = bearer(TEST_SECRET);
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Disabled,
                TEST_SECRET,
                Some(&h),
                "hint"
            ),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn token_presented_but_no_key_configured_is_rejected() {
        let h = bearer(TEST_SECRET);
        assert!(matches!(
            authenticate_bearer(crate::config::AuthMode::Required, b"", Some(&h), "hint"),
            BearerOutcome::Reject(_)
        ));
    }

    fn hmac_set() -> crate::native_jwks::HmacKeySet {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let secret = [0x42u8; 32];
        let doc = serde_json::json!({
            "keys": [{
                "alg": "HS256",
                "kty": "oct",
                "kid": "k1",
                "k": URL_SAFE_NO_PAD.encode(secret),
                "use": "default"
            }]
        });
        crate::native_jwks::HmacKeySet::from_bytes(
            doc.to_string().as_bytes(),
            crate::native_mint::unix_now().unwrap(),
        )
        .unwrap()
    }

    fn sign_user_rpc(cluster: &str, audience: &str, epoch: u64, nonce: u8) -> String {
        use crate::native_cred::{SignedToken, UserRpcCredential};
        let keys = hmac_set();
        let now = crate::native_mint::unix_now().unwrap();
        let cred = UserRpcCredential {
            cluster_id: cluster.into(),
            issuer_host: "login".into(),
            audience: audience.into(),
            audience_epoch: epoch,
            user: "alice".into(),
            uid: 4242,
            gid: 4242,
            issued_at: now,
            expires_at: now + 30,
            nonce: [nonce; crate::native_cred::NONCE_LEN],
            key_id: keys.default_kid().to_string(),
        };
        let payload = cred.to_signing_bytes().unwrap();
        let (kid, signature) = keys.sign(&payload, now).unwrap();
        SignedToken {
            key_id: kid,
            payload,
            signature,
        }
        .to_base64url()
        .unwrap()
    }

    #[test]
    fn native_plugin_authenticates_minted_identity_without_nss() {
        let keys = std::sync::Arc::new(hmac_set());
        let audience = "spur/cluster-a/controller/ctld";
        let token = sign_user_rpc("cluster-a", audience, 9, 1);
        let auth = BearerAuth {
            mode: crate::config::AuthMode::Required,
            jwt_key: Vec::new(),
            native: Some(NativeAuth::new("cluster-a", keys, audience, 9)),
        };
        match auth.authenticate(Some(&format!("Bearer {token}")), "hint") {
            BearerOutcome::Authenticated(id) => {
                assert_eq!(id.user, "alice");
                assert_eq!(id.uid, 4242);
                assert_eq!(id.gid, 4242);
                assert!(id.trusted_unix);
                assert!(!id.is_admin);
            }
            other => panic!("expected Authenticated, got {other:?}"),
        }
    }

    #[test]
    fn native_plugin_rejects_jwt_wrong_cluster_wrong_audience_and_replay() {
        let keys = std::sync::Arc::new(hmac_set());
        let audience = "spur/cluster-a/controller/ctld";
        let native = Some(NativeAuth::new(
            "cluster-a",
            std::sync::Arc::clone(&keys),
            audience,
            9,
        ));
        let jwt = bearer(TEST_SECRET);
        let auth = BearerAuth {
            mode: crate::config::AuthMode::Permissive,
            jwt_key: TEST_SECRET.to_vec(),
            native: native.clone(),
        };
        assert!(matches!(
            auth.authenticate(Some(&jwt), "hint"),
            BearerOutcome::Reject(_)
        ));
        let other_cluster = sign_user_rpc("other-cluster", audience, 9, 2);
        let auth = BearerAuth {
            mode: crate::config::AuthMode::Required,
            jwt_key: Vec::new(),
            native: native.clone(),
        };
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {other_cluster}")), "hint"),
            BearerOutcome::Reject(_)
        ));
        let wrong_aud = sign_user_rpc("cluster-a", "spur/cluster-a/agent/n1", 9, 3);
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {wrong_aud}")), "hint"),
            BearerOutcome::Reject(_)
        ));
        let token = sign_user_rpc("cluster-a", audience, 9, 4);
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {token}")), "hint"),
            BearerOutcome::Authenticated(_)
        ));
        assert!(
            matches!(
                auth.authenticate(Some(&format!("Bearer {token}")), "hint"),
                BearerOutcome::Reject(_)
            ),
            "the same nonce at the same audience must be replay"
        );
        let fresh = sign_user_rpc("cluster-a", audience, 9, 5);
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {fresh}")), "hint"),
            BearerOutcome::Authenticated(_)
        ));
    }

    #[test]
    fn jwt_plugin_rejects_a_native_credential() {
        let token = sign_user_rpc("cluster-a", "aud", 0, 1);
        assert!(matches!(
            authenticate_bearer(
                crate::config::AuthMode::Required,
                TEST_SECRET,
                Some(&format!("Bearer {token}")),
                "hint"
            ),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn canonical_audiences_include_cluster_role_and_host() {
        assert_eq!(
            controller_audience("prod", "ctld1"),
            "spur/prod/controller/ctld1"
        );
        assert_eq!(agent_audience("prod", "gpu-0"), "spur/prod/agent/gpu-0");
        assert!(is_unauthenticated_auth_handshake(
            "/slurm.SlurmController/Ping"
        ));
        assert!(is_unauthenticated_auth_handshake("/slurm.SlurmAgent/Ping"));
        assert!(!is_unauthenticated_auth_handshake(
            "/slurm.SlurmController/SubmitJob"
        ));
    }
}
