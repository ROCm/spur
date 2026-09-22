// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`AuditLayer`] owns the `txn` write; handlers add only what a middleware
//! cannot see, via [`annotate`]. `record_txn` remains for non-RPC writes.

mod layer;
mod registry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) use layer::{AuditLayer, ControllerAudit};

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
}
