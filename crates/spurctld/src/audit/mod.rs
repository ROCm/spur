// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`AuditLayer`] owns the `txn` write; handlers add only what a middleware
//! cannot see, via [`annotate`]. `record_txn` remains for non-RPC writes.

mod layer;
mod registry;

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
}

/// One request's annotation, shared between the handler and the layer. A
/// `Mutex` not a `OnceLock`: a handler may refine it after parsing the request.
#[derive(Debug, Default)]
pub(crate) struct AuditSlot(Mutex<Option<Annotation>>);

impl AuditSlot {
    fn take(&self) -> Option<Annotation> {
        // A poisoned lock would mean a handler panicked mid-annotation. Audit
        // must not turn that into a second failure, so recover the value.
        match self.0.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    fn set(&self, annotation: Annotation) {
        match self.0.lock() {
            Ok(mut guard) => *guard = Some(annotation),
            Err(poisoned) => *poisoned.into_inner() = Some(annotation),
        }
    }
}

/// Take the handle so a handler can annotate after `into_inner()` has consumed
/// the request. `None` when the layer is absent, as in handler unit tests.
pub(crate) fn slot<T>(request: &tonic::Request<T>) -> Option<Arc<AuditSlot>> {
    request.extensions().get::<Arc<AuditSlot>>().cloned()
}

/// Record which object this request acted on. A no-op without a handle, so a
/// handler need not know whether the layer is installed.
pub(crate) fn annotate(handle: &Option<Arc<AuditSlot>>, target: &str, details: serde_json::Value) {
    set(handle, target, details, None);
}

/// [`annotate`] for the RPCs whose request carries a caller name.
pub(crate) fn annotate_as(
    handle: &Option<Arc<AuditSlot>>,
    actor: &str,
    target: &str,
    details: serde_json::Value,
) {
    set(handle, target, details, Some(actor.to_string()));
}

fn set(
    handle: &Option<Arc<AuditSlot>>,
    target: &str,
    details: serde_json::Value,
    asserted_actor: Option<String>,
) {
    if let Some(slot) = handle {
        slot.set(Annotation {
            target: target.to_string(),
            details,
            asserted_actor,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotate_is_a_noop_without_a_slot() {
        // Handlers called directly in unit tests have no layer above them.
        annotate(&None, "n1", serde_json::json!({}));
    }

    #[test]
    fn the_layer_reads_back_what_a_handler_deposited() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate(&handle, "n1", serde_json::json!({ "state": "drain" }));

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

        annotate(&handle, "n1", serde_json::json!({}));
        annotate(&handle, "n2", serde_json::json!({}));

        assert_eq!(slot.take().map(|a| a.target).as_deref(), Some("n2"));
    }

    #[test]
    fn annotate_as_carries_the_asserted_actor() {
        let slot = Arc::new(AuditSlot::default());
        let handle = Some(slot.clone());

        annotate_as(&handle, "alice", "daily", serde_json::json!({}));

        let got = slot.take().expect("annotation");
        assert_eq!(got.asserted_actor.as_deref(), Some("alice"));

        // The plain form leaves it unset, so the layer has nothing to fall back
        // to and records an empty actor rather than inventing one.
        annotate(&handle, "n1", serde_json::json!({}));
        assert!(slot.take().expect("annotation").asserted_actor.is_none());
    }

    #[test]
    fn taking_an_unannotated_slot_yields_nothing() {
        assert!(AuditSlot::default().take().is_none());
    }
}
