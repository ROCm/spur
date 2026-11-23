// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Workload-reported checkpoint events: the wire format a job sends to its
//! step's socket, and the fixed-size per-step record those events update.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Task environment variable naming the step's checkpoint socket.
pub const CHECKPOINT_SOCK_ENV: &str = "SPUR_CHECKPOINT_SOCK";

/// Longest accepted event line, excluding the trailing newline.
pub const MAX_EVENT_BYTES: usize = 512;

const WIRE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Begin,
    End,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndStatus {
    Ok,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointEvent {
    pub kind: EventKind,
    pub step: u64,
    /// Set only for [`EventKind::End`].
    pub status: Option<EndStatus>,
    pub job_id: u32,
    /// From `$SPUR_STEP_ID`. Absent for senders that have no step ID of their
    /// own (e.g. a plain `sbatch` batch step), in which case the listener
    /// skips the step cross-check and relies on the socket being per-step and
    /// the cgroup attributing the sender.
    pub step_id: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    NotJobProcess,
    JobMismatch,
    UidMismatch,
    UnknownKind,
    UnsupportedVersion,
    InvalidStatus,
    UnknownField,
    InvalidField,
    MalformedJson,
    Oversize,
    ReadTimeout,
    RateLimited,
    Busy,
}

impl DropReason {
    pub const ALL: [DropReason; 13] = [
        DropReason::NotJobProcess,
        DropReason::JobMismatch,
        DropReason::UidMismatch,
        DropReason::UnknownKind,
        DropReason::UnsupportedVersion,
        DropReason::InvalidStatus,
        DropReason::UnknownField,
        DropReason::InvalidField,
        DropReason::MalformedJson,
        DropReason::Oversize,
        DropReason::ReadTimeout,
        DropReason::RateLimited,
        DropReason::Busy,
    ];

    /// Stable name, used as a metric label and as the `checkpoint.json` key.
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::NotJobProcess => "not_job_process",
            DropReason::JobMismatch => "job_mismatch",
            DropReason::UidMismatch => "uid_mismatch",
            DropReason::UnknownKind => "unknown_kind",
            DropReason::UnsupportedVersion => "unsupported_version",
            DropReason::InvalidStatus => "invalid_status",
            DropReason::UnknownField => "unknown_field",
            DropReason::InvalidField => "invalid_field",
            DropReason::MalformedJson => "malformed_json",
            DropReason::Oversize => "oversize",
            DropReason::ReadTimeout => "read_timeout",
            DropReason::RateLimited => "rate_limited",
            DropReason::Busy => "busy",
        }
    }

    /// The inverse of [`Self::as_str`]: `None` for a name this version does not recognize.
    pub fn parse(name: &str) -> Option<DropReason> {
        DropReason::ALL
            .into_iter()
            .find(|reason| reason.as_str() == name)
    }

    /// Whether this reason means the event itself was malformed or misattributed to the
    /// wrong job (`spur_checkpoint_events_invalid_total`), as opposed to a well-formed event
    /// that could not be delivered or accepted (`spur_checkpoint_events_dropped_total`).
    pub fn is_invalid(self) -> bool {
        matches!(
            self,
            DropReason::JobMismatch
                | DropReason::UnknownKind
                | DropReason::UnsupportedVersion
                | DropReason::InvalidStatus
                | DropReason::UnknownField
                | DropReason::InvalidField
                | DropReason::MalformedJson
                | DropReason::Oversize
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEvent {
    v: u32,
    kind: String,
    step: u64,
    #[serde(default)]
    status: Option<String>,
    job_id: String,
    #[serde(default)]
    step_id: Option<String>,
}

/// Parse one event line. A trailing newline is ignored; anything outside the
/// v1 schema is rejected whole, so a job can never smuggle extra data in.
/// `step_id` is optional on the wire: absent is accepted, but a present value
/// must still parse as `u32` or the whole line is `Schema`.
pub fn parse_event(line: &[u8]) -> Result<CheckpointEvent, DropReason> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.len() > MAX_EVENT_BYTES {
        return Err(DropReason::Oversize);
    }
    // serde's derived struct deserializer also accepts a JSON array.
    if line.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(DropReason::MalformedJson);
    }
    let wire: WireEvent =
        serde_json::from_slice(line).map_err(|error| classify_json_error(&error))?;
    if wire.v != WIRE_VERSION {
        return Err(DropReason::UnsupportedVersion);
    }
    let kind = match wire.kind.as_str() {
        "checkpoint-begin" => EventKind::Begin,
        "checkpoint-end" => EventKind::End,
        "restore" => EventKind::Restore,
        _ => return Err(DropReason::UnknownKind),
    };
    let status = match (kind, wire.status.as_deref()) {
        (EventKind::End, Some("ok")) => Some(EndStatus::Ok),
        (EventKind::End, Some("failed")) => Some(EndStatus::Failed),
        (EventKind::Begin | EventKind::Restore, None) => None,
        _ => return Err(DropReason::InvalidStatus),
    };
    let step_id = wire.step_id.as_deref().map(parse_id).transpose()?;
    Ok(CheckpointEvent {
        kind,
        step: wire.step,
        status,
        job_id: parse_id(&wire.job_id)?,
        step_id,
    })
}

