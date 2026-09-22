// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`AuditLayer`] owns the `txn` write; handlers add only what a middleware
//! cannot see, via [`annotate`]. `record_txn` remains for non-RPC writes.

mod layer;
mod registry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) use layer::{AuditContext, AuditLayer, ControllerAudit};

/// What a handler adds to the record the layer is already building.
#[derive(Clone, Debug)]
pub(crate) struct Annotation {
    /// Name of the object acted on, rendered as Slurm's `Where`.
    pub target: String,
    /// Requested parameters, as asked for rather than as normalized.
    pub details: serde_json::Value,
    /// Used only when no verified identity exists (`permissive` with no token),
    /// where an asserted name still beats an empty actor.
    pub asserted_actor: Option<String>,
    /// Replaces the registry's fixed action. Needed by the upsert RPCs, which
    /// only learn from the write whether they created or modified the row.
    pub action: Option<crate::accounting::TxnAction>,
}

impl Annotation {
    pub(crate) fn new(target: &str, details: serde_json::Value) -> Self {
        Self {
            target: target.to_string(),
            details,
            asserted_actor: None,
            action: None,
        }
    }

    /// For the RPCs whose request carries the caller's own name.
    pub(crate) fn asserted_actor(mut self, actor: &str) -> Self {
        self.asserted_actor = Some(actor.to_string());
        self
    }

