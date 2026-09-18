// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Unix-socket authentication mint.
//!
//! The minting service issues native user-RPC credentials from kernel peer credentials
//! (`SO_PEERCRED`). It ignores any identity the caller might try to send. The
//! username is resolved with NSS on this host and written into the signed
//! credential so a remote verifier does not look up the numeric UID.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::{Uid, User};
use rand::RngExt;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, warn};

use crate::native_cred::{SignedToken, UserRpcCredential, NONCE_LEN};
use crate::native_jwks::{HmacKeySet, JwksError};

pub const AUTH_SOCKET_ENV: &str = "SPUR_AUTH_SOCKET";
pub const DEFAULT_LIFETIME_SECS: u64 = 30;
pub const PROTOCOL_VERSION: u16 = 1;
pub const CLOCK_SKEW_SECS: u64 = 5;

const MINT_MAGIC: &[u8; 8] = b"SPURMNT1";
const OP_MINT: u8 = 1;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
const MAX_FRAME: usize = 8192;

#[derive(Debug, Error)]
pub enum MintError {
    #[error("credential mint is unavailable ({path}); start spurauthd or set SPUR_AUTH_SOCKET")]
    Unavailable { path: String },
    #[error("mint protocol error: {0}")]
    Protocol(&'static str),
    #[error("mint request failed: {0}")]
    Remote(String),
    #[error("no passwd entry for uid {0}")]
    UnknownUser(u32),
    #[error("invalid cluster name {0}")]
    BadCluster(String),
    #[error("audience is required")]
    EmptyAudience,
    #[error("credential lifetime must be greater than zero")]
    BadLifetime,
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error(transparent)]
    Credential(#[from] crate::native_cred::CredentialError),
    #[error(transparent)]
    Jwks(#[from] JwksError),
    #[error("mint I/O: {0}")]
    Io(String),
}

impl From<io::Error> for MintError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::NotFound {
            Self::Unavailable {
                path: err.to_string(),
            }
        } else {
            Self::Io(err.to_string())
        }
    }
}

/// Parameters for a running mint.
#[derive(Clone)]
pub struct CredentialMint {
    cluster_id: String,
    issuer_host: String,
    keys: Arc<HmacKeySet>,
    lifetime_secs: u64,
}

impl CredentialMint {
    pub fn new(
        cluster_id: impl Into<String>,
        keys: Arc<HmacKeySet>,
        lifetime_secs: u64,
    ) -> Result<Self, MintError> {
        let cluster_id = cluster_id.into();
        validate_cluster_name(&cluster_id)?;
        if lifetime_secs == 0 {
            return Err(MintError::BadLifetime);
        }
        Ok(Self {
            cluster_id,
            issuer_host: hostname(),
            keys,
            lifetime_secs,
        })
    }
}

/// `$SPUR_AUTH_SOCKET` if set, otherwise `/run/spur/<cluster>/auth.sock`.
pub fn resolve_socket_path(cluster_name: &str) -> Result<PathBuf, MintError> {
    match std::env::var(AUTH_SOCKET_ENV) {
        Ok(p) if !p.trim().is_empty() => Ok(PathBuf::from(p.trim())),
        _ => default_socket_path(cluster_name),
    }
}

pub fn default_socket_path(cluster_name: &str) -> Result<PathBuf, MintError> {
    validate_cluster_name(cluster_name)?;
    Ok(PathBuf::from(format!("/run/spur/{cluster_name}/auth.sock")))
}

pub fn validate_cluster_name(name: &str) -> Result<(), MintError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
        || name.contains('\\')
    {
        return Err(MintError::BadCluster(name.to_string()));
    }
    Ok(())
}