fn parse_id(id: &str) -> Result<u32, DropReason> {
    id.parse().map_err(|_| DropReason::InvalidField)
}

/// Maps a `serde_json` parse failure of the v1 wire struct to its precise invalid-event
/// reason. A syntax error or a truncated line (`Eof`) is malformed JSON; a semantic (`Data`)
/// error is `unknown_field` when `deny_unknown_fields` rejected an extra key (serde's own
/// "unknown field" message), otherwise `invalid_field` (wrong type, missing field, a
/// duplicate field). `Io` never arises parsing an in-memory slice; it falls back to
/// `invalid_field` rather than being treated as unreachable.
fn classify_json_error(error: &serde_json::Error) -> DropReason {
    use serde_json::error::Category;
    match error.classify() {
        Category::Syntax | Category::Eof => DropReason::MalformedJson,
        Category::Data if error.to_string().contains("unknown field") => DropReason::UnknownField,
        Category::Data | Category::Io => DropReason::InvalidField,
    }
}

/// How the sender of an applied event was attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Cgroup,
    /// The sender exited before its cgroup could be read; accepted on uid alone.
    Uid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DropCounts {
    pub not_job_process: u64,
    pub job_mismatch: u64,
    pub uid_mismatch: u64,
    pub unknown_kind: u64,
    pub unsupported_version: u64,
    pub invalid_status: u64,
    pub unknown_field: u64,
    pub invalid_field: u64,
    pub malformed_json: u64,
    pub oversize: u64,
    pub read_timeout: u64,
    pub rate_limited: u64,
    pub busy: u64,
}

impl DropCounts {
    pub fn record(&mut self, reason: DropReason) {
        let slot = self.slot_mut(reason);
        *slot = slot.saturating_add(1);
    }

    pub fn get(&self, reason: DropReason) -> u64 {
        match reason {
            DropReason::NotJobProcess => self.not_job_process,
            DropReason::JobMismatch => self.job_mismatch,
            DropReason::UidMismatch => self.uid_mismatch,
            DropReason::UnknownKind => self.unknown_kind,
            DropReason::UnsupportedVersion => self.unsupported_version,
            DropReason::InvalidStatus => self.invalid_status,
            DropReason::UnknownField => self.unknown_field,
            DropReason::InvalidField => self.invalid_field,
            DropReason::MalformedJson => self.malformed_json,
            DropReason::Oversize => self.oversize,
            DropReason::ReadTimeout => self.read_timeout,
            DropReason::RateLimited => self.rate_limited,
            DropReason::Busy => self.busy,
        }
    }

    pub fn total(&self) -> u64 {
        DropReason::ALL
            .iter()
            .fold(0u64, |sum, reason| sum.saturating_add(self.get(*reason)))
    }

    fn slot_mut(&mut self, reason: DropReason) -> &mut u64 {
        match reason {
            DropReason::NotJobProcess => &mut self.not_job_process,
            DropReason::JobMismatch => &mut self.job_mismatch,
            DropReason::UidMismatch => &mut self.uid_mismatch,
            DropReason::UnknownKind => &mut self.unknown_kind,
            DropReason::UnsupportedVersion => &mut self.unsupported_version,
            DropReason::InvalidStatus => &mut self.invalid_status,
            DropReason::UnknownField => &mut self.unknown_field,
            DropReason::InvalidField => &mut self.invalid_field,
            DropReason::MalformedJson => &mut self.malformed_json,
            DropReason::Oversize => &mut self.oversize,
            DropReason::ReadTimeout => &mut self.read_timeout,
            DropReason::RateLimited => &mut self.rate_limited,
            DropReason::Busy => &mut self.busy,
        }
    }
}

