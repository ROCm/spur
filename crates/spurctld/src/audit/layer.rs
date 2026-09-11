// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tower middleware auditing controller RPCs. Added after `AuthLayer`, since
//! `.layer(a).layer(b)` stacks `b` innermost — so this sees auth's `Identity`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use chrono::Utc;
use http::{HeaderMap, Request, Response};
use tonic::{Code, Status};
use tower::{Layer, Service};
use tracing::{info, warn};

use spur_core::auth::Identity;

use super::registry::{self, AuditScope, Mutating, RpcClass};
use super::AuditSlot;
use crate::accounting::{txn, TxnOutcome, TxnRecord, TxnSource};
use crate::auth_middleware::Verified;
use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;
use crate::rpc_middleware::{grpc_operation_name, peer_addr};

/// Not `audit`, which already carries job-submit hook decisions; interleaving
/// the two would leave neither stream filterable.
const AUDIT_RPC_TARGET: &str = "audit_rpc";

/// Behind a trait so the middleware can be driven as a plain `Service` in
/// tests, without a `ClusterManager` or an elected Raft leader.
pub(crate) trait AuditContext: Send + Sync + 'static {
    /// Whether this controller applied the mutation, so a leader-scoped row is
    /// ours to write rather than the leader's.
    fn is_leader(&self) -> bool;
    fn record(&self, record: TxnRecord);
}

pub(crate) struct ControllerAudit {
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
}

impl ControllerAudit {
    pub(crate) fn new(cluster: Arc<ClusterManager>, raft: Arc<RaftHandle>) -> Self {
        Self { cluster, raft }
    }
}

impl AuditContext for ControllerAudit {
    fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }

    fn record(&self, record: TxnRecord) {
        self.cluster.record_txn(record);
    }
}

#[derive(Clone)]
pub(crate) struct AuditLayer {
    context: Arc<dyn AuditContext>,
    log_rpcs: bool,
}

impl AuditLayer {
    pub(crate) fn new(context: Arc<dyn AuditContext>, log_rpcs: bool) -> Self {
        Self { context, log_rpcs }
    }
}

impl<S> Layer<S> for AuditLayer {
    type Service = AuditMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuditMiddleware {
            inner,
            context: self.context.clone(),
            log_rpcs: self.log_rpcs,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AuditMiddleware<S> {
    inner: S,
    context: Arc<dyn AuditContext>,
    log_rpcs: bool,
}

impl<S, B> Service<Request<B>> for AuditMiddleware<S>
where
    S: Service<Request<B>, Response = Response<tonic::body::Body>> + Clone + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<tonic::body::Body>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let method = grpc_operation_name(req.uri().path());
        let class = registry::classify(&method);
        let identity = req.extensions().get::<Identity>().cloned();
        // The marker, not the identity's presence: a future path deriving an
        // identity without checking a credential must not record as verified.
        let verified = req.extensions().get::<Verified>().is_some();
        let peer = peer_addr(req.extensions());

        // Only mutating RPCs get a slot, so a read cannot annotate its way into
        // the log.
        let mutating = match class {
            Some(RpcClass::Mutating(m)) => Some(m),
            _ => None,
        };
        let slot = mutating.map(|_| {
            let slot = Arc::new(AuditSlot::default());
            req.extensions_mut().insert(slot.clone());
            slot
        });

        if class.is_none() {
            // Unreachable while the registry's completeness test passes, but
            // loud rather than silent since the gap it guards is an audit miss.
            warn!(
                method = %method,
                "RPC is missing from the audit registry; it is not being audited"
            );
        }

        let context = self.context.clone();
        let is_leader = context.is_leader();
        let log_rpcs = self.log_rpcs;
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let result = inner.call(req).await;

            let Ok(response) = &result else {
                // The service itself failed, so there is no gRPC status to read
                // and nothing was applied.
                return result.map_err(Into::into);
            };
            let (outcome, error) = outcome_of(response.headers());

            if log_rpcs {
                info!(
                    target: AUDIT_RPC_TARGET,
                    method = %method,
                    user = identity.as_ref().map_or("-", |i| i.user.as_str()),
                    uid = identity.as_ref().map(|i| i.uid),
                    peer = peer.as_deref().unwrap_or("-"),
                    outcome = outcome.as_str(),
                    "rpc"
                );
            }

            if let Some(m) = mutating {
                if should_record(m.scope, is_leader) {
                    let annotation = slot.and_then(|s| s.take());
                    if m.targeted && annotation.is_none() {
                        warn!(
                            method = %method,
                            "audited RPC recorded no target; its handler is missing an \
                             audit::annotate call"
                        );
                    }
                    let record = build_record(
                        m,
                        identity.as_ref(),
                        verified,
                        peer,
                        annotation,
                        outcome,
                        error.as_deref(),
                    );
                    context.record(record);
                }
            }

            result.map_err(Into::into)
        })
    }
}