/// Bind a world-connectable socket. The JWKS file stays mode 0600; this socket is not a key.
pub async fn bind_socket(path: &Path) -> Result<UnixListener, MintError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, mint: Arc<CredentialMint>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let mint = Arc::clone(&mint);
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, &mint).await {
                        debug!(error = %err, "mint connection failed");
                    }
                });
            }
            Err(err) => {
                warn!(error = %err, "mint accept failed");
            }
        }
    }
}

/// Ask the local credential mint for a one-shot credential bound to `audience` / `epoch`.
pub async fn mint(socket: &Path, audience: &str, audience_epoch: u64) -> Result<String, MintError> {
    if audience.is_empty() {
        return Err(MintError::EmptyAudience);
    }
    let mut stream = UnixStream::connect(socket).await.map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound || err.kind() == io::ErrorKind::ConnectionRefused {
            MintError::Unavailable {
                path: socket.display().to_string(),
            }
        } else {
            MintError::Io(err.to_string())
        }
    })?;
    write_frame(&mut stream, &encode_mint_request(audience, audience_epoch)?).await?;
    let body = read_frame(&mut stream).await?;
    decode_mint_response(&body)
}

/// Blocking mint for sync callers (tonic interceptors cannot `.await`).
pub fn mint_blocking(
    socket: &Path,
    audience: &str,
    audience_epoch: u64,
) -> Result<String, MintError> {
    if audience.is_empty() {
        return Err(MintError::EmptyAudience);
    }
    let mut stream = std::os::unix::net::UnixStream::connect(socket).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound || err.kind() == io::ErrorKind::ConnectionRefused {
            MintError::Unavailable {
                path: socket.display().to_string(),
            }
        } else {
            MintError::Io(err.to_string())
        }
    })?;
    write_frame_sync(&mut stream, &encode_mint_request(audience, audience_epoch)?)?;
    let body = read_frame_sync(&mut stream)?;
    decode_mint_response(&body)
}

async fn handle_connection(mut stream: UnixStream, mint: &CredentialMint) -> Result<(), MintError> {
    let body = read_frame(&mut stream).await?;
    let reply = match serve_request(&stream, mint, &body) {
        Ok(token) => encode_ok(&token)?,
        Err(err) => encode_err(&err.to_string())?,
    };
    write_frame(&mut stream, &reply).await
}

fn serve_request(
    stream: &UnixStream,
    mint: &CredentialMint,
    body: &[u8],
) -> Result<String, MintError> {
    let (audience, epoch) = decode_mint_request(body)?;
    if audience.is_empty() {
        return Err(MintError::EmptyAudience);
    }
    let peer = getsockopt(stream, PeerCredentials)
        .map_err(|e| MintError::Io(format!("SO_PEERCRED: {e}")))?;
    let uid = peer.uid();
    let gid = peer.gid();
    debug!(pid = peer.pid(), uid, gid, "peer credentials");
    let user = lookup_username(uid)?;
    let now = unix_now()?;
    mint_for_peer(mint, &user, uid, gid, &audience, epoch, now)
}

fn mint_for_peer(
    mint: &CredentialMint,
    user: &str,
    uid: u32,
    gid: u32,
    audience: &str,
    audience_epoch: u64,
    now: u64,
) -> Result<String, MintError> {
    let nonce: [u8; NONCE_LEN] = rand::rng().random();
    let mut cred = UserRpcCredential {
        cluster_id: mint.cluster_id.clone(),
        issuer_host: mint.issuer_host.clone(),
        audience: audience.to_string(),
        audience_epoch,
        user: user.to_string(),
        uid,
        gid,
        issued_at: now,
        expires_at: now.saturating_add(mint.lifetime_secs),
        nonce,
        key_id: mint.keys.default_kid().to_string(),
    };
    let mut payload = cred.to_signing_bytes()?;
    let (kid, signature) = mint.keys.sign(&payload, now)?;
    if kid != cred.key_id {
        cred.key_id = kid.clone();
        payload = cred.to_signing_bytes()?;
        let (kid2, sig2) = mint.keys.sign(&payload, now)?;
        if kid2 != cred.key_id {
            return Err(MintError::Jwks(JwksError::NoDefault));
        }
        return SignedToken {
            key_id: kid2,
            payload,
            signature: sig2,
        }
        .to_base64url()
        .map_err(MintError::from);
    }
    SignedToken {
        key_id: kid,
        payload,
        signature,
    }
    .to_base64url()
    .map_err(MintError::from)
}

