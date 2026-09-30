// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tower middleware that authenticates controller RPC callers.
//!
//! This is the single verification point for the control plane. It runs as a Tower layer rather
//! than a per-service tonic interceptor deliberately: the layer wraps everything served on the port,
//! so the accounting service — which has no authorization of its own, and whose `add_user` takes an
//! `admin_level` — is covered by the same gate as the controller.
//!
//! On success a verified [`Identity`] is inserted into the request extensions; handlers read it
//! instead of trusting a client-supplied `user`/`caller` field. Nothing else in the pipeline may
//! insert an `Identity`, so a handler that finds one knows it was verified here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use http_body_util::{BodyExt, Full, Limited};
use tower::{Layer, Service};
use tracing::warn;

use spur_core::auth::{BearerAuth, BearerOutcome};
use spur_core::config::AuthMode;

/// gRPC length-prefixed message header: a 1-byte compression flag followed by a
/// 4-byte big-endian message length.
const GRPC_FRAME_HEADER_LEN: usize = 5;

/// Check a forwarded unary request body against the digest the follower signed.
///
/// The follower signs the SHA-256 of the exact message bytes it puts on the wire, so
/// the leader hashes the bytes it received rather than re-encoding a decoded message:
/// prost `map` fields have no canonical byte order, so a re-encode would not reproduce
/// the follower's digest. Forwarded RPCs are unary and uncompressed, so anything other
/// than a single plain frame is rejected.
fn forwarded_body_digest_matches(frame: &[u8], expected: &[u8; 32]) -> Result<(), String> {
    if frame.len() < GRPC_FRAME_HEADER_LEN {
        return Err("body is not a gRPC frame".into());
    }
    if frame[0] != 0 {
        return Err("body is compressed".into());
    }
    let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
    let message = &frame[GRPC_FRAME_HEADER_LEN..];
    if message.len() != len {
        return Err("body frame length mismatch".into());
    }
    if &spur_core::native_peer::request_digest(message) != expected {
        return Err("request digest".into());
    }
    Ok(())
}

/// Marks an `Identity` as credential-verified, which the audit log's `verified` column keys off.
/// A path deriving an identity without checking a credential must insert the `Identity` and not this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified;

#[derive(Clone)]
pub struct AuthLayer {
    inner: Arc<BearerAuth>,
    peer: Option<std::sync::Arc<spur_core::native_peer::PeerVerifier>>,
}

impl AuthLayer {
    #[allow(dead_code)]
    pub fn new(mode: AuthMode, jwt_key: &str) -> Self {
        Self::from_bearer(BearerAuth::jwt(mode, jwt_key.as_bytes()))
    }

    pub fn from_bearer(auth: BearerAuth) -> Self {
        Self {
            inner: Arc::new(auth),
            peer: None,
        }
    }