/// One step's checkpoint state. Persisted as `checkpoint.json` across Spur
/// versions, so every field must default when absent. Ranks may disagree on a
/// step's outcome; when both counters cover it, `last_completed_step` wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointRecord {
    pub completed: u64,
    pub failed: u64,
    pub last_completed_step: Option<u64>,
    pub last_completed_at: Option<DateTime<Utc>>,
    pub last_failed_step: Option<u64>,
    pub in_progress_step: Option<u64>,
    pub in_progress_since: Option<DateTime<Utc>>,
    pub restore_step: Option<u64>,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub uid_verified: u64,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub dropped: DropCounts,
}

impl CheckpointRecord {
    /// Apply one attributed event. Every rank of a step reports the same
    /// training steps, so a repeat of a step already counted changes nothing.
    pub fn apply(
        &mut self,
        event: &CheckpointEvent,
        verification: Verification,
        now: DateTime<Utc>,
    ) -> Result<(), DropReason> {
        match event.kind {
            EventKind::Begin => self.begin(event.step, now),
            EventKind::End => self.end(event.step, event.status, now)?,
            EventKind::Restore => self.restore_step = Some(event.step),
        }
        if verification == Verification::Uid {
            self.uid_verified = self.uid_verified.saturating_add(1);
        }
        self.first_event_at.get_or_insert(now);
        self.last_event_at = Some(now);
        Ok(())
    }

    pub fn record_drop(&mut self, reason: DropReason) {
        self.dropped.record(reason);
    }

    fn begin(&mut self, step: u64, now: DateTime<Utc>) {
        let already_done = self.last_completed_step.is_some_and(|done| step <= done);
        let already_running = self.in_progress_step.is_some_and(|current| step <= current);
        if already_done || already_running {
            return;
        }
        self.in_progress_step = Some(step);
        self.in_progress_since = Some(now);
    }