    pub(crate) fn action(mut self, action: crate::accounting::TxnAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// One request's shared state, written by the handler and read by the layer.
#[derive(Debug, Default)]
pub(crate) struct AuditSlot {
    /// A `Mutex` not a `OnceLock`: a handler may refine it after parsing.
    annotation: Mutex<Option<Annotation>>,
    executed_locally: AtomicBool,
}

impl AuditSlot {
    fn take(&self) -> Option<Annotation> {
        // A poisoned lock would mean a handler panicked mid-annotation. Audit
        // must not turn that into a second failure, so recover the value.
        match self.annotation.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    fn set(&self, annotation: Annotation) {
        match self.annotation.lock() {
            Ok(mut guard) => *guard = Some(annotation),
            Err(poisoned) => *poisoned.into_inner() = Some(annotation),
        }
    }

    pub(crate) fn executed_locally(&self) -> bool {
        self.executed_locally.load(Ordering::Relaxed)
    }

    fn mark_executed_locally(&self) {
        self.executed_locally.store(true, Ordering::Relaxed);
    }
}

/// Set by `check_leader` when this controller applies the action itself, so the
/// layer never samples leadership again and disagrees with it.
pub(crate) fn mark_executed_locally<T>(request: &tonic::Request<T>) {
    if let Some(slot) = request.extensions().get::<Arc<AuditSlot>>() {
        slot.mark_executed_locally();
    }
}

/// Read back what a handler deposited, for tests that drive a handler directly
/// rather than through the layer.
#[cfg(test)]
pub(crate) fn take_for_test(slot: &Arc<AuditSlot>) -> Option<Annotation> {
    slot.take()
}

/// Take the handle so a handler can annotate after `into_inner()` has consumed
/// the request. `None` when the layer is absent, as in handler unit tests.
pub(crate) fn slot<T>(request: &tonic::Request<T>) -> Option<Arc<AuditSlot>> {
    request.extensions().get::<Arc<AuditSlot>>().cloned()
}

/// Record which object this request acted on. A no-op without a handle, so a
/// handler need not know whether the layer is installed.
pub(crate) fn annotate(handle: &Option<Arc<AuditSlot>>, annotation: Annotation) {
    if let Some(slot) = handle {
        slot.set(annotation);
    }
}

/// Write the row the Tower layer would have, for a handler it cannot see. REST
/// dispatches into these handlers below the layer, so its mutations need this.
pub(crate) async fn recorded<Req, Resp, F, Fut>(
    context: &dyn AuditContext,
    method: &str,
    peer: Option<String>,
    mut request: tonic::Request<Req>,
    call: F,
) -> Result<Resp, tonic::Status>
where
    F: FnOnce(tonic::Request<Req>) -> Fut,
    Fut: std::future::Future<Output = Result<Resp, tonic::Status>>,
{
    let Some(registry::RpcClass::Mutating(m)) = registry::classify(method) else {
        return call(request).await;
    };

    let slot = Arc::new(AuditSlot::default());
    request.extensions_mut().insert(slot.clone());
    let identity = request
        .extensions()
        .get::<spur_core::auth::Identity>()
        .cloned();
    let verified = request
        .extensions()
        .get::<crate::auth_middleware::Verified>()
        .is_some();

    let result = call(request).await;

    // `check_leader` marks the slot when this node applied the action, so a
    // request the handler forwarded is recorded by the leader, not here.
    if layer::should_record(m.scope, slot.executed_locally()) {
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        let outcome = crate::accounting::txn::outcome_from_status(&status);
        let error = status.as_ref().err().map(|s| s.message().to_string());
        context.record(layer::build_record(
            m,
            layer::Caller {
                identity: identity.as_ref(),
                verified,
                peer,
                forwarded: false,
            },
            slot.take(),
            outcome,
            error.as_deref(),
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotate_is_a_noop_without_a_slot() {
        // Handlers called directly in unit tests have no layer above them.
        annotate(&None, Annotation::new("n1", serde_json::json!({})));
    }

    #[test]
    fn the_layer_reads_back_what_a_handler_deposited() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate(
            &handle,
            Annotation::new("n1", serde_json::json!({ "state": "drain" })),
        );

        let got = slot
            .take()
            .expect("annotation must be visible to the layer");
        assert_eq!(got.target, "n1");
        assert_eq!(got.details["state"], "drain");
    }

    #[test]
    fn a_later_annotation_replaces_an_earlier_one() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate(&handle, Annotation::new("n1", serde_json::json!({})));
        annotate(&handle, Annotation::new("n2", serde_json::json!({})));

        assert_eq!(slot.take().map(|a| a.target).as_deref(), Some("n2"));
    }

    #[test]
    fn an_asserted_actor_is_carried_only_when_set() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate(
            &handle,
            Annotation::new("daily", serde_json::json!({})).asserted_actor("alice"),
        );

        let got = slot.take().expect("annotation");
        assert_eq!(got.asserted_actor.as_deref(), Some("alice"));

        // Left unset, the layer has nothing to fall back to and records an empty
        // actor rather than inventing one.
        annotate(&handle, Annotation::new("n1", serde_json::json!({})));
        assert!(slot.take().expect("annotation").asserted_actor.is_none());
    }

    /// An upsert only learns its verb from the write, so the handler may replace
    /// the registry's fixed action.
    #[test]
    fn an_action_override_is_carried_when_set() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate(&handle, Annotation::new("gpu", serde_json::json!({})));
        assert_eq!(slot.take().expect("annotation").action, None);

        annotate(
            &handle,
            Annotation::new("gpu", serde_json::json!({}))
                .action(crate::accounting::TxnAction::Update),
        );
        assert_eq!(
            slot.take().expect("annotation").action,
            Some(crate::accounting::TxnAction::Update)
        );
    }

    #[test]
    fn taking_an_unannotated_slot_yields_nothing() {
        assert!(AuditSlot::default().take().is_none());
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<crate::accounting::TxnRecord>>);

    impl AuditContext for Recorder {
        fn record(&self, record: crate::accounting::TxnRecord) {
            self.0.lock().expect("recorder lock").push(record);
        }
    }

    impl Recorder {
        fn rows(&self) -> Vec<crate::accounting::TxnRecord> {
            self.0.lock().expect("recorder lock").clone()
        }
    }

    /// REST dispatches below the layer, so this is what keeps it audited.
    #[tokio::test]
    async fn recorded_writes_a_row_for_a_handler_the_layer_never_saw() {
        let sink = Recorder::default();
        let out = recorded(
            &sink,
            "CancelJob",
            Some("10.0.0.7:4433".into()),
            tonic::Request::new(()),
            |req| async move {
                // Stands in for the handler: names its target and, as
                // `check_leader` would on the leader, claims the work.
                mark_executed_locally(&req);
                annotate(&slot(&req), Annotation::new("7", serde_json::json!({})));
                Ok(())
            },
        )
        .await;

        assert!(out.is_ok());
        let rows = sink.rows();
        assert_eq!(rows.len(), 1, "one mutation, one row");
        assert_eq!(rows[0].entity_name, "7");
        assert_eq!(rows[0].peer_addr, "10.0.0.7:4433");
        assert_eq!(rows[0].outcome, crate::accounting::TxnOutcome::Success);
    }

    /// A denial is the case an operator most needs recorded.
    #[tokio::test]
    async fn recorded_keeps_the_row_when_the_handler_refuses() {
        let sink = Recorder::default();
        let out: Result<(), _> = recorded(
            &sink,
            "CancelJob",
            None,
            tonic::Request::new(()),
            |req| async move {
                mark_executed_locally(&req);
                annotate(&slot(&req), Annotation::new("7", serde_json::json!({})));
                Err(tonic::Status::permission_denied("not your job"))
            },
        )
        .await;

        assert!(out.is_err());
        let rows = sink.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, crate::accounting::TxnOutcome::Denied);
        assert!(rows[0].details.contains("not your job"));
    }

    /// A request the handler forwarded is applied and recorded by the leader, so
    /// recording it here too would double-count it.
    #[tokio::test]
    async fn recorded_writes_nothing_when_the_handler_forwarded() {
        let sink = Recorder::default();
        let out: Result<(), _> = recorded(
            &sink,
            "CancelJob",
            None,
            tonic::Request::new(()),
            // No `mark_executed_locally`: this node forwarded instead.
            |_req| async move { Ok(()) },
        )
        .await;

        assert!(out.is_ok());
        assert!(sink.rows().is_empty());
    }

    /// Reads must not be dragged into the txn log by the REST path either.
    #[tokio::test]
    async fn recorded_ignores_a_read() {
        let sink = Recorder::default();
        let out: Result<(), _> = recorded(
            &sink,
            "GetJobs",
            None,
            tonic::Request::new(()),
            |_req| async move { Ok(()) },
        )
        .await;

        assert!(out.is_ok());
        assert!(sink.rows().is_empty());
    }
}