    pub fn with_peer(mut self, peer: std::sync::Arc<spur_core::native_peer::PeerVerifier>) -> Self {
        self.peer = Some(peer);
        self
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            config: self.inner.clone(),
            peer: self.peer.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    inner: S,
    config: Arc<BearerAuth>,
    peer: Option<std::sync::Arc<spur_core::native_peer::PeerVerifier>>,
}

/// The ruling itself lives in `spur_core::auth` so the controller and the agent cannot drift apart
/// on a security decision; this module only supplies the Tower plumbing.
fn decide(config: &BearerAuth, header: Option<&str>) -> BearerOutcome {
    config.authenticate(header, "pass a token (see `spur token user`)")
}

impl<S> Service<Request<tonic::body::Body>> for AuthMiddleware<S>
where
    S: Service<Request<tonic::body::Body>, Response = Response<tonic::body::Body>>
        + Clone
        + Send
        + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
{
    type Response = Response<tonic::body::Body>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: Request<tonic::body::Body>) -> Self::Future {
        let config = self.config.clone();
        if spur_core::auth::is_unauthenticated_auth_handshake(req.uri().path()) {
            let mut inner = self.inner.clone();
            return Box::pin(async move { inner.call(req).await.map_err(Into::into) });
        }
        // Handlers forward by signing this path, so it must come from the wire
        // rather than from the Rust request type, which `Empty` RPCs all share.
        let path = req.uri().path().to_owned();
        req.extensions_mut()
            .insert(spur_core::native_peer::RpcPath(path));

        let forwarded = req
            .headers()
            .get(spur_core::native_peer::FORWARDED_HEADER)
            .is_some();
        if forwarded {
            if let Some(peer) = self.peer.clone() {
                let env = req
                    .headers()
                    .get(spur_core::native_peer::IDENTITY_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let Some(env) = env else {
                    let resp = tonic::Status::unauthenticated(
                        "forwarded RPC missing signed identity envelope",
                    )
                    .into_http();
                    return Box::pin(async move { Ok(resp) });
                };
                let now = spur_core::native_mint::unix_now().unwrap_or(0);
                match peer.verify(&env, now) {
                    Ok((identity, binding)) => {
                        // Stops a captured `Empty`-bodied read from being
                        // replayed onto another RPC, such as Reconfigure.
                        let path = req.uri().path().to_owned();
                        if binding.action != path {
                            let resp = tonic::Status::unauthenticated(format!(
                                "forwarded identity is bound to {}, not {path}",
                                binding.action
                            ))
                            .into_http();
                            return Box::pin(async move { Ok(resp) });
                        }
                        let expected = binding.request_digest;
                        req.extensions_mut().insert(identity);
                        req.extensions_mut().insert(binding);
                        req.extensions_mut().insert(Verified);
                        let mut inner = self.inner.clone();
                        // The follower signed the digest of the bytes it sent. Verify against the
                        // received bytes here, once, rather than re-encoding a decoded body in every
                        // handler (prost maps have no canonical byte order, so a re-encode diverges).
                        return Box::pin(async move {
                            let (parts, body) = req.into_parts();
                            // Same ceiling the gRPC decoder would apply, plus the frame header, so a
                            // forwarded body costs no more memory here than in the handler.
                            let limit = spur_proto::MAX_GRPC_REQUEST_SIZE + GRPC_FRAME_HEADER_LEN;
                            let collected = match Limited::new(body, limit).collect().await {
                                Ok(buf) => buf.to_bytes(),
                                Err(_) => {
                                    return Ok(tonic::Status::unauthenticated(
                                        "forwarded request body exceeded the size limit or \
                                         could not be read",
                                    )
                                    .into_http());
                                }
                            };
                            if let Err(reason) =
                                forwarded_body_digest_matches(&collected, &expected)
                            {
                                return Ok(tonic::Status::unauthenticated(format!(
                                    "forwarded identity does not match this RPC: {reason}"
                                ))
                                .into_http());
                            }
                            let req = Request::from_parts(
                                parts,
                                tonic::body::Body::new(Full::new(collected)),
                            );
                            inner.call(req).await.map_err(Into::into)
                        });
                    }
                    Err(e) => {
                        let resp = tonic::Status::unauthenticated(format!(
                            "invalid forwarded identity: {e}"
                        ))
                        .into_http();
                        return Box::pin(async move { Ok(resp) });
                    }
                }
            }
        }
        let header = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        match decide(&config, header.as_deref()) {
            BearerOutcome::Authenticated(identity) => {
                req.extensions_mut().insert(*identity);
                req.extensions_mut().insert(Verified);
            }
            BearerOutcome::Anonymous => {
                if config.mode == AuthMode::Permissive {
                    // Name the caller so an operator rolling out credentials can see exactly who is
                    // still unauthenticated instead of guessing.
                    warn!(
                        path = %req.uri().path(),
                        "unauthenticated request accepted (auth.mode = permissive); \
                         the caller's asserted identity is being trusted"
                    );
                }
            }
            BearerOutcome::Reject(msg) => {
                // Audit runs inside this layer, so a rejection reaches neither
                // tier. Unconditional, as Slurm errors regardless of DebugFlags.
                warn!(
                    path = %req.uri().path(),
                    peer = crate::rpc_middleware::peer_addr(req.extensions()).as_deref().unwrap_or("-"),
                    reason = %msg,
                    "rejected an RPC with an invalid credential"
                );
                let resp = tonic::Status::unauthenticated(msg).into_http();
                return Box::pin(async move { Ok(resp) });
            }
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await.map_err(Into::into) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::auth::generate_token;

    fn cfg(mode: AuthMode, key: &str) -> BearerAuth {
        BearerAuth::jwt(mode, key.as_bytes())
    }

    fn token(key: &str) -> String {
        generate_token("alice", 1000, false, key.as_bytes(), 3600).unwrap()
    }

    #[test]
    fn required_rejects_a_missing_credential() {
        assert!(matches!(
            decide(&cfg(AuthMode::Required, "k"), None),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn permissive_allows_a_missing_credential() {
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "k"), None),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn a_valid_token_authenticates_and_carries_the_subject() {
        let t = token("k");
        let header = format!("Bearer {t}");
        match decide(&cfg(AuthMode::Required, "k"), Some(&header)) {
            BearerOutcome::Authenticated(id) => {
                assert_eq!(id.user, "alice");
                assert_eq!(id.uid, 1000);
                assert!(!id.is_admin);
            }
            _ => panic!("valid token must authenticate"),
        }
    }

    #[test]
    fn permissive_still_rejects_an_invalid_token() {
        // Permissive tolerates the absence of a credential, never a bad one — otherwise forging a
        // token would be strictly better for an attacker than sending none.
        let forged = format!("Bearer {}", token("attacker-key"));
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, "real-key"), Some(&forged)),
            BearerOutcome::Reject(_)
        ));
    }

    #[test]
    fn a_malformed_header_is_rejected_not_downgraded() {
        for h in ["", "Basic abc", "Bearer", "Bearer    "] {
            assert!(
                matches!(
                    decide(&cfg(AuthMode::Permissive, "k"), Some(h)),
                    BearerOutcome::Reject(_)
                ),
                "header {h:?} must be rejected"
            );
        }
    }

    #[test]
    fn disabled_ignores_even_a_valid_token() {
        let t = token("k");
        let header = format!("Bearer {t}");
        assert!(matches!(
            decide(&cfg(AuthMode::Disabled, "k"), Some(&header)),
            BearerOutcome::Anonymous
        ));
    }

    #[test]
    fn a_token_without_a_configured_key_is_rejected() {
        let header = format!("Bearer {}", token("k"));
        assert!(matches!(
            decide(&cfg(AuthMode::Permissive, ""), Some(&header)),
            BearerOutcome::Reject(_)
        ));
    }

    /// Counts calls, and captures the body it received, so a test can assert the
    /// inner service was reached with exactly the bytes the follower forwarded.
    #[derive(Clone, Default)]
    struct CountingInner {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        last_body: std::sync::Arc<std::sync::Mutex<Option<bytes::Bytes>>>,
    }

    impl Service<Request<tonic::body::Body>> for CountingInner {
        type Response = Response<tonic::body::Body>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<tonic::body::Body>) -> Self::Future {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let last_body = self.last_body.clone();
            Box::pin(async move {
                let bytes = req
                    .into_body()
                    .collect()
                    .await
                    .map(|b| b.to_bytes())
                    .unwrap_or_default();
                *last_body.lock().expect("body lock") = Some(bytes);
                Ok(Response::new(tonic::body::Body::default()))
            })
        }
    }