    fn end(
        &mut self,
        step: u64,
        status: Option<EndStatus>,
        now: DateTime<Utc>,
    ) -> Result<(), DropReason> {
        let status = status.ok_or(DropReason::InvalidStatus)?;
        // Ranks finish a save at different times, so a slow rank's end for a
        // step older than the newest completed one is expected, not malformed.
        if self.last_completed_step.is_some_and(|done| step < done) {
            return Ok(());
        }
        if self.in_progress_step.is_some_and(|current| current <= step) {
            self.in_progress_step = None;
            self.in_progress_since = None;
        }
        match status {
            EndStatus::Ok if self.last_completed_step != Some(step) => {
                self.completed = self.completed.saturating_add(1);
                self.last_completed_step = Some(step);
                self.last_completed_at = Some(now);
            }
            EndStatus::Failed if self.last_failed_step != Some(step) => {
                self.failed = self.failed.saturating_add(1);
                self.last_failed_step = Some(step);
            }
            EndStatus::Ok | EndStatus::Failed => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(json: &str) -> Vec<u8> {
        format!("{json}\n").into_bytes()
    }

    #[test]
    fn parse_event_accepts_every_v1_kind() {
        let begin = parse_event(&line(
            r#"{"v":1,"kind":"checkpoint-begin","step":10,"job_id":"7","step_id":"0"}"#,
        ));
        assert_eq!(
            begin,
            Ok(CheckpointEvent {
                kind: EventKind::Begin,
                step: 10,
                status: None,
                job_id: 7,
                step_id: Some(0)
            })
        );
        let ok = parse_event(&line(
            r#"{"v":1,"kind":"checkpoint-end","step":10,"status":"ok","job_id":"7","step_id":"0"}"#,
        ));
        assert_eq!(
            ok.map(|e| (e.kind, e.status)),
            Ok((EventKind::End, Some(EndStatus::Ok)))
        );
        let failed = parse_event(&line(
            r#"{"v":1,"kind":"checkpoint-end","step":10,"status":"failed","job_id":"7","step_id":"0"}"#,
        ));
        assert_eq!(failed.map(|e| e.status), Ok(Some(EndStatus::Failed)));
        let restore = parse_event(
            br#"{"v":1,"kind":"restore","step":4420,"job_id":"7","step_id":"4294967291"}"#,
        );
        assert_eq!(
            restore,
            Ok(CheckpointEvent {
                kind: EventKind::Restore,
                step: 4420,
                status: None,
                job_id: 7,
                step_id: Some(4294967291)
            })
        );
    }

    #[test]
    fn parse_event_accepts_an_absent_step_id() {
        let begin = parse_event(&line(
            r#"{"v":1,"kind":"checkpoint-begin","step":10,"job_id":"7"}"#,
        ));
        assert_eq!(
            begin,
            Ok(CheckpointEvent {
                kind: EventKind::Begin,
                step: 10,
                status: None,
                job_id: 7,
                step_id: None
            })
        );
    }

    #[test]
    fn parse_event_rejects_an_unrecognized_extra_field_as_unknown_field() {
        assert_eq!(
            parse_event(&line(
                r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"0","extra":1}"#
            )),
            Err(DropReason::UnknownField)
        );
    }

    #[test]
    fn parse_event_rejects_a_non_v1_wire_version_as_unsupported_version() {
        assert_eq!(
            parse_event(&line(
                r#"{"v":2,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"0"}"#
            )),
            Err(DropReason::UnsupportedVersion)
        );
    }

    #[test]
    fn parse_event_rejects_an_unrecognized_kind_as_unknown_kind() {
        assert_eq!(
            parse_event(&line(
                r#"{"v":1,"kind":"checkpoint-start","step":1,"job_id":"7","step_id":"0"}"#
            )),
            Err(DropReason::UnknownKind)
        );
    }

    #[test]
    fn parse_event_rejects_a_misplaced_or_invalid_status_as_invalid_status() {
        let rejected = [
            // status on a non-end event
            r#"{"v":1,"kind":"checkpoint-begin","step":1,"status":"ok","job_id":"7","step_id":"0"}"#,
            // status missing on an end event
            r#"{"v":1,"kind":"checkpoint-end","step":1,"job_id":"7","step_id":"0"}"#,
            // status present but not ok|failed
            r#"{"v":1,"kind":"checkpoint-end","step":1,"status":"maybe","job_id":"7","step_id":"0"}"#,
        ];
        for json in rejected {
            assert_eq!(
                parse_event(&line(json)),
                Err(DropReason::InvalidStatus),
                "{json}"
            );
        }
    }

    #[test]
    fn parse_event_rejects_bad_field_values_as_invalid_field() {
        let rejected = [
            // non-numeric job_id
            r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":"x7","step_id":"0"}"#,
            // non-numeric step_id
            r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"x0"}"#,
            // job_id as the wrong JSON type (number, not string)
            r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":7,"step_id":"0"}"#,
            // step cannot be negative
            r#"{"v":1,"kind":"checkpoint-begin","step":-1,"job_id":"7","step_id":"0"}"#,
            // step must be an integer
            r#"{"v":1,"kind":"checkpoint-begin","step":1.5,"job_id":"7","step_id":"0"}"#,
            // a duplicate field is a Data error too, just not an "unknown field" one
            r#"{"v":1,"v":1,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"0"}"#,
        ];
        for json in rejected {
            assert_eq!(
                parse_event(&line(json)),
                Err(DropReason::InvalidField),
                "{json}"
            );
        }
    }

