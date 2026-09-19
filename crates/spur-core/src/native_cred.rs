// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Versioned native Spur credentials (not JWT).
//!
//! Canonical signing bytes are a big-endian, length-prefixed encoding with a
//! fixed field order. `kind` is authenticated immediately after the version so
//! a user-RPC payload cannot verify as a job or step credential (and vice
//! versa). Signatures are not applied here; later steps sign these bytes.

use std::collections::BTreeSet;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use thiserror::Error;

use crate::job::JobId;
use crate::step::StepId;

/// Claims schema encoded after [`MAGIC`].
pub const CREDENTIAL_VERSION: u16 = 1;

/// Encoding tag. Changing the layout requires a new magic, not a claims bump.
const MAGIC: &[u8; 8] = b"SPURCRD1";

pub const USER_RPC_CONTEXT: &str = "spur/native/user-rpc";
pub const EXECUTION_CONTEXT: &str = "spur/native/execution";
pub const CONTROLLER_RPC_CONTEXT: &str = "spur/native/controller-rpc";
pub const NODE_IDENTITY_CONTEXT: &str = "spur/native/node-identity";
pub const FORWARDED_IDENTITY_CONTEXT: &str = "spur/native/forwarded-identity";

pub const NONCE_LEN: usize = 16;
pub const CREDENTIAL_ID_LEN: usize = 16;
pub const DIGEST_LEN: usize = 32;

const MAX_STR: usize = 1024;
const MAX_NODES: usize = 4096;
const MAX_DEVICES: usize = 4096;
const MAX_GIDS: usize = 1024;
const MAX_BLOB: usize = 65_536;
const TOKEN_MAGIC: &[u8; 8] = b"SPURTKN1";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CredentialError {
    #[error("unsupported credential version {0}")]
    UnsupportedVersion(u16),
    #[error("malformed credential ({0})")]
    Malformed(&'static str),
    #[error("credential kind is {found}, expected {expected}")]
    WrongKind {
        expected: CredentialKind,
        found: CredentialKind,
    },
    #[error("credential kind {found} is not an execution credential")]
    NotExecution { found: CredentialKind },
    #[error("credential cluster_id mismatch")]
    ClusterMismatch,
    #[error("credential audience mismatch")]
    AudienceMismatch,
    #[error("credential audience epoch mismatch")]
    AudienceEpochMismatch,
    #[error("credential expired")]
    Expired,
    #[error("credential not yet valid")]
    NotYetValid,
    #[error("credential issued too far in the future")]
    IssuedInFuture,
    #[error("credential lifetime is invalid")]
    InvalidLifetime,
    #[error("unknown key id {0}")]
    UnknownKeyId(String),
    #[error("credential signature is invalid")]
    BadSignature,
    #[error("credential nonce already consumed")]
    Replay,
    #[error("execution credential does not name this node")]
    WrongNode,
    #[error("execution credential run_attempt mismatch")]
    WrongAttempt,
    #[error("execution credential resource slice mismatch")]
    ResourceMismatch,
    #[error("execution credential command digest mismatch")]
    CommandDigestMismatch,
    #[error("execution credential reused with different claims")]
    IdempotencyConflict,
    #[error("execution credential uid/gid mismatch")]
    IdentityMismatch,
}

/// How an agent should surface a credential failure on the gRPC wire.
///
/// Kept next to the error so `spurd` and `spur-k8s` cannot drift on which
/// failures are unauthenticated vs a bound-but-wrong launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatusCode {
    Unauthenticated,
    PermissionDenied,
    FailedPrecondition,
}

impl CredentialError {
    pub fn status_code(&self) -> CredentialStatusCode {
        use CredentialError::*;
        match self {
            WrongNode
            | ResourceMismatch
            | CommandDigestMismatch
            | IdempotencyConflict
            | WrongAttempt => CredentialStatusCode::FailedPrecondition,
            IdentityMismatch | WrongKind { .. } | NotExecution { .. } => {
                CredentialStatusCode::PermissionDenied
            }
            _ => CredentialStatusCode::Unauthenticated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CredentialKind {
    UserRpc = 1,
    Job = 2,
    Step = 3,
    ControllerRpc = 4,
    Node = 5,
    Forwarded = 6,
}

impl CredentialKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserRpc => "user-rpc",
            Self::Job => "job",
            Self::Step => "step",
            Self::ControllerRpc => "controller-rpc",
            Self::Node => "node",
            Self::Forwarded => "forwarded",
        }
    }

    pub const fn is_execution(self) -> bool {
        matches!(self, Self::Job | Self::Step)
    }

