// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Strict JWKS loading for native authentication (HMAC) and execution
//! credentials (Ed25519). Files are independent: an auth set is never used as
//! an execution set, and a verification set must not contain private keys.
//!
//! [`LiveSet::reload`] swaps an in-memory snapshot after a complete successful
//! parse. Daemons load JWKS once at startup via `from_path`; pick up a new file
//! by replacing it and restarting the process.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aws_lc_rs::hmac::{self, HMAC_SHA256};
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use thiserror::Error;

pub const AUTH_JWKS_PATH: &str = "/etc/spur/auth.jwks";
pub const CRED_SIGNING_JWKS_PATH: &str = "/etc/spur/cred-signing.jwks";
pub const CRED_VERIFICATION_JWKS_PATH: &str = "/etc/spur/cred-verification.jwks";
pub const NODE_SIGNING_JWKS_PATH: &str = "/etc/spur/node-signing.jwks";
pub const CONTROLLER_SIGNING_JWKS_PATH: &str = "/etc/spur/controller-signing.jwks";
pub const CONTROLLER_VERIFICATION_JWKS_PATH: &str = "/etc/spur/controller-verification.jwks";

const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_KEYS: usize = 64;
const HMAC_MIN_LEN: usize = 32;
const ED25519_LEN: usize = 32;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum JwksError {
    #[error("key set file not found: {0}")]
    NotFound(String),
    #[error("key set file is too large")]
    TooLarge,
    #[error("failed to read key set: {0}")]
    Read(String),
    #[error("JWKS JSON is invalid: {0}")]
    Json(String),
    #[error("JWKS has no keys")]
    Empty,
    #[error("JWKS has too many keys")]
    TooManyKeys,
    #[error("duplicate kid {0}")]
    DuplicateKid(String),
    #[error("JWKS has no default key")]
    NoDefault,
    #[error("JWKS has more than one default key")]
    MultipleDefaults,
    #[error("default key {0} is not usable at this time")]
    DefaultInactive(String),
    #[error("unknown algorithm {alg} on kid {kid}")]
    UnknownAlg { kid: String, alg: String },
    #[error("unsupported kty on kid {0}")]
    BadKty(String),
    #[error("unsupported curve on kid {0}")]
    BadCurve(String),
    #[error("malformed key material on kid {0}")]
    BadMaterial(String),
    #[error("HMAC key material must be at least 32 bytes (kid {0})")]
    KeyTooShort(String),
    #[error("unexpected private key in verification set (kid {0})")]
    PrivateKeyInVerifySet(String),
    #[error("missing private key in signing set (kid {0})")]
    MissingPrivateKey(String),
    #[error("public key does not match private key (kid {0})")]
    PublicMismatch(String),
    #[error("unknown use value on kid {0}")]
    BadUse(String),
    #[error("kid is empty")]
    EmptyKid,
    #[error(
        "SPUR_CLUSTER_NAME is required when SPUR_AUTH_PLUGIN=spur and no config file is loaded"
    )]
    MissingClusterName,
    #[error("unknown key id {0}")]
    UnknownKid(String),
    #[error("key {0} is not yet valid")]
    KeyNotYetValid(String),
    #[error("key {0} has expired")]
    KeyExpired(String),
    #[error("credential signature is invalid")]
    BadSignature,
    #[error("signing failed")]
    SignFailed,
}

#[derive(Deserialize)]
struct JwksDocument {
    keys: Vec<RawJwk>,
}

#[derive(Deserialize)]
struct RawJwk {
    alg: String,
    kty: String,
    kid: String,
    #[serde(default)]
    k: Option<String>,
    #[serde(default)]
    d: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(rename = "use", default)]
    key_use: Option<String>,
    #[serde(default)]
    not_before: Option<u64>,
    #[serde(default)]
    exp: Option<u64>,
}

#[derive(Clone)]
struct KeySchedule {
    kid: String,
    not_before: Option<u64>,
    exp: Option<u64>,
    is_default: bool,
}