fn lookup_username(uid: u32) -> Result<String, MintError> {
    match User::from_uid(Uid::from_raw(uid)) {
        Ok(Some(u)) if !u.name.is_empty() => Ok(u.name),
        Ok(_) => Err(MintError::UnknownUser(uid)),
        Err(_) => Err(MintError::UnknownUser(uid)),
    }
}

pub fn unix_now() -> Result<u64, MintError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| MintError::Clock)
}

fn hostname() -> String {
    whoami::hostname().unwrap_or_else(|_| "localhost".into())
}

fn encode_mint_request(audience: &str, epoch: u64) -> Result<Vec<u8>, MintError> {
    let mut body = Vec::new();
    body.extend_from_slice(MINT_MAGIC);
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    body.push(OP_MINT);
    put_str(&mut body, audience)?;
    body.extend_from_slice(&epoch.to_be_bytes());
    Ok(body)
}

fn decode_mint_request(body: &[u8]) -> Result<(String, u64), MintError> {
    let mut r = body;
    expect_magic(&mut r)?;
    let version = take_u16(&mut r)?;
    if version != PROTOCOL_VERSION {
        return Err(MintError::Protocol("unsupported version"));
    }
    let op = take_u8(&mut r)?;
    if op != OP_MINT {
        return Err(MintError::Protocol("unknown op"));
    }
    let audience = take_str(&mut r)?;
    let epoch = take_u64(&mut r)?;
    if !r.is_empty() {
        return Err(MintError::Protocol("trailing bytes"));
    }
    Ok((audience, epoch))
}

fn encode_ok(token: &str) -> Result<Vec<u8>, MintError> {
    let mut body = Vec::new();
    body.extend_from_slice(MINT_MAGIC);
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    body.push(STATUS_OK);
    put_str(&mut body, token)?;
    Ok(body)
}

fn encode_err(message: &str) -> Result<Vec<u8>, MintError> {
    let mut body = Vec::new();
    body.extend_from_slice(MINT_MAGIC);
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    body.push(STATUS_ERR);
    put_str(&mut body, message)?;
    Ok(body)
}

fn decode_mint_response(body: &[u8]) -> Result<String, MintError> {
    let mut r = body;
    expect_magic(&mut r)?;
    let version = take_u16(&mut r)?;
    if version != PROTOCOL_VERSION {
        return Err(MintError::Protocol("unsupported version"));
    }
    let status = take_u8(&mut r)?;
    let text = take_str(&mut r)?;
    if !r.is_empty() {
        return Err(MintError::Protocol("trailing bytes"));
    }
    match status {
        STATUS_OK => Ok(text),
        STATUS_ERR => Err(MintError::Remote(text)),
        _ => Err(MintError::Protocol("unknown status")),
    }
}

fn expect_magic(buf: &mut &[u8]) -> Result<(), MintError> {
    if buf.len() < MINT_MAGIC.len() || &buf[..MINT_MAGIC.len()] != MINT_MAGIC {
        return Err(MintError::Protocol("bad magic"));
    }
    *buf = &buf[MINT_MAGIC.len()..];
    Ok(())
}

fn take_u8(buf: &mut &[u8]) -> Result<u8, MintError> {
    if buf.is_empty() {
        return Err(MintError::Protocol("truncated"));
    }
    let v = buf[0];
    *buf = &buf[1..];
    Ok(v)
}