    pub(crate) fn from_u8(v: u8) -> Result<Self, CredentialError> {
        match v {
            1 => Ok(Self::UserRpc),
            2 => Ok(Self::Job),
            3 => Ok(Self::Step),
            4 => Ok(Self::ControllerRpc),
            5 => Ok(Self::Node),
            6 => Ok(Self::Forwarded),
            _ => Err(CredentialError::Malformed("unknown kind")),
        }
    }
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HMAC-signed user-RPC payload for transport (Unix mint → RPC header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedToken {
    pub key_id: String,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
}

impl SignedToken {
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        if self.key_id.is_empty() {
            return Err(CredentialError::Malformed("key_id"));
        }
        let mut w = Writer::new();
        w.magic_tag(TOKEN_MAGIC);
        w.u16(CREDENTIAL_VERSION);
        w.str(&self.key_id)?;
        w.blob(&self.payload)?;
        w.blob(&self.signature)?;
        Ok(w.into_inner())
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.expect_magic(TOKEN_MAGIC)?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let token = Self {
            key_id: r.str()?,
            payload: r.blob()?,
            signature: r.blob()?,
        };
        r.finish()?;
        if token.key_id.is_empty() || token.payload.is_empty() || token.signature.is_empty() {
            return Err(CredentialError::Malformed("empty token field"));
        }
        Ok(token)
    }

    pub fn to_base64url(&self) -> Result<String, CredentialError> {
        Ok(payload_to_base64url(&self.to_wire_bytes()?))
    }

    pub fn from_base64url(s: &str) -> Result<Self, CredentialError> {
        Self::from_wire_bytes(&payload_from_base64url(s)?)
    }
}

/// One-shot user RPC identity. Roles and supplementary groups are absent;
/// `user` is the NSS name resolved on the minting host, not by a remote verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRpcCredential {
    pub cluster_id: String,
    pub issuer_host: String,
    pub audience: String,
    pub audience_epoch: u64,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: [u8; NONCE_LEN],
    pub key_id: String,
}

/// Signed proof that a controller peer already authenticated a user RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedIdentity {
    pub cluster_id: String,
    pub source_controller_id: u64,
    pub dest_leader_id: u64,
    pub term: u64,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub trusted_unix: bool,
    pub is_admin: bool,
    pub action: String,
    pub request_digest: [u8; DIGEST_LEN],
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: [u8; NONCE_LEN],
    pub key_id: String,
}

/// Controller-to-agent service identity. Distinct from user-RPC and node JWTs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerRpcCredential {
    pub cluster_id: String,
    pub audience: String,
    pub audience_epoch: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: [u8; NONCE_LEN],
    pub key_id: String,
}

/// Node identity presented on registration, heartbeats, and recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentityCredential {
    pub cluster_id: String,
    pub hostname: String,
    pub audience: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub key_id: String,
}

/// Controller-signed job or step launch authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionCredential {
    pub kind: CredentialKind,
    pub cluster_id: String,
    pub key_id: String,
    pub job_id: JobId,
    pub step_id: StepId,
    pub run_attempt: u32,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub supplementary_gids: Vec<u32>,
    pub account: String,
    pub partition: String,
    pub qos: String,
    pub resources_by_node: Vec<NodeResourceSlice>,
    pub command_digest: [u8; DIGEST_LEN],
    pub container_digest: Option<[u8; DIGEST_LEN]>,
    pub issued_at: u64,
    pub not_before: u64,
    pub expires_at: u64,
    pub credential_id: [u8; CREDENTIAL_ID_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeResourceSlice {
    pub node: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub devices: Vec<DeviceSlice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSlice {
    pub name: String,
    pub device_id: u32,
    pub count: u64,
}

impl ExecutionCredential {
    pub fn target_nodes(&self) -> impl Iterator<Item = &str> {
        self.resources_by_node.iter().map(|n| n.node.as_str())
    }
}

/// Kind stored in `bytes` without decoding claims. Used to route verification.
pub fn peek_kind(bytes: &[u8]) -> Result<CredentialKind, CredentialError> {
    let mut r = Reader::new(bytes);
    r.magic()?;
    let version = r.u16()?;
    if version != CREDENTIAL_VERSION {
        return Err(CredentialError::UnsupportedVersion(version));
    }
    CredentialKind::from_u8(r.u8()?)
}

pub fn payload_to_base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn payload_from_base64url(s: &str) -> Result<Vec<u8>, CredentialError> {
    URL_SAFE_NO_PAD
        .decode(s.trim().as_bytes())
        .map_err(|_| CredentialError::Malformed("invalid base64url"))
}

impl UserRpcCredential {
    pub fn kind(&self) -> CredentialKind {
        CredentialKind::UserRpc
    }

    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        let mut w = Writer::new();
        w.magic();
        w.u16(CREDENTIAL_VERSION);
        w.u8(CredentialKind::UserRpc as u8);
        w.str(USER_RPC_CONTEXT)?;
        w.str(&self.cluster_id)?;
        w.str(&self.issuer_host)?;
        w.str(&self.audience)?;
        w.u64(self.audience_epoch);
        w.str(&self.user)?;
        w.u32(self.uid);
        w.u32(self.gid);
        w.u64(self.issued_at);
        w.u64(self.expires_at);
        w.fixed(&self.nonce);
        w.str(&self.key_id)?;
        Ok(w.into_inner())
    }

    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.magic()?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let kind = CredentialKind::from_u8(r.u8()?)?;
        if kind != CredentialKind::UserRpc {
            return Err(CredentialError::WrongKind {
                expected: CredentialKind::UserRpc,
                found: kind,
            });
        }
        let context = r.str()?;
        if context != USER_RPC_CONTEXT {
            return Err(CredentialError::Malformed("protocol_context"));
        }
        let cred = Self {
            cluster_id: r.str()?,
            issuer_host: r.str()?,
            audience: r.str()?,
            audience_epoch: r.u64()?,
            user: r.str()?,
            uid: r.u32()?,
            gid: r.u32()?,
            issued_at: r.u64()?,
            expires_at: r.u64()?,
            nonce: r.fixed()?,
            key_id: r.str()?,
        };
        r.finish()?;
        cred.validate()?;
        Ok(cred)
    }

    pub fn require_cluster(&self, cluster_id: &str) -> Result<(), CredentialError> {
        if self.cluster_id == cluster_id {
            Ok(())
        } else {
            Err(CredentialError::ClusterMismatch)
        }
    }

    pub fn require_audience(&self, audience: &str, epoch: u64) -> Result<(), CredentialError> {
        if self.audience != audience {
            return Err(CredentialError::AudienceMismatch);
        }
        if self.audience_epoch != epoch {
            return Err(CredentialError::AudienceEpochMismatch);
        }
        Ok(())
    }

    pub fn validate_time(&self, now_unix: u64, skew_secs: u64) -> Result<(), CredentialError> {
        check_issued_expires(self.issued_at, self.expires_at, now_unix, skew_secs)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        require_nonempty("cluster_id", &self.cluster_id)?;
        require_nonempty("issuer_host", &self.issuer_host)?;
        require_nonempty("audience", &self.audience)?;
        require_nonempty("user", &self.user)?;
        require_nonempty("key_id", &self.key_id)?;
        if self.expires_at <= self.issued_at {
            return Err(CredentialError::InvalidLifetime);
        }
        Ok(())
    }
}

