// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Administrative audit-log records for the accounting `txn` table.
//!
//! Pure, DB-agnostic types and helpers: the record built here is handed to the
//! notifier for a best-effort async write. Kept free of `sqlx`/`tonic` server
//! plumbing so the mapping and detail-serialization logic stays unit-testable.

use chrono::{DateTime, Utc};
use tonic::{Code, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnAction {
    Create,
    Update,
    Delete,
}

impl TxnAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnEntity {
    Reservation,
}

impl TxnEntity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reservation => "reservation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    Success,
    Denied,
    Error,
}

impl TxnOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

/// Where the action originated. `Api` covers any external RPC/CLI caller (a
/// gRPC server cannot tell the CLI from any other client); `System` is internal
/// controller maintenance (e.g. expired-reservation purge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnSource {
    Api,
    System,
}

impl TxnSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::System => "system",
        }
    }
}

/// One audit row, built at action time and written best-effort by the notifier.
#[derive(Clone, Debug)]
pub struct TxnRecord {
    pub ts: DateTime<Utc>,
    pub actor: String,
    pub actor_uid: Option<i64>,
    /// True only when a JWT identity was cryptographically verified. False for
    /// permissive/disabled anonymous callers (asserted, trust-on-wire) and for
    /// `System` rows (trusted by being internal, which `source` conveys).
    pub verified: bool,
    pub source: TxnSource,
    pub action: TxnAction,
    pub entity_type: TxnEntity,
    pub entity_name: String,
    pub outcome: TxnOutcome,
    pub details: String,
}

/// Classify a handler's final result into an audit outcome. Mapping off the
/// final `Status` (rather than a domain error) means handler-level
/// `invalid_argument` validation failures are captured too, not just
/// cluster-layer errors.
pub fn outcome_from_status(result: &Result<(), Status>) -> TxnOutcome {
    match result {
        Ok(()) => TxnOutcome::Success,
        Err(s) if s.code() == Code::PermissionDenied => TxnOutcome::Denied,
        Err(_) => TxnOutcome::Error,
    }
}

/// Details for a reservation create attempt (the requested parameters, pre
/// server-side normalization).
pub fn create_details(
    start_time: &str,
    duration_minutes: u32,
    nodes: &[String],
    accounts: &[String],
    users: &[String],
    flags: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "start_time": start_time,
        "duration_minutes": duration_minutes,
        "nodes": nodes,
        "accounts": accounts,
        "users": users,
        "flags": flags,
    })
}

/// Details for a reservation update attempt (the requested deltas).
#[allow(clippy::too_many_arguments)]
pub fn update_details(
    duration_minutes: u32,
    add_nodes: &[String],
    remove_nodes: &[String],
    add_users: &[String],
    remove_users: &[String],
    add_accounts: &[String],
    remove_accounts: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "duration_minutes": duration_minutes,
        "add_nodes": add_nodes,
        "remove_nodes": remove_nodes,
        "add_users": add_users,
        "remove_users": remove_users,
        "add_accounts": add_accounts,
        "remove_accounts": remove_accounts,
    })
}

/// Details for a reservation delete. `reason` is set for internal purges.
pub fn delete_details(reason: Option<&str>) -> serde_json::Value {
    match reason {
        Some(r) => serde_json::json!({ "reason": r }),
        None => serde_json::json!({}),
    }
}

/// Serialize a details object to the stored string, attaching the error message
/// for non-success outcomes.
pub fn finalize_details(mut base: serde_json::Value, error: Option<&str>) -> String {
    if let Some(err) = error {
        if let Some(obj) = base.as_object_mut() {
            obj.insert(
                "error".to_string(),
                serde_json::Value::String(err.to_string()),
            );
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_maps_status_codes() {
        assert_eq!(outcome_from_status(&Ok(())), TxnOutcome::Success);
        assert_eq!(
            outcome_from_status(&Err(Status::permission_denied("no"))),
            TxnOutcome::Denied
        );
        assert_eq!(
            outcome_from_status(&Err(Status::invalid_argument("bad"))),
            TxnOutcome::Error
        );
        assert_eq!(
            outcome_from_status(&Err(Status::not_found("gone"))),
            TxnOutcome::Error
        );
    }

    #[test]
    fn create_details_captures_requested_values() {
        let v = create_details(
            "now",
            60,
            &["n1".into(), "n2".into()],
            &["acct".into()],
            &["alice".into()],
            &["MAINT".into()],
        );
        assert_eq!(v["start_time"], "now");
        assert_eq!(v["duration_minutes"], 60);
        assert_eq!(v["nodes"], serde_json::json!(["n1", "n2"]));
        assert_eq!(v["users"], serde_json::json!(["alice"]));
        assert_eq!(v["flags"], serde_json::json!(["MAINT"]));
    }

    #[test]
    fn finalize_details_attaches_error_only_on_failure() {
        let base = delete_details(None);
        let ok = finalize_details(base.clone(), None);
        assert_eq!(ok, "{}");

        let failed = finalize_details(base, Some("user 'bob' cannot modify"));
        let parsed: serde_json::Value = serde_json::from_str(&failed).unwrap();
        assert_eq!(parsed["error"], "user 'bob' cannot modify");
    }

    #[test]
    fn enum_str_values_are_stable() {
        assert_eq!(TxnAction::Create.as_str(), "create");
        assert_eq!(TxnAction::Update.as_str(), "update");
        assert_eq!(TxnAction::Delete.as_str(), "delete");
        assert_eq!(TxnEntity::Reservation.as_str(), "reservation");
        assert_eq!(TxnOutcome::Success.as_str(), "success");
        assert_eq!(TxnOutcome::Denied.as_str(), "denied");
        assert_eq!(TxnOutcome::Error.as_str(), "error");
        assert_eq!(TxnSource::Api.as_str(), "api");
        assert_eq!(TxnSource::System.as_str(), "system");
    }
}