/// A follower forwards every mutating controller RPC to the leader, so only the
/// leader may record it. Accounting bypasses Raft and records wherever it ran.
fn should_record(scope: AuditScope, is_leader: bool) -> bool {
    match scope {
        AuditScope::LeaderOnly => is_leader,
        AuditScope::Local => true,
    }
}

/// tonic puts a handler error in the response *headers* and success's
/// `grpc-status: 0` in the trailers, so headers alone suffice — no body buffer.
fn outcome_of(headers: &HeaderMap) -> (TxnOutcome, Option<String>) {
    match Status::from_header_map(headers) {
        None => (TxnOutcome::Success, None),
        Some(status) if status.code() == Code::Ok => (TxnOutcome::Success, None),
        Some(status) => {
            let message = status.message().to_string();
            let outcome = txn::outcome_from_status(&Err(status));
            (outcome, Some(message))
        }
    }
}

/// Assemble the row from the layer's half (who, where from, how it ended) and
/// the handler's half (which object, which parameters).
fn build_record(
    m: Mutating,
    identity: Option<&Identity>,
    verified: bool,
    peer: Option<String>,
    annotation: Option<super::Annotation>,
    outcome: TxnOutcome,
    error: Option<&str>,
) -> TxnRecord {
    let (target, details, asserted) = match annotation {
        Some(a) => (a.target, a.details, a.asserted_actor),
        None => (String::new(), serde_json::json!({}), None),
    };
    // An identity always wins over the wire, verified or not; the asserted name
    // is consulted only when there is no identity at all.
    let actor = match identity {
        Some(id) => id.user.clone(),
        None => asserted.unwrap_or_default(),
    };
    TxnRecord {
        ts: Utc::now(),
        actor,
        // Gated on verification, not presence, so a recorded uid is always one
        // a credential proved rather than one a host asserted.
        actor_uid: verified
            .then(|| identity.map(|id| i64::from(id.uid)))
            .flatten(),
        verified,
        peer_addr: peer.unwrap_or_default(),
        source: TxnSource::Api,
        action: m.action,
        entity_type: m.entity,
        entity_name: target,
        outcome,
        details: txn::finalize_details(details, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{TxnAction, TxnEntity};
    use crate::audit::Annotation;

    fn identity(user: &str, uid: u32) -> Identity {
        Identity {
            user: user.into(),
            uid,
            gid: uid,
            is_admin: false,
        }
    }

    fn node_update() -> Mutating {
        let Some(RpcClass::Mutating(m)) = registry::classify("UpdateNode") else {
            panic!("UpdateNode must be mutating");
        };
        m
    }

    fn annotation(target: &str, asserted: Option<&str>) -> Annotation {
        Annotation {
            target: target.into(),
            details: serde_json::json!({ "state": "drain" }),
            asserted_actor: asserted.map(str::to_owned),
        }
    }

    #[test]
    fn a_verified_identity_supplies_the_actor_and_uid() {
        let rec = build_record(
            node_update(),
            Some(&identity("alice", 1000)),
            true,
            Some("10.11.99.42:51234".into()),
            Some(annotation("n1", None)),
            TxnOutcome::Success,
            None,
        );

        assert_eq!(rec.actor, "alice");
        assert_eq!(rec.actor_uid, Some(1000));
        assert!(rec.verified);
        assert_eq!(rec.peer_addr, "10.11.99.42:51234");
        assert_eq!(rec.entity_type, TxnEntity::Node);
        assert_eq!(rec.action, TxnAction::Update);
        assert_eq!(rec.entity_name, "n1");
        assert_eq!(rec.source, TxnSource::Api);
    }

    #[test]
    fn a_credential_is_never_overridden_by_the_wire() {
        let rec = build_record(
            node_update(),
            Some(&identity("alice", 1000)),
            true,
            None,
            Some(annotation("n1", Some("root"))),
            TxnOutcome::Success,
            None,
        );
        assert_eq!(
            rec.actor, "alice",
            "the asserted name must lose to the verified one"
        );
        assert!(rec.verified);
    }

    #[test]
    fn an_unauthenticated_caller_falls_back_to_the_asserted_name() {
        // `permissive` with no token: an asserted actor still beats an empty one,
        // but must be marked unverified so it is never read as proof.
        let rec = build_record(
            node_update(),
            None,
            false,
            None,
            Some(annotation("daily", Some("bob"))),
            TxnOutcome::Denied,
            Some("user 'bob' cannot modify"),
        );
        assert_eq!(rec.actor, "bob");
        assert!(!rec.verified);
        assert_eq!(
            rec.actor_uid, None,
            "an unverified row must not carry a uid that could be read as root"
        );
        assert_eq!(rec.outcome, TxnOutcome::Denied);
        assert!(rec.details.contains("cannot modify"));
    }

    /// Guards a mechanism that does not exist yet: `auth.plugin = "none"` would
    /// derive an identity from the local UNIX user, which is not proof.
    #[test]
    fn an_unverified_identity_names_the_actor_but_claims_no_proof() {
        let rec = build_record(
            node_update(),
            Some(&identity("root", 0)),
            false,
            None,
            Some(annotation("n1", None)),
            TxnOutcome::Success,
            None,
        );

        assert_eq!(
            rec.actor, "root",
            "the derived name is still worth recording"
        );
        assert!(!rec.verified, "no credential was verified");
        assert_eq!(
            rec.actor_uid, None,
            "an unproven uid 0 must not be stored as though a credential proved it"
        );
    }

    #[test]
    fn a_uid_above_i32_max_is_not_wrapped_negative() {
        let rec = build_record(
            node_update(),
            Some(&identity("svc", 4_000_000_000)),
            true,
            None,
            Some(annotation("n1", None)),
            TxnOutcome::Success,
            None,
        );
        assert_eq!(rec.actor_uid, Some(4_000_000_000));
    }

    #[test]
    fn an_unannotated_mutation_still_records_the_action() {
        // The point of layer-owned writes: a handler that contributes nothing
        // cannot make the action disappear.
        let rec = build_record(
            node_update(),
            None,
            false,
            None,
            None,
            TxnOutcome::Error,
            Some("boom"),
        );

        assert_eq!(rec.entity_type, TxnEntity::Node);
        assert_eq!(rec.action, TxnAction::Update);
        assert_eq!(rec.entity_name, "");
        assert_eq!(rec.outcome, TxnOutcome::Error);
        let details: serde_json::Value = serde_json::from_str(&rec.details).expect("json details");
        assert_eq!(details["error"], "boom");
    }

    #[test]
    fn outcome_reads_success_from_an_absent_or_ok_status() {
        let (outcome, err) = outcome_of(&HeaderMap::new());
        assert_eq!(outcome, TxnOutcome::Success);
        assert!(err.is_none());

        let mut ok = HeaderMap::new();
        ok.insert("grpc-status", "0".parse().unwrap());
        assert_eq!(outcome_of(&ok).0, TxnOutcome::Success);
    }

    #[test]
    fn outcome_distinguishes_denied_from_other_failures() {
        // `into_http` is exactly how tonic renders a handler error, so this
        // asserts the real encoding rather than a guess at it.
        let denied =
            Status::permission_denied("update node requires cluster admin").into_http::<()>();
        let (outcome, err) = outcome_of(denied.headers());
        assert_eq!(outcome, TxnOutcome::Denied);
        assert_eq!(err.as_deref(), Some("update node requires cluster admin"));

        let invalid = Status::invalid_argument("invalid node state").into_http::<()>();
        assert_eq!(outcome_of(invalid.headers()).0, TxnOutcome::Error);

        let missing = Status::not_found("node n9 not found").into_http::<()>();
        assert_eq!(outcome_of(missing.headers()).0, TxnOutcome::Error);
    }

    #[test]
    fn leader_scope_suppresses_a_followers_duplicate_row() {
        // The follower forwarded the work to the leader, which records it there.
        assert!(should_record(AuditScope::LeaderOnly, true));
        assert!(!should_record(AuditScope::LeaderOnly, false));
        // Accounting has no leader, so a follower serving it must still record.
        assert!(should_record(AuditScope::Local, true));
        assert!(should_record(AuditScope::Local, false));
    }

    // Driving the middleware as a real Service: `build_record` tests alone miss
    // a break in the slot hand-off through extensions, which the design needs.

    use std::convert::Infallible;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Default)]
    struct RecordingContext {
        leader: bool,
        rows: Mutex<Vec<TxnRecord>>,
    }

    impl AuditContext for RecordingContext {
        fn is_leader(&self) -> bool {
            self.leader
        }

        fn record(&self, record: TxnRecord) {
            self.rows.lock().expect("rows lock").push(record);
        }
    }

    /// Inner service standing in for a tonic handler: optionally annotates the
    /// slot the layer put in extensions, then returns `status`.
    #[derive(Clone)]
    struct StubHandler {
        annotate_target: Option<&'static str>,
        status: Option<Code>,
    }

    impl Service<Request<()>> for StubHandler {
        type Response = Response<tonic::body::Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<()>) -> Self::Future {
            if let Some(target) = self.annotate_target {
                let slot = req
                    .extensions()
                    .get::<Arc<AuditSlot>>()
                    .cloned()
                    .expect("a mutating RPC must receive an audit slot");
                super::super::annotate(&Some(slot), target, serde_json::json!({ "k": "v" }));
            }
            let response = match self.status {
                Some(code) => Status::new(code, "denied by stub").into_http(),
                None => Response::new(tonic::body::Body::empty()),
            };
            std::future::ready(Ok(response))
        }
    }

    /// Drive one request through the layer and return the rows it recorded.
    async fn rows_for(
        method: &str,
        leader: bool,
        handler: StubHandler,
        identity: Option<Identity>,
    ) -> Vec<TxnRecord> {
        let context = Arc::new(RecordingContext {
            leader,
            rows: Mutex::new(Vec::new()),
        });
        let mut req = Request::builder()
            .uri(format!("/slurm.SlurmController/{method}"))
            .body(())
            .expect("request");
        // Mirrors `AuthLayer`, which inserts both together on a verified token.
        if let Some(id) = identity {
            req.extensions_mut().insert(id);
            req.extensions_mut().insert(Verified);
        }

        let layer = AuditLayer::new(context.clone() as Arc<dyn AuditContext>, false);
        layer
            .layer(handler)
            .oneshot(req)
            .await
            .expect("middleware must not fail");

        let rows = context.rows.lock().expect("rows lock").clone();
        rows
    }

    fn annotating(target: &'static str) -> StubHandler {
        StubHandler {
            annotate_target: Some(target),
            status: None,
        }
    }

    fn silent() -> StubHandler {
        StubHandler {
            annotate_target: None,
            status: None,
        }
    }

    #[tokio::test]
    async fn the_slot_survives_the_round_trip_to_the_handler() {
        let rows = rows_for(
            "UpdateNode",
            true,
            annotating("n1"),
            Some(identity("alice", 1000)),
        )
        .await;

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].entity_name, "n1",
            "the handler's annotation must reach the layer"
        );
        assert_eq!(rows[0].actor, "alice");
        assert_eq!(rows[0].outcome, TxnOutcome::Success);
        assert_eq!(rows[0].entity_type, TxnEntity::Node);
        assert!(
            rows[0].verified,
            "the Verified marker must reach the record"
        );
        assert_eq!(rows[0].actor_uid, Some(1000));
    }

    #[tokio::test]
    async fn a_handler_error_is_read_off_the_response_as_the_outcome() {
        let handler = StubHandler {
            annotate_target: Some("n1"),
            status: Some(Code::PermissionDenied),
        };
        let rows = rows_for("UpdateNode", true, handler, None).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, TxnOutcome::Denied);
        assert!(rows[0].details.contains("denied by stub"));
    }

    #[tokio::test]
    async fn a_mutating_handler_that_forgets_to_annotate_still_produces_a_row() {
        let rows = rows_for("UpdateNode", true, silent(), Some(identity("alice", 1000))).await;

        assert_eq!(rows.len(), 1, "the action must not vanish from the log");
        assert_eq!(rows[0].entity_name, "");
        assert_eq!(rows[0].action, TxnAction::Update);
    }

    #[tokio::test]
    async fn reads_and_daemon_traffic_write_nothing() {
        for method in ["GetNodes", "Heartbeat", "Ping", "GetTransactions"] {
            let rows = rows_for(method, true, silent(), Some(identity("alice", 1000))).await;
            assert!(rows.is_empty(), "{method} must not be recorded");
        }
    }

    #[tokio::test]
    async fn a_follower_records_nothing_for_a_leader_scoped_rpc() {
        // It forwarded the work, so the leader writes the row.
        let rows = rows_for(
            "UpdateNode",
            false,
            annotating("n1"),
            Some(identity("alice", 1000)),
        )
        .await;
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn a_follower_still_records_an_accounting_mutation() {
        // Accounting bypasses Raft, so gating on leadership would lose the row.
        let rows = rows_for(
            "CreateAccount",
            false,
            silent(),
            Some(identity("alice", 1000)),
        )
        .await;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_type, TxnEntity::Account);
    }

    #[tokio::test]
    async fn an_unclassified_method_is_not_recorded() {
        let rows = rows_for("NoSuchRpc", true, silent(), Some(identity("alice", 1000))).await;
        assert!(rows.is_empty());
    }
}