impl ForwardedIdentity {
    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        let mut w = Writer::new();
        w.magic();
        w.u16(CREDENTIAL_VERSION);
        w.u8(CredentialKind::Forwarded as u8);
        w.str(FORWARDED_IDENTITY_CONTEXT)?;
        w.str(&self.cluster_id)?;
        w.u64(self.source_controller_id);
        w.u64(self.dest_leader_id);
        w.u64(self.term);
        w.str(&self.user)?;
        w.u32(self.uid);
        w.u32(self.gid);
        w.u8(u8::from(self.trusted_unix));
        w.u8(u8::from(self.is_admin));
        w.str(&self.action)?;
        w.fixed(&self.request_digest);
        w.u64(self.issued_at);
        w.u64(self.expires_at);
        w.fixed(&self.nonce);
        w.str(&self.key_id)?;
        Ok(w.into_inner())
    }

    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.magic()?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let kind = CredentialKind::from_u8(r.u8()?)?;
        if kind != CredentialKind::Forwarded {
            return Err(CredentialError::WrongKind {
                expected: CredentialKind::Forwarded,
                found: kind,
            });
        }
        if r.str()? != FORWARDED_IDENTITY_CONTEXT {
            return Err(CredentialError::Malformed("protocol_context"));
        }
        let cred = Self {
            cluster_id: r.str()?,
            source_controller_id: r.u64()?,
            dest_leader_id: r.u64()?,
            term: r.u64()?,
            user: r.str()?,
            uid: r.u32()?,
            gid: r.u32()?,
            trusted_unix: r.u8()? != 0,
            is_admin: r.u8()? != 0,
            action: r.str()?,
            request_digest: r.fixed()?,
            issued_at: r.u64()?,
            expires_at: r.u64()?,
            nonce: r.fixed()?,
            key_id: r.str()?,
        };
        r.finish()?;
        cred.validate()?;
        Ok(cred)
    }

    pub fn validate_time(&self, now: u64, skew: u64) -> Result<(), CredentialError> {
        check_issued_expires(self.issued_at, self.expires_at, now, skew)
    }

    pub fn require_destination(&self, dest: u64) -> Result<(), CredentialError> {
        if self.dest_leader_id == dest {
            Ok(())
        } else {
            Err(CredentialError::AudienceMismatch)
        }
    }

    pub fn require_digest(&self, digest: &[u8; DIGEST_LEN]) -> Result<(), CredentialError> {
        if &self.request_digest == digest {
            Ok(())
        } else {
            Err(CredentialError::Malformed("request digest"))
        }
    }

    pub fn require_action(&self, action: &str) -> Result<(), CredentialError> {
        if self.action == action {
            Ok(())
        } else {
            Err(CredentialError::Malformed("action"))
        }
    }

    fn validate(&self) -> Result<(), CredentialError> {
        require_nonempty("cluster_id", &self.cluster_id)?;
        require_nonempty("user", &self.user)?;
        require_nonempty("action", &self.action)?;
        require_nonempty("key_id", &self.key_id)?;
        if self.expires_at <= self.issued_at {
            return Err(CredentialError::InvalidLifetime);
        }
        Ok(())
    }
}