impl KeySchedule {
    fn require_active(&self, now: u64) -> Result<(), JwksError> {
        if self.not_before.is_some_and(|nbf| now < nbf) {
            return Err(JwksError::KeyNotYetValid(self.kid.clone()));
        }
        if self.exp.is_some_and(|exp| now >= exp) {
            return Err(JwksError::KeyExpired(self.kid.clone()));
        }
        Ok(())
    }
}

/// HMAC-SHA256 key set for native user-RPC credentials (`auth.jwks`).
#[derive(Clone)]
pub struct HmacKeySet {
    keys: Vec<HmacKey>,
    default_idx: usize,
}

#[derive(Clone)]
struct HmacKey {
    schedule: KeySchedule,
    secret: Vec<u8>,
}

impl std::fmt::Debug for HmacKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacKeySet")
            .field("kids", &self.kids())
            .field("default", &self.default_kid())
            .finish()
    }
}

/// Controller-private Ed25519 set (`cred-signing.jwks`).
#[derive(Clone)]
pub struct Ed25519SigningKeySet {
    keys: Vec<Ed25519SignKey>,
    default_idx: usize,
}

#[derive(Clone)]
struct Ed25519SignKey {
    schedule: KeySchedule,
    seed: [u8; ED25519_LEN],
}

impl std::fmt::Debug for Ed25519SigningKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519SigningKeySet")
            .field("kids", &self.kids())
            .field("default", &self.default_kid())
            .finish()
    }
}

/// Agent-public Ed25519 set (`cred-verification.jwks`).
#[derive(Clone)]
pub struct Ed25519VerifyKeySet {
    keys: Vec<Ed25519VerifyKey>,
    default_idx: usize,
}

#[derive(Clone)]
struct Ed25519VerifyKey {
    schedule: KeySchedule,
    public: [u8; ED25519_LEN],
}

impl std::fmt::Debug for Ed25519VerifyKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519VerifyKeySet")
            .field("kids", &self.kids())
            .field("default", &self.default_kid())
            .finish()
    }
}

/// Process-wide snapshot that reloads atomically.
pub struct LiveSet<T> {
    path: PathBuf,
    inner: RwLock<Arc<T>>,
}

pub trait FromJwks: Sized + Send + Sync + 'static {
    fn parse(bytes: &[u8], now: u64) -> Result<Self, JwksError>;
}

impl FromJwks for HmacKeySet {
    fn parse(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        HmacKeySet::from_bytes(bytes, now)
    }
}

impl FromJwks for Ed25519SigningKeySet {
    fn parse(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        Ed25519SigningKeySet::from_bytes(bytes, now)
    }
}

impl FromJwks for Ed25519VerifyKeySet {
    fn parse(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        Ed25519VerifyKeySet::from_bytes(bytes, now)
    }
}

impl<T: FromJwks> LiveSet<T> {
    pub fn load(path: impl Into<PathBuf>, now: u64) -> Result<Self, JwksError> {
        let path = path.into();
        let bytes = read_jwks_file(&path)?;
        let set = T::parse(&bytes, now)?;
        Ok(Self {
            path,
            inner: RwLock::new(Arc::new(set)),
        })
    }

    pub fn reload(&self, now: u64) -> Result<(), JwksError> {
        let bytes = read_jwks_file(&self.path)?;
        let set = T::parse(&bytes, now)?;
        *write_lock(&self.inner) = Arc::new(set);
        Ok(())
    }