fn take_u16(buf: &mut &[u8]) -> Result<u16, MintError> {
    if buf.len() < 2 {
        return Err(MintError::Protocol("truncated"));
    }
    let mut b = [0u8; 2];
    b.copy_from_slice(&buf[..2]);
    *buf = &buf[2..];
    Ok(u16::from_be_bytes(b))
}

fn take_u64(buf: &mut &[u8]) -> Result<u64, MintError> {
    if buf.len() < 8 {
        return Err(MintError::Protocol("truncated"));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[..8]);
    *buf = &buf[8..];
    Ok(u64::from_be_bytes(b))
}

fn take_str(buf: &mut &[u8]) -> Result<String, MintError> {
    if buf.len() < 4 {
        return Err(MintError::Protocol("truncated"));
    }
    let mut lb = [0u8; 4];
    lb.copy_from_slice(&buf[..4]);
    *buf = &buf[4..];
    let len = u32::from_be_bytes(lb) as usize;
    if len > MAX_FRAME {
        return Err(MintError::Protocol("field too long"));
    }
    if buf.len() < len {
        return Err(MintError::Protocol("truncated"));
    }
    let s = std::str::from_utf8(&buf[..len]).map_err(|_| MintError::Protocol("invalid utf-8"))?;
    let out = s.to_string();
    *buf = &buf[len..];
    Ok(out)
}

fn put_str(buf: &mut Vec<u8>, s: &str) -> Result<(), MintError> {
    if s.len() > MAX_FRAME {
        return Err(MintError::Protocol("field too long"));
    }
    let len = u32::try_from(s.len()).map_err(|_| MintError::Protocol("field too long"))?;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
    Ok(())
}