impl ControllerRpcCredential {
    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        let mut w = Writer::new();
        w.magic();
        w.u16(CREDENTIAL_VERSION);
        w.u8(CredentialKind::ControllerRpc as u8);
        w.str(CONTROLLER_RPC_CONTEXT)?;
        w.str(&self.cluster_id)?;
        w.str(&self.audience)?;
        w.u64(self.audience_epoch);
        w.u64(self.issued_at);
        w.u64(self.expires_at);
        w.fixed(&self.nonce);
        w.str(&self.key_id)?;
        Ok(w.into_inner())
    }

    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.magic()?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let kind = CredentialKind::from_u8(r.u8()?)?;
        if kind != CredentialKind::ControllerRpc {
            return Err(CredentialError::WrongKind {
                expected: CredentialKind::ControllerRpc,
                found: kind,
            });
        }
        if r.str()? != CONTROLLER_RPC_CONTEXT {
            return Err(CredentialError::Malformed("protocol_context"));
        }
        let cred = Self {
            cluster_id: r.str()?,
            audience: r.str()?,
            audience_epoch: r.u64()?,
            issued_at: r.u64()?,
            expires_at: r.u64()?,
            nonce: r.fixed()?,
            key_id: r.str()?,
        };
        r.finish()?;
        cred.validate()?;
        Ok(cred)
    }

    pub fn validate_time(&self, now: u64, skew: u64) -> Result<(), CredentialError> {
        check_issued_expires(self.issued_at, self.expires_at, now, skew)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        require_nonempty("cluster_id", &self.cluster_id)?;
        require_nonempty("audience", &self.audience)?;
        require_nonempty("key_id", &self.key_id)?;
        if self.expires_at <= self.issued_at {
            return Err(CredentialError::InvalidLifetime);
        }
        Ok(())
    }
}

impl NodeIdentityCredential {
    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        let mut w = Writer::new();
        w.magic();
        w.u16(CREDENTIAL_VERSION);
        w.u8(CredentialKind::Node as u8);
        w.str(NODE_IDENTITY_CONTEXT)?;
        w.str(&self.cluster_id)?;
        w.str(&self.hostname)?;
        w.str(&self.audience)?;
        w.u64(self.issued_at);
        w.u64(self.expires_at);
        w.str(&self.key_id)?;
        Ok(w.into_inner())
    }

    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.magic()?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let kind = CredentialKind::from_u8(r.u8()?)?;
        if kind != CredentialKind::Node {
            return Err(CredentialError::WrongKind {
                expected: CredentialKind::Node,
                found: kind,
            });
        }
        if r.str()? != NODE_IDENTITY_CONTEXT {
            return Err(CredentialError::Malformed("protocol_context"));
        }
        let cred = Self {
            cluster_id: r.str()?,
            hostname: r.str()?,
            audience: r.str()?,
            issued_at: r.u64()?,
            expires_at: r.u64()?,
            key_id: r.str()?,
        };
        r.finish()?;
        cred.validate()?;
        Ok(cred)
    }

    pub fn validate_time(&self, now: u64, skew: u64) -> Result<(), CredentialError> {
        check_issued_expires(self.issued_at, self.expires_at, now, skew)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        require_nonempty("cluster_id", &self.cluster_id)?;
        require_nonempty("hostname", &self.hostname)?;
        require_nonempty("audience", &self.audience)?;
        require_nonempty("key_id", &self.key_id)?;
        if self.expires_at <= self.issued_at {
            return Err(CredentialError::InvalidLifetime);
        }
        Ok(())
    }
}