    #[test]
    fn parse_event_rejects_malformed_json_as_malformed_json() {
        let rejected = [
            // trailing data after a complete, otherwise-valid value
            r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"0"} trailing"#,
            // a JSON array, not an object
            r#"["v",1]"#,
            r#"[1,"checkpoint-begin",1,null,"7","0"]"#,
            "not json",
            "",
        ];
        for json in rejected {
            assert_eq!(
                parse_event(&line(json)),
                Err(DropReason::MalformedJson),
                "{json}"
            );
        }
    }

    #[test]
    fn parse_event_limit_is_512_bytes_before_the_newline() {
        let prefix = r#"{"v":1,"kind":"checkpoint-begin","step":1,"job_id":"7","step_id":"0"}"#;
        let at_limit = format!("{prefix}{}", " ".repeat(MAX_EVENT_BYTES - prefix.len()));
        assert_eq!(at_limit.len(), MAX_EVENT_BYTES);
        assert!(parse_event(at_limit.as_bytes()).is_ok());
        assert!(parse_event(format!("{at_limit}\n").as_bytes()).is_ok());
        let over = format!("{at_limit} ");
        assert_eq!(parse_event(over.as_bytes()), Err(DropReason::Oversize));
    }

    #[test]
    fn parse_event_never_panics_on_truncated_or_mutated_input() {
        let seeds = [
            r#"{"v":1,"kind":"checkpoint-begin","step":10,"job_id":"7","step_id":"0"}"#,
            r#"{"v":1,"kind":"checkpoint-end","step":18446744073709551615,"status":"failed","job_id":"4294967295","step_id":"4294967291"}"#,
            r#"{"v":1,"kind":"restore","step":0,"job_id":"1","step_id":"1"}"#,
        ];
        let replacements = [0x00u8, b'"', b'{', b'}', b'\\', b'9', b' ', b'\n', 0xff];
        for seed in seeds {
            let bytes = seed.as_bytes();
            for end in 0..=bytes.len() {
                let _ = parse_event(&bytes[..end]);
            }
            for pos in 0..bytes.len() {
                for &byte in &replacements {
                    let mut mutated = bytes.to_vec();
                    mutated[pos] = byte;
                    let _ = parse_event(&mutated);
                }
            }
        }
    }

    #[test]
    fn drop_reason_names_are_distinct_snake_case() {
        let names: std::collections::HashSet<_> = DropReason::ALL
            .iter()
            .map(|reason| reason.as_str())
            .collect();
        assert_eq!(names.len(), DropReason::ALL.len());
        assert!(names
            .iter()
            .all(|name| name.chars().all(|c| c.is_ascii_lowercase() || c == '_')));
    }

    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0)
            .single()
            .expect("valid timestamp")
    }

    fn event(kind: EventKind, step: u64, status: Option<EndStatus>) -> CheckpointEvent {
        CheckpointEvent {
            kind,
            step,
            status,
            job_id: 7,
            step_id: Some(0),
        }
    }

    fn begin(step: u64) -> CheckpointEvent {
        event(EventKind::Begin, step, None)
    }

    fn end_ok(step: u64) -> CheckpointEvent {
        event(EventKind::End, step, Some(EndStatus::Ok))
    }

    fn end_failed(step: u64) -> CheckpointEvent {
        event(EventKind::End, step, Some(EndStatus::Failed))
    }

    #[test]
    fn apply_begin_then_end_ok_completes_a_checkpoint() {
        let mut record = CheckpointRecord::default();
        assert_eq!(
            record.apply(&begin(1000), Verification::Cgroup, at(0)),
            Ok(())
        );
        assert_eq!(record.in_progress_step, Some(1000));
        assert_eq!(record.in_progress_since, Some(at(0)));
        assert_eq!(
            record.apply(&end_ok(1000), Verification::Cgroup, at(5)),
            Ok(())
        );
        assert_eq!(record.completed, 1);
        assert_eq!(record.last_completed_step, Some(1000));
        assert_eq!(record.last_completed_at, Some(at(5)));
        assert_eq!(record.in_progress_step, None);
        assert_eq!(record.in_progress_since, None);
        assert_eq!(record.first_event_at, Some(at(0)));
        assert_eq!(record.last_event_at, Some(at(5)));
    }

    #[test]
    fn apply_counts_each_training_step_once_across_ranks() {
        let mut record = CheckpointRecord::default();
        for rank in 0..8 {
            assert_eq!(
                record.apply(&begin(2000), Verification::Cgroup, at(rank)),
                Ok(())
            );
        }
        assert_eq!(record.in_progress_since, Some(at(0)));
        for rank in 0..8 {
            assert_eq!(
                record.apply(&end_ok(2000), Verification::Cgroup, at(10 + rank)),
                Ok(())
            );
        }
        assert_eq!(record.completed, 1);
        assert_eq!(record.last_completed_at, Some(at(10)));
        assert_eq!(
            record.apply(&begin(2000), Verification::Cgroup, at(20)),
            Ok(())
        );
        assert_eq!(
            record.in_progress_step, None,
            "a slow rank's late begin is a no-op"
        );
    }

    #[test]
    fn apply_ignores_a_late_end_below_the_last_completed_step() {
        let mut record = CheckpointRecord::default();
        assert_eq!(
            record.apply(&end_ok(3000), Verification::Cgroup, at(0)),
            Ok(())
        );
        for late in [end_ok(2999), end_failed(2999)] {
            assert_eq!(record.apply(&late, Verification::Cgroup, at(1)), Ok(()));
        }
        assert_eq!(record.completed, 1);
        assert_eq!(record.failed, 0);
        assert_eq!(record.last_completed_step, Some(3000));
        assert_eq!(record.dropped.total(), 0);
    }

    #[test]
    fn apply_counts_a_failed_checkpoint_once_per_step() {
        let mut record = CheckpointRecord::default();
        assert_eq!(
            record.apply(&begin(500), Verification::Cgroup, at(0)),
            Ok(())
        );
        for rank in 0..4 {
            assert_eq!(
                record.apply(&end_failed(500), Verification::Cgroup, at(1 + rank)),
                Ok(())
            );
        }
        assert_eq!(record.failed, 1);
        assert_eq!(record.last_failed_step, Some(500));
        assert_eq!(record.completed, 0);
        assert_eq!(record.in_progress_step, None);
    }

    #[test]
    fn apply_records_restore_and_uid_verification() {
        let mut record = CheckpointRecord::default();
        let restore = event(EventKind::Restore, 4420, None);
        assert_eq!(record.apply(&restore, Verification::Uid, at(0)), Ok(()));
        assert_eq!(record.restore_step, Some(4420));
        assert_eq!(record.uid_verified, 1);
        assert_eq!(
            record.apply(&begin(5000), Verification::Cgroup, at(1)),
            Ok(())
        );
        assert_eq!(record.uid_verified, 1);
    }

    #[test]
    fn record_drop_counts_by_reason() {
        let mut record = CheckpointRecord::default();
        record.record_drop(DropReason::Busy);
        record.record_drop(DropReason::Busy);
        record.record_drop(DropReason::UidMismatch);
        assert_eq!(record.dropped.get(DropReason::Busy), 2);
        assert_eq!(record.dropped.get(DropReason::UidMismatch), 1);
        assert_eq!(record.dropped.get(DropReason::MalformedJson), 0);
        assert_eq!(record.dropped.total(), 3);
    }

    #[test]
    fn drop_counts_serialize_under_the_drop_reason_names() {
        let mut counts = DropCounts::default();
        for reason in DropReason::ALL {
            counts.record(reason);
        }
        let json = serde_json::to_value(counts).expect("serialize");
        let keys: std::collections::HashSet<_> =
            json.as_object().expect("object").keys().cloned().collect();
        let names: std::collections::HashSet<_> = DropReason::ALL
            .iter()
            .map(|reason| reason.as_str().to_string())
            .collect();
        assert_eq!(keys, names);
        for reason in DropReason::ALL {
            assert_eq!(json[reason.as_str()], 1, "{}", reason.as_str());
        }
    }

    #[test]
    fn checkpoint_record_loads_with_missing_fields() {
        let empty: CheckpointRecord = serde_json::from_str("{}").expect("empty object");
        assert_eq!(empty, CheckpointRecord::default());
        let older: CheckpointRecord = serde_json::from_str(
            r#"{"completed":3,"last_completed_step":900,"dropped":{"busy":2}}"#,
        )
        .expect("subset of fields");
        assert_eq!(older.completed, 3);
        assert_eq!(older.last_completed_step, Some(900));
        assert_eq!(older.dropped.get(DropReason::Busy), 2);
        assert_eq!(older.dropped.get(DropReason::MalformedJson), 0);
    }

    #[test]
    fn checkpoint_record_ignores_a_retired_drop_reason_key() {
        // A `checkpoint.json` written before `schema` was split into finer
        // reasons must still load (`#[serde(default)]`); the retired key is
        // silently dropped rather than mapped onto any current reason.
        let older: CheckpointRecord = serde_json::from_str(r#"{"dropped":{"schema":2,"busy":1}}"#)
            .expect("loads despite a retired drop-reason key");
        assert_eq!(older.dropped.get(DropReason::Busy), 1);
        assert_eq!(older.dropped.total(), 1);
    }

    #[test]
    fn checkpoint_record_round_trips_with_a_final_key() {
        let mut record = CheckpointRecord::default();
        assert_eq!(
            record.apply(&end_ok(10), Verification::Cgroup, at(0)),
            Ok(())
        );
        record.is_final = true;
        let json = serde_json::to_value(&record).expect("serialize");
        assert_eq!(json["final"], true);
        let back: CheckpointRecord = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, record);
    }
}