    pub fn snapshot(&self) -> Arc<T> {
        Arc::clone(&read_lock(&self.inner))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl HmacKeySet {
    pub fn from_path(path: &Path, now: u64) -> Result<Self, JwksError> {
        Self::from_bytes(&read_jwks_file(path)?, now)
    }

    pub fn from_bytes(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        let keys = parse_document(bytes)?
            .into_iter()
            .map(HmacKey::from_raw)
            .collect::<Result<Vec<_>, _>>()?;
        let default_idx = select_default(keys.iter().map(|k| &k.schedule), now)?;
        Ok(Self { keys, default_idx })
    }

    pub fn default_kid(&self) -> &str {
        &self.keys[self.default_idx].schedule.kid
    }

    pub fn kids(&self) -> Vec<&str> {
        self.keys.iter().map(|k| k.schedule.kid.as_str()).collect()
    }

    /// Sign with the current default key. Overlap keys verify; they do not mint.
    pub fn sign(&self, msg: &[u8], now: u64) -> Result<(String, Vec<u8>), JwksError> {
        let key = &self.keys[self.default_idx];
        key.schedule.require_active(now)?;
        let hmac_key = hmac::Key::new(HMAC_SHA256, &key.secret);
        let tag = hmac::sign(&hmac_key, msg);
        Ok((key.schedule.kid.clone(), tag.as_ref().to_vec()))
    }

    pub fn verify(&self, kid: &str, msg: &[u8], sig: &[u8], now: u64) -> Result<(), JwksError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.schedule.kid == kid)
            .ok_or_else(|| JwksError::UnknownKid(kid.to_string()))?;
        key.schedule.require_active(now)?;
        let hmac_key = hmac::Key::new(HMAC_SHA256, &key.secret);
        hmac::verify(&hmac_key, msg, sig).map_err(|_| JwksError::BadSignature)
    }
}

impl HmacKey {
    fn from_raw(raw: RawJwk) -> Result<Self, JwksError> {
        let schedule = schedule_from_raw(&raw)?;
        if raw.alg != "HS256" {
            return Err(JwksError::UnknownAlg {
                kid: schedule.kid,
                alg: raw.alg,
            });
        }
        if raw.kty != "oct" {
            return Err(JwksError::BadKty(schedule.kid));
        }
        let k = raw
            .k
            .as_deref()
            .ok_or_else(|| JwksError::BadMaterial(schedule.kid.clone()))?;
        let secret = decode_b64url(k).map_err(|_| JwksError::BadMaterial(schedule.kid.clone()))?;
        if secret.len() < HMAC_MIN_LEN {
            return Err(JwksError::KeyTooShort(schedule.kid));
        }
        Ok(Self { schedule, secret })
    }
}

impl Ed25519SigningKeySet {
    pub fn from_path(path: &Path, now: u64) -> Result<Self, JwksError> {
        Self::from_bytes(&read_jwks_file(path)?, now)
    }

    pub fn from_bytes(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        let keys = parse_document(bytes)?
            .into_iter()
            .map(Ed25519SignKey::from_raw)
            .collect::<Result<Vec<_>, _>>()?;
        let default_idx = select_default(keys.iter().map(|k| &k.schedule), now)?;
        Ok(Self { keys, default_idx })
    }

    pub fn default_kid(&self) -> &str {
        &self.keys[self.default_idx].schedule.kid
    }

    pub fn kids(&self) -> Vec<&str> {
        self.keys.iter().map(|k| k.schedule.kid.as_str()).collect()
    }

    pub fn sign(&self, msg: &[u8], now: u64) -> Result<(String, Vec<u8>), JwksError> {
        let key = &self.keys[self.default_idx];
        key.schedule.require_active(now)?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&key.seed)
            .map_err(|_| JwksError::BadMaterial(key.schedule.kid.clone()))?;
        let sig = pair.try_sign(msg).map_err(|_| JwksError::SignFailed)?;
        Ok((key.schedule.kid.clone(), sig.as_ref().to_vec()))
    }

    pub fn verify(&self, kid: &str, msg: &[u8], sig: &[u8], now: u64) -> Result<(), JwksError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.schedule.kid == kid)
            .ok_or_else(|| JwksError::UnknownKid(kid.to_string()))?;
        key.schedule.require_active(now)?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&key.seed)
            .map_err(|_| JwksError::BadMaterial(key.schedule.kid.clone()))?;
        let public = pair.public_key();
        UnparsedPublicKey::new(&ED25519, public.as_ref())
            .verify(msg, sig)
            .map_err(|_| JwksError::BadSignature)
    }
}

impl Ed25519SignKey {
    fn from_raw(raw: RawJwk) -> Result<Self, JwksError> {
        let schedule = require_ed25519(&raw)?;
        let d = raw
            .d
            .as_deref()
            .ok_or_else(|| JwksError::MissingPrivateKey(schedule.kid.clone()))?;
        let seed: [u8; ED25519_LEN] = decode_fixed(d, &schedule.kid)?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| JwksError::BadMaterial(schedule.kid.clone()))?;
        if let Some(x) = raw.x.as_deref() {
            let public: [u8; ED25519_LEN] = decode_fixed(x, &schedule.kid)?;
            if public.as_slice() != pair.public_key().as_ref() {
                return Err(JwksError::PublicMismatch(schedule.kid));
            }
        }
        Ok(Self { schedule, seed })
    }
}