impl ExecutionCredential {
    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, CredentialError> {
        let cred = self.canonical();
        cred.validate()?;
        let mut w = Writer::new();
        w.magic();
        w.u16(CREDENTIAL_VERSION);
        w.u8(cred.kind as u8);
        w.str(EXECUTION_CONTEXT)?;
        w.str(&cred.cluster_id)?;
        w.str(&cred.key_id)?;
        w.u32(cred.job_id);
        w.u32(cred.step_id);
        w.u32(cred.run_attempt);
        w.str(&cred.user)?;
        w.u32(cred.uid);
        w.u32(cred.gid);
        w.u32(
            u32::try_from(cred.supplementary_gids.len())
                .map_err(|_| CredentialError::Malformed("gids"))?,
        );
        for gid in &cred.supplementary_gids {
            w.u32(*gid);
        }
        w.str(&cred.account)?;
        w.str(&cred.partition)?;
        w.str(&cred.qos)?;
        w.u32(
            u32::try_from(cred.resources_by_node.len())
                .map_err(|_| CredentialError::Malformed("nodes"))?,
        );
        for node in &cred.resources_by_node {
            w.str(&node.node)?;
            w.u32(node.cpus);
            w.u64(node.memory_mb);
            w.u32(
                u32::try_from(node.devices.len())
                    .map_err(|_| CredentialError::Malformed("devices"))?,
            );
            for dev in &node.devices {
                w.str(&dev.name)?;
                w.u32(dev.device_id);
                w.u64(dev.count);
            }
        }
        w.fixed(&cred.command_digest);
        match &cred.container_digest {
            None => w.u8(0),
            Some(d) => {
                w.u8(1);
                w.fixed(d);
            }
        }
        w.u64(cred.issued_at);
        w.u64(cred.not_before);
        w.u64(cred.expires_at);
        w.fixed(&cred.credential_id);
        Ok(w.into_inner())
    }

    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, CredentialError> {
        let mut r = Reader::new(bytes);
        r.magic()?;
        let version = r.u16()?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        let kind = CredentialKind::from_u8(r.u8()?)?;
        if !kind.is_execution() {
            return Err(CredentialError::NotExecution { found: kind });
        }
        let context = r.str()?;
        if context != EXECUTION_CONTEXT {
            return Err(CredentialError::Malformed("protocol_context"));
        }
        let cluster_id = r.str()?;
        let key_id = r.str()?;
        let job_id = r.u32()?;
        let step_id = r.u32()?;
        let run_attempt = r.u32()?;
        let user = r.str()?;
        let uid = r.u32()?;
        let gid = r.u32()?;
        let gid_n = r.u32()? as usize;
        if gid_n > MAX_GIDS {
            return Err(CredentialError::Malformed("gids"));
        }
        let mut supplementary_gids = Vec::with_capacity(gid_n);
        for _ in 0..gid_n {
            supplementary_gids.push(r.u32()?);
        }
        let account = r.str()?;
        let partition = r.str()?;
        let qos = r.str()?;
        let node_n = r.u32()? as usize;
        if node_n == 0 || node_n > MAX_NODES {
            return Err(CredentialError::Malformed("nodes"));
        }
        let mut resources_by_node = Vec::with_capacity(node_n);
        for _ in 0..node_n {
            let node = r.str()?;
            let cpus = r.u32()?;
            let memory_mb = r.u64()?;
            let dev_n = r.u32()? as usize;
            if dev_n > MAX_DEVICES {
                return Err(CredentialError::Malformed("devices"));
            }
            let mut devices = Vec::with_capacity(dev_n);
            for _ in 0..dev_n {
                devices.push(DeviceSlice {
                    name: r.str()?,
                    device_id: r.u32()?,
                    count: r.u64()?,
                });
            }
            resources_by_node.push(NodeResourceSlice {
                node,
                cpus,
                memory_mb,
                devices,
            });
        }
        let command_digest = r.fixed()?;
        let container_digest = match r.u8()? {
            0 => None,
            1 => Some(r.fixed()?),
            _ => return Err(CredentialError::Malformed("container_digest")),
        };
        let cred = Self {
            kind,
            cluster_id,
            key_id,
            job_id,
            step_id,
            run_attempt,
            user,
            uid,
            gid,
            supplementary_gids,
            account,
            partition,
            qos,
            resources_by_node,
            command_digest,
            container_digest,
            issued_at: r.u64()?,
            not_before: r.u64()?,
            expires_at: r.u64()?,
            credential_id: r.fixed()?,
        };
        r.finish()?;
        cred.validate()?;
        Ok(cred)
    }

    pub fn require_cluster(&self, cluster_id: &str) -> Result<(), CredentialError> {
        if self.cluster_id == cluster_id {
            Ok(())
        } else {
            Err(CredentialError::ClusterMismatch)
        }
    }

    pub fn require_kind(&self, kind: CredentialKind) -> Result<(), CredentialError> {
        if self.kind == kind {
            Ok(())
        } else {
            Err(CredentialError::WrongKind {
                expected: kind,
                found: self.kind,
            })
        }
    }

    pub fn require_node(&self, hostname: &str) -> Result<&NodeResourceSlice, CredentialError> {
        self.resources_by_node
            .iter()
            .find(|n| n.node == hostname)
            .ok_or(CredentialError::WrongNode)
    }

    pub fn require_run_attempt(&self, attempt: u32) -> Result<(), CredentialError> {
        if self.run_attempt == attempt {
            Ok(())
        } else {
            Err(CredentialError::WrongAttempt)
        }
    }

    pub fn require_unix(&self, uid: u32, gid: u32) -> Result<(), CredentialError> {
        if self.uid == uid && self.gid == gid {
            Ok(())
        } else {
            Err(CredentialError::IdentityMismatch)
        }
    }

    pub fn require_command_digest(&self, digest: &[u8; DIGEST_LEN]) -> Result<(), CredentialError> {
        if &self.command_digest == digest {
            Ok(())
        } else {
            Err(CredentialError::CommandDigestMismatch)
        }
    }

    pub fn require_slice(
        &self,
        hostname: &str,
        cpus: u32,
        memory_mb: u64,
        devices: &[(String, u32, u64)],
    ) -> Result<(), CredentialError> {
        let slice = self.require_node(hostname)?;
        if slice.cpus != cpus || slice.memory_mb != memory_mb {
            return Err(CredentialError::ResourceMismatch);
        }
        if slice.devices.len() != devices.len() {
            return Err(CredentialError::ResourceMismatch);
        }
        for (expected, got) in slice.devices.iter().zip(devices.iter()) {
            if expected.name != got.0 || expected.device_id != got.1 || expected.count != got.2 {
                return Err(CredentialError::ResourceMismatch);
            }
        }
        Ok(())
    }