async fn write_frame(stream: &mut UnixStream, body: &[u8]) -> Result<(), MintError> {
    if body.len() > MAX_FRAME {
        return Err(MintError::Protocol("frame too long"));
    }
    let len = u32::try_from(body.len()).map_err(|_| MintError::Protocol("frame too long"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, MintError> {
    let mut lenb = [0u8; 4];
    stream.read_exact(&mut lenb).await?;
    let len = u32::from_be_bytes(lenb) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(MintError::Protocol("frame too long"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

fn write_frame_sync(stream: &mut impl Write, body: &[u8]) -> Result<(), MintError> {
    if body.len() > MAX_FRAME {
        return Err(MintError::Protocol("frame too long"));
    }
    let len = u32::try_from(body.len()).map_err(|_| MintError::Protocol("frame too long"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn read_frame_sync(stream: &mut impl Read) -> Result<Vec<u8>, MintError> {
    let mut lenb = [0u8; 4];
    stream.read_exact(&mut lenb)?;
    let len = u32::from_be_bytes(lenb) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(MintError::Protocol("frame too long"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn open_minted_with_skew(
    token: &str,
    keys: &HmacKeySet,
    now: u64,
    skew_secs: u64,
) -> Result<UserRpcCredential, MintError> {
    let signed = SignedToken::from_base64url(token)?;
    keys.verify(&signed.key_id, &signed.payload, &signed.signature, now)?;
    let cred = UserRpcCredential::from_signing_bytes(&signed.payload)?;
    if cred.key_id != signed.key_id {
        return Err(MintError::Protocol("key_id mismatch"));
    }
    cred.validate_time(now, skew_secs)?;
    Ok(cred)
}

/// Verify a user-RPC credential for this cluster. Does not look the UID up in NSS.
pub fn verify_user_rpc(
    token: &str,
    keys: &HmacKeySet,
    cluster_id: &str,
    now: u64,
    skew_secs: u64,
) -> Result<UserRpcCredential, MintError> {
    let cred = open_minted_with_skew(token, keys, now, skew_secs)?;
    cred.require_cluster(cluster_id)?;
    Ok(cred)
}

/// Decode a minted token and check its HMAC with the mint's key set.
pub fn open_minted(
    token: &str,
    keys: &HmacKeySet,
    now: u64,
) -> Result<UserRpcCredential, MintError> {
    open_minted_with_skew(token, keys, now, CLOCK_SKEW_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_jwks::HmacKeySet;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::json;

    fn hmac_set() -> HmacKeySet {
        let secret = [0x42u8; 32];
        let doc = json!({
            "keys": [{
                "alg": "HS256",
                "kty": "oct",
                "kid": "k1",
                "k": URL_SAFE_NO_PAD.encode(secret),
                "use": "default"
            }]
        });
        HmacKeySet::from_bytes(doc.to_string().as_bytes(), unix_now().unwrap()).unwrap()
    }

    #[test]
    fn socket_path_is_cluster_specific() {
        assert_eq!(
            default_socket_path("cluster-a").unwrap(),
            PathBuf::from("/run/spur/cluster-a/auth.sock")
        );
        assert!(default_socket_path("../etc").is_err());
        assert!(default_socket_path("a/b").is_err());
        assert!(default_socket_path("").is_err());
    }

    #[test]
    #[serial_test::serial]
    fn env_overrides_default_socket() {
        let _guard = EnvGuard;
        std::env::set_var(AUTH_SOCKET_ENV, "/tmp/custom.sock");
        assert_eq!(
            resolve_socket_path("cluster-a").unwrap(),
            PathBuf::from("/tmp/custom.sock")
        );
    }

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(AUTH_SOCKET_ENV);
        }
    }

    #[tokio::test]
    async fn mint_uses_kernel_peer_identity() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("auth.sock");
        let keys = Arc::new(hmac_set());
        let server = Arc::new(CredentialMint::new("cluster-a", Arc::clone(&keys), 30).unwrap());
        let listener = bind_socket(&sock).await.unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
        tokio::spawn(serve(listener, Arc::clone(&server)));

        let token = mint(&sock, "spurctld-1", 7).await.unwrap();
        let cred = open_minted(&token, &keys, unix_now().unwrap()).unwrap();
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let name = User::from_uid(Uid::from_raw(uid)).unwrap().unwrap().name;
        assert_eq!(cred.uid, uid);
        assert_eq!(cred.gid, gid);
        assert_eq!(cred.user, name);
        assert_eq!(cred.audience, "spurctld-1");
        assert_eq!(cred.audience_epoch, 7);
        assert_eq!(cred.cluster_id, "cluster-a");
        cred.require_audience("spurctld-1", 7).unwrap();

        let token2 = mint(&sock, "spurctld-1", 7).await.unwrap();
        assert_ne!(token, token2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_mint_uses_kernel_peer_identity() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("auth.sock");
        let keys = Arc::new(hmac_set());
        let server = Arc::new(CredentialMint::new("cluster-a", Arc::clone(&keys), 30).unwrap());
        let listener = bind_socket(&sock).await.unwrap();
        tokio::spawn(serve(listener, Arc::clone(&server)));

        let token = mint_blocking(&sock, "spurctld-1", 7).unwrap();
        let cred = open_minted(&token, &keys, unix_now().unwrap()).unwrap();
        assert_eq!(cred.audience, "spurctld-1");
        assert_eq!(cred.audience_epoch, 7);
        let token2 = mint_blocking(&sock, "spurctld-1", 7).unwrap();
        assert_ne!(token, token2);
    }

    #[tokio::test]
    async fn mint_rejects_empty_audience_and_missing_socket() {
        assert!(matches!(
            mint(Path::new("/no/such.sock"), "a", 1).await,
            Err(MintError::Unavailable { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("auth.sock");
        let server = Arc::new(CredentialMint::new("c", Arc::new(hmac_set()), 30).unwrap());
        let listener = bind_socket(&sock).await.unwrap();
        tokio::spawn(serve(listener, server));
        let err = mint(&sock, "", 1).await.unwrap_err();
        assert!(matches!(err, MintError::EmptyAudience));
    }
}