impl Ed25519VerifyKeySet {
    pub fn from_path(path: &Path, now: u64) -> Result<Self, JwksError> {
        Self::from_bytes(&read_jwks_file(path)?, now)
    }

    pub fn from_bytes(bytes: &[u8], now: u64) -> Result<Self, JwksError> {
        let keys = parse_document(bytes)?
            .into_iter()
            .map(Ed25519VerifyKey::from_raw)
            .collect::<Result<Vec<_>, _>>()?;
        let default_idx = select_default(keys.iter().map(|k| &k.schedule), now)?;
        Ok(Self { keys, default_idx })
    }

    pub fn default_kid(&self) -> &str {
        &self.keys[self.default_idx].schedule.kid
    }

    pub fn kids(&self) -> Vec<&str> {
        self.keys.iter().map(|k| k.schedule.kid.as_str()).collect()
    }

    pub fn verify(&self, kid: &str, msg: &[u8], sig: &[u8], now: u64) -> Result<(), JwksError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.schedule.kid == kid)
            .ok_or_else(|| JwksError::UnknownKid(kid.to_string()))?;
        key.schedule.require_active(now)?;
        UnparsedPublicKey::new(&ED25519, key.public.as_slice())
            .verify(msg, sig)
            .map_err(|_| JwksError::BadSignature)
    }
}

impl Ed25519VerifyKey {
    fn from_raw(raw: RawJwk) -> Result<Self, JwksError> {
        let schedule = require_ed25519(&raw)?;
        if raw.d.as_ref().is_some_and(|d| !d.is_empty()) {
            return Err(JwksError::PrivateKeyInVerifySet(schedule.kid));
        }
        let x = raw
            .x
            .as_deref()
            .ok_or_else(|| JwksError::BadMaterial(schedule.kid.clone()))?;
        let public = decode_fixed(x, &schedule.kid)?;
        Ok(Self { schedule, public })
    }
}

fn require_ed25519(raw: &RawJwk) -> Result<KeySchedule, JwksError> {
    let schedule = schedule_from_raw(raw)?;
    if raw.alg != "EdDSA" {
        return Err(JwksError::UnknownAlg {
            kid: schedule.kid,
            alg: raw.alg.clone(),
        });
    }
    if raw.kty != "OKP" {
        return Err(JwksError::BadKty(schedule.kid));
    }
    match raw.crv.as_deref() {
        Some("Ed25519") => Ok(schedule),
        _ => Err(JwksError::BadCurve(schedule.kid)),
    }
}

fn schedule_from_raw(raw: &RawJwk) -> Result<KeySchedule, JwksError> {
    if raw.kid.is_empty() {
        return Err(JwksError::EmptyKid);
    }
    let is_default = match raw
        .key_use
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => false,
        Some("default") => true,
        Some(_) => return Err(JwksError::BadUse(raw.kid.clone())),
    };
    Ok(KeySchedule {
        kid: raw.kid.clone(),
        not_before: raw.not_before,
        exp: raw.exp,
        is_default,
    })
}

fn parse_document(bytes: &[u8]) -> Result<Vec<RawJwk>, JwksError> {
    let doc: JwksDocument =
        serde_json::from_slice(bytes).map_err(|e| JwksError::Json(e.to_string()))?;
    if doc.keys.is_empty() {
        return Err(JwksError::Empty);
    }
    if doc.keys.len() > MAX_KEYS {
        return Err(JwksError::TooManyKeys);
    }
    let mut seen = HashSet::new();
    for key in &doc.keys {
        if key.kid.is_empty() {
            return Err(JwksError::EmptyKid);
        }
        if !seen.insert(&key.kid) {
            return Err(JwksError::DuplicateKid(key.kid.clone()));
        }
    }
    Ok(doc.keys)
}