    pub fn validate_time(&self, now_unix: u64, skew_secs: u64) -> Result<(), CredentialError> {
        let earliest = self.not_before.saturating_sub(skew_secs);
        if now_unix < earliest {
            return Err(CredentialError::NotYetValid);
        }
        check_issued_expires(self.issued_at, self.expires_at, now_unix, skew_secs)
    }

    fn canonical(&self) -> Self {
        let mut cred = self.clone();
        cred.supplementary_gids.sort_unstable();
        cred.supplementary_gids.dedup();
        for node in &mut cred.resources_by_node {
            node.devices.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then(a.device_id.cmp(&b.device_id))
                    .then(a.count.cmp(&b.count))
            });
        }
        cred.resources_by_node.sort_by(|a, b| a.node.cmp(&b.node));
        cred
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if !self.kind.is_execution() {
            return Err(CredentialError::Malformed("execution kind"));
        }
        require_nonempty("cluster_id", &self.cluster_id)?;
        require_nonempty("key_id", &self.key_id)?;
        require_nonempty("user", &self.user)?;
        if self.resources_by_node.is_empty() {
            return Err(CredentialError::Malformed("nodes"));
        }
        if self.resources_by_node.len() > MAX_NODES {
            return Err(CredentialError::Malformed("nodes"));
        }
        if self.supplementary_gids.len() > MAX_GIDS {
            return Err(CredentialError::Malformed("gids"));
        }
        let mut names = BTreeSet::new();
        for node in &self.resources_by_node {
            require_nonempty("node", &node.node)?;
            if !names.insert(node.node.as_str()) {
                return Err(CredentialError::Malformed("duplicate node"));
            }
            if node.devices.len() > MAX_DEVICES {
                return Err(CredentialError::Malformed("devices"));
            }
            for dev in &node.devices {
                require_nonempty("device", &dev.name)?;
            }
        }
        if self.expires_at <= self.issued_at || self.not_before > self.expires_at {
            return Err(CredentialError::InvalidLifetime);
        }
        Ok(())
    }
}

fn check_issued_expires(
    issued_at: u64,
    expires_at: u64,
    now_unix: u64,
    skew_secs: u64,
) -> Result<(), CredentialError> {
    if expires_at <= issued_at {
        return Err(CredentialError::InvalidLifetime);
    }
    if issued_at > now_unix.saturating_add(skew_secs) {
        return Err(CredentialError::IssuedInFuture);
    }
    if now_unix > expires_at.saturating_add(skew_secs) {
        return Err(CredentialError::Expired);
    }
    Ok(())
}

fn require_nonempty(what: &'static str, s: &str) -> Result<(), CredentialError> {
    if s.is_empty() {
        Err(CredentialError::Malformed(what))
    } else if s.len() > MAX_STR {
        Err(CredentialError::Malformed("field too long"))
    } else {
        Ok(())
    }
}

pub(crate) struct Writer(Vec<u8>);

impl Writer {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn magic(&mut self) {
        self.magic_tag(MAGIC);
    }