    /// A rejection short-circuits here, so nothing downstream — audit included —
    /// observes it. Hence the log line lives in this module.
    #[tokio::test]
    async fn a_rejected_credential_never_reaches_the_inner_service() {
        use tower::ServiceExt;

        let inner = CountingInner::default();
        let forged = format!("Bearer {}", token("attacker-key"));
        let req = Request::builder()
            .uri("/slurm.SlurmController/UpdateNode")
            .header(http::header::AUTHORIZATION, forged)
            .body(tonic::body::Body::empty())
            .expect("request");

        let response = AuthLayer::new(AuthMode::Required, "real-key")
            .layer(inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert_eq!(
            inner.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a forged credential must not reach the service"
        );
        let status = tonic::Status::from_header_map(response.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    use spur_core::native_jwks::Ed25519SigningKeySet;
    use spur_core::native_peer::PeerVerifier;

    fn peer_keys() -> std::sync::Arc<Ed25519SigningKeySet> {
        let (signing, _verify) =
            spur_core::native_jwks::generate_ed25519_jwks("peer1").expect("keys");
        std::sync::Arc::new(
            Ed25519SigningKeySet::from_bytes(signing.as_bytes(), 1_700_000_000)
                .expect("signing keys"),
        )
    }

    fn identity() -> spur_core::auth::Identity {
        spur_core::auth::Identity {
            user: "alice".into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
            trusted_unix: true,
        }
    }

    /// Wrap a protobuf message body in a single uncompressed gRPC length-prefixed frame.
    fn grpc_frame(message: &[u8]) -> bytes::Bytes {
        let mut buf = Vec::with_capacity(GRPC_FRAME_HEADER_LEN + message.len());
        buf.push(0);
        buf.extend_from_slice(&(message.len() as u32).to_be_bytes());
        buf.extend_from_slice(message);
        bytes::Bytes::from(buf)
    }

    const SUBMIT_PATH: &str = "/slurm.SlurmController/SubmitJob";

    /// A `SubmitJobRequest` whose spec carries many environment entries, the case that
    /// broke a re-encode digest because prost maps iterate in a random order.
    fn submit_with_env(entries: usize) -> Vec<u8> {
        use prost::Message;
        let environment = (0..entries)
            .map(|i| (format!("VAR_{i}"), format!("value_{i}")))
            .collect();
        let spec = spur_proto::proto::JobSpec {
            name: "job".into(),
            user: "alice".into(),
            environment,
            ..Default::default()
        };
        let req = spur_proto::proto::SubmitJobRequest { spec: Some(spec) };
        req.encode_to_vec()
    }

    /// One verifier shared by signing and verifying: `generate_ed25519_jwks` picks a
    /// fresh random key, so a signer and a verifier built separately would not match.
    fn verifier() -> std::sync::Arc<PeerVerifier> {
        std::sync::Arc::new(PeerVerifier::new("cluster-a", 2, peer_keys()))
    }

    /// The middleware verifies against the real clock, so envelopes are signed with it too.
    fn now() -> u64 {
        spur_core::native_mint::unix_now().expect("clock")
    }

    /// Sign an envelope over `message` for this verifier and frame `message` on the wire.
    fn forwarded_submit(peer: &PeerVerifier, message: &[u8]) -> Request<tonic::body::Body> {
        let digest = spur_core::native_peer::request_digest(message);
        let envelope = peer
            .sign(
                &identity(),
                peer.controller_id,
                1,
                SUBMIT_PATH,
                digest,
                now(),
            )
            .expect("sign envelope");
        Request::builder()
            .uri(SUBMIT_PATH)
            .header(spur_core::native_peer::FORWARDED_HEADER, "true")
            .header(spur_core::native_peer::IDENTITY_HEADER, envelope)
            .body(tonic::body::Body::new(Full::new(grpc_frame(message))))
            .expect("request")
    }

    fn peer_layer(
        peer: std::sync::Arc<PeerVerifier>,
        inner: CountingInner,
    ) -> AuthMiddleware<CountingInner> {
        AuthLayer::from_bearer(cfg(AuthMode::Permissive, "k"))
            .with_peer(peer)
            .layer(inner)
    }

    #[tokio::test]
    async fn a_forwarded_request_with_many_env_entries_reaches_the_service_unchanged() {
        use tower::ServiceExt;

        let peer = verifier();
        let message = submit_with_env(30);
        let inner = CountingInner::default();
        let req = forwarded_submit(&peer, &message);

        let response = peer_layer(peer, inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert!(
            tonic::Status::from_header_map(response.headers()).is_none(),
            "a matching digest must be accepted"
        );
        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let body = inner.last_body.lock().expect("body lock").clone().unwrap();
        assert_eq!(
            &body[..],
            &grpc_frame(&message)[..],
            "the service must receive the exact forwarded bytes"
        );
    }

    #[tokio::test]
    async fn a_flipped_body_byte_is_rejected_and_never_reaches_the_service() {
        use tower::ServiceExt;

        let peer = verifier();
        let message = submit_with_env(30);
        let digest = spur_core::native_peer::request_digest(&message);
        let envelope = peer
            .sign(
                &identity(),
                peer.controller_id,
                1,
                SUBMIT_PATH,
                digest,
                now(),
            )
            .expect("sign envelope");
        // Tamper with the body after the envelope is signed.
        let mut tampered = message.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let req = Request::builder()
            .uri(SUBMIT_PATH)
            .header(spur_core::native_peer::FORWARDED_HEADER, "true")
            .header(spur_core::native_peer::IDENTITY_HEADER, envelope)
            .body(tonic::body::Body::new(Full::new(grpc_frame(&tampered))))
            .expect("request");

        let inner = CountingInner::default();
        let response = peer_layer(peer, inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert_eq!(
            inner.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a tampered body must not reach the service"
        );
        let status = tonic::Status::from_header_map(response.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn a_compressed_forwarded_frame_is_rejected() {
        use tower::ServiceExt;

        let peer = verifier();
        let message = submit_with_env(3);
        let digest = spur_core::native_peer::request_digest(&message);
        let envelope = peer
            .sign(
                &identity(),
                peer.controller_id,
                1,
                SUBMIT_PATH,
                digest,
                now(),
            )
            .expect("sign envelope");
        // Set the compression flag the follower never sets.
        let mut frame = grpc_frame(&message).to_vec();
        frame[0] = 1;
        let req = Request::builder()
            .uri(SUBMIT_PATH)
            .header(spur_core::native_peer::FORWARDED_HEADER, "true")
            .header(spur_core::native_peer::IDENTITY_HEADER, envelope)
            .body(tonic::body::Body::new(Full::new(bytes::Bytes::from(frame))))
            .expect("request");

        let inner = CountingInner::default();
        let response = peer_layer(peer, inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let status = tonic::Status::from_header_map(response.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn a_forwarded_request_without_an_envelope_is_rejected() {
        use tower::ServiceExt;

        let req = Request::builder()
            .uri(SUBMIT_PATH)
            .header(spur_core::native_peer::FORWARDED_HEADER, "true")
            .body(tonic::body::Body::new(Full::new(grpc_frame(
                &submit_with_env(3),
            ))))
            .expect("request");

        let inner = CountingInner::default();
        let response = peer_layer(verifier(), inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let status = tonic::Status::from_header_map(response.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn an_envelope_bound_to_another_rpc_cannot_be_replayed() {
        use tower::ServiceExt;

        // Signed for GetRpcStats, replayed onto SubmitJob: the action check rejects it
        // before the body is even read, so a captured envelope cannot cross RPCs.
        let peer = verifier();
        let message = submit_with_env(3);
        let digest = spur_core::native_peer::request_digest(&message);
        let envelope = peer
            .sign(
                &identity(),
                peer.controller_id,
                1,
                "/slurm.SlurmController/GetRpcStats",
                digest,
                now(),
            )
            .expect("sign envelope");
        let req = Request::builder()
            .uri(SUBMIT_PATH)
            .header(spur_core::native_peer::FORWARDED_HEADER, "true")
            .header(spur_core::native_peer::IDENTITY_HEADER, envelope)
            .body(tonic::body::Body::new(Full::new(grpc_frame(&message))))
            .expect("request");

        let inner = CountingInner::default();
        let response = peer_layer(peer, inner.clone())
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let status = tonic::Status::from_header_map(response.headers()).expect("grpc-status");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn a_multi_frame_body_is_rejected() {
        let message = submit_with_env(3);
        let digest = spur_core::native_peer::request_digest(&message);
        // Two frames concatenated: length prefix of the first no longer covers the buffer.
        let mut two = grpc_frame(&message).to_vec();
        two.extend_from_slice(&grpc_frame(&message));
        assert!(forwarded_body_digest_matches(&two, &digest).is_err());
    }

    #[test]
    fn a_matching_single_frame_is_accepted() {
        let message = submit_with_env(3);
        let digest = spur_core::native_peer::request_digest(&message);
        forwarded_body_digest_matches(&grpc_frame(&message), &digest).expect("must match");
    }
}