fn select_default<'a>(
    keys: impl Iterator<Item = &'a KeySchedule>,
    now: u64,
) -> Result<usize, JwksError> {
    let defaults: Vec<(usize, &KeySchedule)> =
        keys.enumerate().filter(|(_, k)| k.is_default).collect();
    match defaults.as_slice() {
        [] => Err(JwksError::NoDefault),
        [(_, k)] => k
            .require_active(now)
            .map_err(|_| JwksError::DefaultInactive(k.kid.clone()))
            .map(|()| defaults[0].0),
        _ => Err(JwksError::MultipleDefaults),
    }
}

fn decode_b64url(s: &str) -> Result<Vec<u8>, ()> {
    URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .or_else(|_| URL_SAFE.decode(s.as_bytes()))
        .map_err(|_| ())
}

fn decode_fixed<const N: usize>(s: &str, kid: &str) -> Result<[u8; N], JwksError> {
    let bytes = decode_b64url(s).map_err(|_| JwksError::BadMaterial(kid.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| JwksError::BadMaterial(kid.to_string()))
}

pub fn path_from_env_or(env: &str, default: &str) -> PathBuf {
    std::env::var(env)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

/// HMAC JWKS with one default key. Write mode 0600 on mint hosts.
pub fn generate_hmac_jwks(kid: &str) -> String {
    use rand::RngExt;
    let mut k = vec![0u8; HMAC_MIN_LEN];
    rand::rng().fill(&mut k[..]);
    serde_json::json!({
        "keys": [{
            "alg": "HS256",
            "kty": "oct",
            "kid": kid,
            "use": "default",
            "k": URL_SAFE_NO_PAD.encode(&k),
        }]
    })
    .to_string()
}

/// Ed25519 signing document plus the public-only verification document.
pub fn generate_ed25519_jwks(kid: &str) -> (String, String) {
    use rand::RngExt;
    let mut seed = [0u8; ED25519_LEN];
    rand::rng().fill(&mut seed);
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("ed25519 seed");
    let d = URL_SAFE_NO_PAD.encode(seed);
    let x = URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
    let signing = serde_json::json!({
        "keys": [{
            "alg": "EdDSA",
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": kid,
            "use": "default",
            "d": d,
            "x": x,
        }]
    })
    .to_string();
    let verification = serde_json::json!({
        "keys": [{
            "alg": "EdDSA",
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": kid,
            "use": "default",
            "x": x,
        }]
    })
    .to_string();
    (signing, verification)
}

fn read_jwks_file(path: &Path) -> Result<Vec<u8>, JwksError> {
    let meta = fs::metadata(path).map_err(|e| io_error(path, e))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(JwksError::TooLarge);
    }
    fs::read(path).map_err(|e| io_error(path, e))
}

fn io_error(path: &Path, err: std::io::Error) -> JwksError {
    if err.kind() == ErrorKind::NotFound {
        JwksError::NotFound(path.display().to_string())
    } else {
        JwksError::Read(err.to_string())
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_cred::UserRpcCredential;
    use serde_json::json;

    const NOW: u64 = 1_700_000_000;

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn hmac_json(keys: serde_json::Value) -> String {
        json!({ "keys": keys }).to_string()
    }

    fn hmac_key(kid: &str, secret: &[u8], use_default: bool) -> serde_json::Value {
        let mut v = json!({
            "alg": "HS256",
            "kty": "oct",
            "kid": kid,
            "k": b64(secret),
            "extra_ignored": true
        });
        if use_default {
            v["use"] = json!("default");
        }
        v
    }

    fn seed_and_public(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
        let seed = [seed_byte; 32];
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let mut public = [0u8; 32];
        public.copy_from_slice(pair.public_key().as_ref());
        (seed, public)
    }

    fn ed_sign_key(
        kid: &str,
        seed: &[u8],
        public: Option<&[u8]>,
        use_default: bool,
    ) -> serde_json::Value {
        let mut v = json!({
            "alg": "EdDSA",
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": kid,
            "d": b64(seed),
        });
        if let Some(x) = public {
            v["x"] = json!(b64(x));
        }
        if use_default {
            v["use"] = json!("default");
        }
        v
    }

    fn ed_verify_key(kid: &str, public: &[u8], use_default: bool) -> serde_json::Value {
        let mut v = json!({
            "alg": "EdDSA",
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": kid,
            "x": b64(public),
        });
        if use_default {
            v["use"] = json!("default");
        }
        v
    }

    fn user_bytes() -> Vec<u8> {
        UserRpcCredential {
            cluster_id: "cluster-a".into(),
            issuer_host: "login01".into(),
            audience: "spurctld-1".into(),
            audience_epoch: 1,
            user: "alice".into(),
            uid: 1001,
            gid: 1001,
            issued_at: NOW,
            expires_at: NOW + 30,
            nonce: [1; 16],
            key_id: "new".into(),
        }
        .to_signing_bytes()
        .unwrap()
    }

    #[test]
    fn hmac_overlap_signs_with_default_and_verifies_both() {
        let old = [0x11u8; 32];
        let new = [0x22u8; 32];
        let set = HmacKeySet::from_bytes(
            hmac_json(json!([
                hmac_key("old", &old, false),
                hmac_key("new", &new, true)
            ]))
            .as_bytes(),
            NOW,
        )
        .unwrap();
        assert_eq!(set.default_kid(), "new");
        let msg = user_bytes();
        let (kid, sig) = set.sign(&msg, NOW).unwrap();
        assert_eq!(kid, "new");
        set.verify("new", &msg, &sig, NOW).unwrap();
        let old_key = hmac::Key::new(HMAC_SHA256, &old);
        let old_sig = hmac::sign(&old_key, &msg);
        set.verify("old", &msg, old_sig.as_ref(), NOW).unwrap();
        assert_eq!(
            set.verify("missing", &msg, &sig, NOW).unwrap_err(),
            JwksError::UnknownKid("missing".into())
        );
    }

    #[test]
    fn hmac_rejects_short_key_duplicate_and_two_defaults() {
        let short = HmacKeySet::from_bytes(
            hmac_json(json!([hmac_key("k", &[1u8; 16], true)])).as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(short, JwksError::KeyTooShort("k".into()));

        let dup = HmacKeySet::from_bytes(
            hmac_json(json!([
                hmac_key("k", &[1u8; 32], true),
                hmac_key("k", &[2u8; 32], false)
            ]))
            .as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(dup, JwksError::DuplicateKid("k".into()));

        let two = HmacKeySet::from_bytes(
            hmac_json(json!([
                hmac_key("a", &[1u8; 32], true),
                hmac_key("b", &[2u8; 32], true)
            ]))
            .as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(two, JwksError::MultipleDefaults);

        let none = HmacKeySet::from_bytes(
            hmac_json(json!([hmac_key("a", &[1u8; 32], false)])).as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(none, JwksError::NoDefault);
    }

    #[test]
    fn hmac_does_not_load_ed25519_document() {
        let (seed, _) = seed_and_public(7);
        let err = HmacKeySet::from_bytes(
            json!({ "keys": [ed_sign_key("e1", &seed, None, true)] })
                .to_string()
                .as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            JwksError::UnknownAlg { .. } | JwksError::BadKty(_)
        ));
    }

    #[test]
    fn default_must_be_active_at_load() {
        let mut key = hmac_key("k", &[3u8; 32], true);
        key["exp"] = json!(NOW);
        let err = HmacKeySet::from_bytes(hmac_json(json!([key])).as_bytes(), NOW).unwrap_err();
        assert_eq!(err, JwksError::DefaultInactive("k".into()));

        let mut future = hmac_key("k", &[3u8; 32], true);
        future["not_before"] = json!(NOW + 10);
        let err = HmacKeySet::from_bytes(hmac_json(json!([future])).as_bytes(), NOW).unwrap_err();
        assert_eq!(err, JwksError::DefaultInactive("k".into()));
    }

    #[test]
    fn expired_overlap_key_loads_but_does_not_verify() {
        let mut old = hmac_key("old", &[1u8; 32], false);
        old["exp"] = json!(NOW);
        let new = hmac_key("new", &[2u8; 32], true);
        let set = HmacKeySet::from_bytes(hmac_json(json!([old, new])).as_bytes(), NOW).unwrap();
        let msg = b"msg";
        let (_, sig) = set.sign(msg, NOW).unwrap();
        set.verify("new", msg, &sig, NOW).unwrap();
        assert_eq!(
            set.verify("old", msg, &sig, NOW).unwrap_err(),
            JwksError::KeyExpired("old".into())
        );
    }

    #[test]
    fn live_reload_is_atomic_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.jwks");
        fs::write(&path, hmac_json(json!([hmac_key("a", &[9u8; 32], true)]))).unwrap();
        let live = LiveSet::<HmacKeySet>::load(&path, NOW).unwrap();
        assert_eq!(live.snapshot().default_kid(), "a");

        fs::write(&path, "{not json").unwrap();
        assert!(matches!(live.reload(NOW).unwrap_err(), JwksError::Json(_)));
        assert_eq!(live.snapshot().default_kid(), "a");

        fs::write(&path, hmac_json(json!([hmac_key("b", &[8u8; 32], true)]))).unwrap();
        live.reload(NOW).unwrap();
        assert_eq!(live.snapshot().default_kid(), "b");
    }

    #[test]
    fn missing_file_fails_closed() {
        let err = HmacKeySet::from_path(Path::new("/no/such/auth.jwks"), NOW).unwrap_err();
        assert!(matches!(err, JwksError::NotFound(_)));
    }

    #[test]
    fn ed25519_sign_set_verifies_on_public_set() {
        let (seed, public) = seed_and_public(5);
        let sign = Ed25519SigningKeySet::from_bytes(
            json!({ "keys": [ed_sign_key("e1", &seed, Some(&public), true)] })
                .to_string()
                .as_bytes(),
            NOW,
        )
        .unwrap();
        let verify = Ed25519VerifyKeySet::from_bytes(
            json!({ "keys": [ed_verify_key("e1", &public, true)] })
                .to_string()
                .as_bytes(),
            NOW,
        )
        .unwrap();
        let msg = user_bytes();
        let (kid, sig) = sign.sign(&msg, NOW).unwrap();
        assert_eq!(kid, "e1");
        sign.verify("e1", &msg, &sig, NOW).unwrap();
        verify.verify("e1", &msg, &sig, NOW).unwrap();
        assert_eq!(
            verify.verify("e1", &msg, &[0u8; 64], NOW).unwrap_err(),
            JwksError::BadSignature
        );
    }

    #[test]
    fn verification_set_rejects_private_key_material() {
        let (seed, public) = seed_and_public(9);
        let mut key = ed_verify_key("e1", &public, true);
        key["d"] = json!(b64(&seed));
        let err =
            Ed25519VerifyKeySet::from_bytes(json!({ "keys": [key] }).to_string().as_bytes(), NOW)
                .unwrap_err();
        assert_eq!(err, JwksError::PrivateKeyInVerifySet("e1".into()));
    }

    #[test]
    fn signing_set_rejects_mismatched_x_and_hmac_document() {
        let (seed, _) = seed_and_public(1);
        let (_, other) = seed_and_public(2);
        let err = Ed25519SigningKeySet::from_bytes(
            json!({ "keys": [ed_sign_key("e1", &seed, Some(&other), true)] })
                .to_string()
                .as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(err, JwksError::PublicMismatch("e1".into()));

        let err = Ed25519SigningKeySet::from_bytes(
            hmac_json(json!([hmac_key("h", &[1u8; 32], true)])).as_bytes(),
            NOW,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            JwksError::UnknownAlg { .. } | JwksError::BadKty(_)
        ));
    }

    #[test]
    fn unknown_use_and_empty_kid_are_rejected() {
        let mut key = hmac_key("k", &[1u8; 32], false);
        key["use"] = json!("sig");
        assert_eq!(
            HmacKeySet::from_bytes(hmac_json(json!([key])).as_bytes(), NOW).unwrap_err(),
            JwksError::BadUse("k".into())
        );
        let mut empty = hmac_key("k", &[1u8; 32], true);
        empty["kid"] = json!("");
        assert_eq!(
            HmacKeySet::from_bytes(hmac_json(json!([empty])).as_bytes(), NOW).unwrap_err(),
            JwksError::EmptyKid
        );
    }
}