    fn magic_tag(&mut self, tag: &[u8; 8]) {
        self.0.extend_from_slice(tag);
    }

    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn fixed(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn str(&mut self, s: &str) -> Result<(), CredentialError> {
        if s.len() > MAX_STR {
            return Err(CredentialError::Malformed("field too long"));
        }
        let len =
            u32::try_from(s.len()).map_err(|_| CredentialError::Malformed("field too long"))?;
        self.u32(len);
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }

    fn blob(&mut self, bytes: &[u8]) -> Result<(), CredentialError> {
        if bytes.len() > MAX_BLOB {
            return Err(CredentialError::Malformed("blob too long"));
        }
        let len =
            u32::try_from(bytes.len()).map_err(|_| CredentialError::Malformed("blob too long"))?;
        self.u32(len);
        self.fixed(bytes);
        Ok(())
    }

    fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn magic(&mut self) -> Result<(), CredentialError> {
        self.expect_magic(MAGIC)
    }

    fn expect_magic(&mut self, tag: &[u8; 8]) -> Result<(), CredentialError> {
        let got = self.take(tag.len())?;
        if got != tag {
            return Err(CredentialError::Malformed("bad magic"));
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CredentialError> {
        if self.0.len() < n {
            return Err(CredentialError::Malformed("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, CredentialError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CredentialError> {
        let mut b = [0u8; 2];
        b.copy_from_slice(self.take(2)?);
        Ok(u16::from_be_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, CredentialError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_be_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, CredentialError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(b))
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], CredentialError> {
        let mut b = [0u8; N];
        b.copy_from_slice(self.take(N)?);
        Ok(b)
    }

    fn str(&mut self) -> Result<String, CredentialError> {
        let len = self.u32()? as usize;
        if len > MAX_STR {
            return Err(CredentialError::Malformed("field too long"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| CredentialError::Malformed("invalid utf-8"))
    }

    fn blob(&mut self) -> Result<Vec<u8>, CredentialError> {
        let len = self.u32()? as usize;
        if len > MAX_BLOB {
            return Err(CredentialError::Malformed("blob too long"));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), CredentialError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(CredentialError::Malformed("trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_rpc() -> UserRpcCredential {
        UserRpcCredential {
            cluster_id: "cluster-a".into(),
            issuer_host: "login01".into(),
            audience: "spurctld-1".into(),
            audience_epoch: 9,
            user: "alice".into(),
            uid: 1001,
            gid: 1001,
            issued_at: 1_000,
            expires_at: 1_030,
            nonce: [7; NONCE_LEN],
            key_id: "2026-09".into(),
        }
    }

    fn node(name: &str) -> NodeResourceSlice {
        NodeResourceSlice {
            node: name.into(),
            cpus: 8,
            memory_mb: 32_768,
            devices: vec![DeviceSlice {
                name: "gpu".into(),
                device_id: 1,
                count: 1,
            }],
        }
    }

    fn execution(kind: CredentialKind) -> ExecutionCredential {
        ExecutionCredential {
            kind,
            cluster_id: "cluster-a".into(),
            key_id: "exec-1".into(),
            job_id: 42,
            step_id: if kind == CredentialKind::Step { 3 } else { 0 },
            run_attempt: 1,
            user: "alice".into(),
            uid: 1001,
            gid: 1001,
            supplementary_gids: vec![1001, 27],
            account: "ml".into(),
            partition: "gpu".into(),
            qos: "normal".into(),
            resources_by_node: vec![node("gpu02"), node("gpu01")],
            command_digest: [3; DIGEST_LEN],
            container_digest: Some([4; DIGEST_LEN]),
            issued_at: 1_000,
            not_before: 1_000,
            expires_at: 2_000,
            credential_id: [9; CREDENTIAL_ID_LEN],
        }
    }

    #[test]
    fn user_rpc_round_trips() {
        let cred = user_rpc();
        let bytes = cred.to_signing_bytes().unwrap();
        assert_eq!(peek_kind(&bytes).unwrap(), CredentialKind::UserRpc);
        assert_eq!(UserRpcCredential::from_signing_bytes(&bytes).unwrap(), cred);
        let wire = payload_to_base64url(&bytes);
        let decoded = payload_from_base64url(&wire).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn execution_round_trips_and_canonicalizes_nodes() {
        let cred = execution(CredentialKind::Step);
        let bytes = cred.to_signing_bytes().unwrap();
        let back = ExecutionCredential::from_signing_bytes(&bytes).unwrap();
        assert_eq!(back.kind, CredentialKind::Step);
        assert_eq!(
            back.target_nodes().collect::<Vec<_>>(),
            vec!["gpu01", "gpu02"]
        );
        assert_eq!(back.supplementary_gids, vec![27, 1001]);
        assert_eq!(
            ExecutionCredential::from_signing_bytes(&bytes).unwrap(),
            back
        );
    }

    #[test]
    fn job_and_step_are_domain_separated() {
        let job = execution(CredentialKind::Job).to_signing_bytes().unwrap();
        let step = execution(CredentialKind::Step).to_signing_bytes().unwrap();
        assert_ne!(job, step);
        assert_eq!(peek_kind(&job).unwrap(), CredentialKind::Job);
        let job_cred = ExecutionCredential::from_signing_bytes(&job).unwrap();
        job_cred.require_kind(CredentialKind::Job).unwrap();
        assert_eq!(
            job_cred.require_kind(CredentialKind::Step),
            Err(CredentialError::WrongKind {
                expected: CredentialKind::Step,
                found: CredentialKind::Job,
            })
        );
    }

    #[test]
    fn user_rpc_bytes_are_not_execution() {
        let bytes = user_rpc().to_signing_bytes().unwrap();
        let err = ExecutionCredential::from_signing_bytes(&bytes).unwrap_err();
        assert_eq!(
            err,
            CredentialError::NotExecution {
                found: CredentialKind::UserRpc,
            }
        );
        let exec = execution(CredentialKind::Job).to_signing_bytes().unwrap();
        let err = UserRpcCredential::from_signing_bytes(&exec).unwrap_err();
        assert_eq!(
            err,
            CredentialError::WrongKind {
                expected: CredentialKind::UserRpc,
                found: CredentialKind::Job,
            }
        );
    }

    #[test]
    fn equal_claims_produce_identical_bytes() {
        let a = user_rpc().to_signing_bytes().unwrap();
        let b = user_rpc().to_signing_bytes().unwrap();
        assert_eq!(a, b);
        let mut other = user_rpc();
        other.nonce[0] = 8;
        assert_ne!(other.to_signing_bytes().unwrap(), a);
    }

    #[test]
    fn trailing_and_truncated_bytes_are_malformed() {
        let mut bytes = user_rpc().to_signing_bytes().unwrap();
        bytes.push(0);
        assert_eq!(
            UserRpcCredential::from_signing_bytes(&bytes).unwrap_err(),
            CredentialError::Malformed("trailing bytes")
        );
        bytes.pop();
        bytes.pop();
        assert_eq!(
            UserRpcCredential::from_signing_bytes(&bytes).unwrap_err(),
            CredentialError::Malformed("truncated")
        );
        bytes[..8].copy_from_slice(b"XXXXXXXX");
        assert_eq!(
            UserRpcCredential::from_signing_bytes(&bytes).unwrap_err(),
            CredentialError::Malformed("bad magic")
        );
    }

    #[test]
    fn unsupported_version_is_typed() {
        let mut bytes = user_rpc().to_signing_bytes().unwrap();
        bytes[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            UserRpcCredential::from_signing_bytes(&bytes).unwrap_err(),
            CredentialError::UnsupportedVersion(2)
        );
        assert_eq!(
            peek_kind(&bytes).unwrap_err(),
            CredentialError::UnsupportedVersion(2)
        );
    }

    #[test]
    fn protocol_context_is_authenticated() {
        let mut bytes = user_rpc().to_signing_bytes().unwrap();
        let needle = USER_RPC_CONTEXT.as_bytes();
        let pos = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        bytes[pos] = b'X';
        assert_eq!(
            UserRpcCredential::from_signing_bytes(&bytes).unwrap_err(),
            CredentialError::Malformed("protocol_context")
        );
    }

    #[test]
    fn empty_user_is_rejected() {
        let mut cred = user_rpc();
        cred.user.clear();
        assert_eq!(
            cred.to_signing_bytes().unwrap_err(),
            CredentialError::Malformed("user")
        );
    }

    #[test]
    fn audience_and_cluster_checks() {
        let cred = user_rpc();
        cred.require_cluster("cluster-a").unwrap();
        assert_eq!(
            cred.require_cluster("other"),
            Err(CredentialError::ClusterMismatch)
        );
        cred.require_audience("spurctld-1", 9).unwrap();
        assert_eq!(
            cred.require_audience("spurd-gpu01", 9),
            Err(CredentialError::AudienceMismatch)
        );
        assert_eq!(
            cred.require_audience("spurctld-1", 8),
            Err(CredentialError::AudienceEpochMismatch)
        );
    }

    #[test]
    fn user_rpc_clock_window() {
        let cred = user_rpc();
        cred.validate_time(1_010, 5).unwrap();
        assert_eq!(cred.validate_time(1_040, 5), Err(CredentialError::Expired));
        assert_eq!(
            cred.validate_time(990, 5),
            Err(CredentialError::IssuedInFuture)
        );
        cred.validate_time(997, 5).unwrap();
    }

    #[test]
    fn execution_not_before_and_lifetime() {
        let cred = execution(CredentialKind::Job);
        cred.validate_time(1_500, 0).unwrap();
        assert_eq!(
            cred.validate_time(999, 0),
            Err(CredentialError::NotYetValid)
        );
        let mut bad = cred.clone();
        bad.expires_at = bad.issued_at;
        assert_eq!(
            bad.to_signing_bytes().unwrap_err(),
            CredentialError::InvalidLifetime
        );
    }

    #[test]
    fn credential_status_code_splits_launch_failures() {
        assert_eq!(
            CredentialError::WrongNode.status_code(),
            CredentialStatusCode::FailedPrecondition
        );
        assert_eq!(
            CredentialError::IdentityMismatch.status_code(),
            CredentialStatusCode::PermissionDenied
        );
        assert_eq!(
            CredentialError::BadSignature.status_code(),
            CredentialStatusCode::Unauthenticated
        );
        assert_eq!(
            CredentialError::WrongKind {
                expected: CredentialKind::Job,
                found: CredentialKind::Step,
            }
            .status_code(),
            CredentialStatusCode::PermissionDenied
        );
    }

    #[test]
    fn execution_requires_nodes() {
        let mut cred = execution(CredentialKind::Job);
        cred.resources_by_node.clear();
        assert_eq!(
            cred.to_signing_bytes().unwrap_err(),
            CredentialError::Malformed("nodes")
        );
        cred.resources_by_node = vec![node("gpu01"), node("gpu01")];
        assert_eq!(
            cred.to_signing_bytes().unwrap_err(),
            CredentialError::Malformed("duplicate node")
        );
    }

    #[test]
    fn signed_token_round_trips() {
        let token = SignedToken {
            key_id: "2026-09".into(),
            payload: user_rpc().to_signing_bytes().unwrap(),
            signature: vec![9; 32],
        };
        let wire = token.to_base64url().unwrap();
        assert_eq!(SignedToken::from_base64url(&wire).unwrap(), token);
    }

    #[test]
    fn verification_error_variants_exist() {
        for err in [
            CredentialError::UnknownKeyId("k".into()),
            CredentialError::BadSignature,
            CredentialError::Replay,
        ] {
            assert!(!err.to_string().is_empty());
        }
    }
}
