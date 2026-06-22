// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Burst-buffer (BB) staging model.
//!
//! A job's `--bb` string carries both stage-in/stage-out shell directives
//! (consumed by the node agent, see `spurd::executor::wrap_with_burst_buffer`)
//! and an optional cluster-wide capacity reservation. This module owns the
//! capacity grammar and the per-job staging state machine so the controller and
//! the agent agree on how `--bb` is parsed.

use serde::{Deserialize, Serialize};

/// Per-job burst-buffer staging phase, advanced controller-side.
///
/// A job requesting BB capacity moves `None -> Staging` once its capacity is
/// reserved, then `Staging -> Ready` once stage-in completes. Only a `Ready`
/// (or `None`, i.e. no BB) job is dispatchable. Persisted with the `Job` in the
/// Raft snapshot so the phase survives controller restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BbStageState {
    /// No BB capacity requested, or none reserved yet.
    #[default]
    None,
    /// Capacity reserved; stage-in in progress (job held off dispatch).
    Staging,
    /// Stage-in complete; the job may be dispatched.
    Ready,
}

/// Extract the BB capacity (in GB) a `--bb` string reserves cluster-wide.
///
/// Grammar (semicolon-separated directives, same splitting as the agent's
/// stage-in/out parser): `capacity=NNN` where NNN is gibibytes. Unknown
/// directives (e.g. `stage_in:`/`stage_out:`) are ignored here — they are the
/// agent's concern. Returns 0 when no capacity is requested, so a job with only
/// stage directives never consumes the pool.
pub fn parse_capacity_gb(bb: &str) -> u64 {
    let mut total = 0u64;
    for directive in bb.split(';') {
        let directive = directive.trim();
        if let Some(val) = directive.strip_prefix("capacity=") {
            // Tolerate a trailing unit suffix (e.g. "100GB"); take leading digits.
            let digits: String = val
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u64>() {
                total = total.saturating_add(n);
            }
        }
    }
    total
}

/// Currently-free BB capacity: configured total minus capacity held by jobs
/// that already reserved it (`used`). Saturating so it never underflows.
pub fn free_capacity_gb(total_gb: u64, used_gb: u64) -> u64 {
    total_gb.saturating_sub(used_gb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_parses_plain_gb() {
        assert_eq!(parse_capacity_gb("capacity=100"), 100);
    }

    #[test]
    fn capacity_tolerates_unit_suffix() {
        assert_eq!(parse_capacity_gb("capacity=250GB"), 250);
    }

    #[test]
    fn capacity_ignores_stage_directives() {
        assert_eq!(parse_capacity_gb("stage_in:cp a b;stage_out:cp c d"), 0);
    }

    #[test]
    fn capacity_combines_with_stage_directives() {
        assert_eq!(parse_capacity_gb("capacity=64;stage_in:cp /data /tmp"), 64);
    }

    #[test]
    fn capacity_sums_multiple_directives() {
        assert_eq!(parse_capacity_gb("capacity=10;capacity=5"), 15);
    }

    #[test]
    fn capacity_empty_and_garbage_is_zero() {
        assert_eq!(parse_capacity_gb(""), 0);
        assert_eq!(parse_capacity_gb("capacity=abc"), 0);
        assert_eq!(parse_capacity_gb("nonsense"), 0);
    }

    #[test]
    fn free_capacity_saturates() {
        assert_eq!(free_capacity_gb(100, 30), 70);
        assert_eq!(free_capacity_gb(20, 50), 0);
    }

    #[test]
    fn stage_state_default_is_none() {
        assert_eq!(BbStageState::default(), BbStageState::None);
    }

    #[test]
    fn stage_state_serde_roundtrips() {
        for st in [
            BbStageState::None,
            BbStageState::Staging,
            BbStageState::Ready,
        ] {
            let json = serde_json::to_string(&st).expect("serialize");
            let back: BbStageState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, st);
        }
    }

    #[test]
    fn stage_state_serializes_screaming_snake() {
        assert_eq!(
            serde_json::to_string(&BbStageState::Staging).unwrap(),
            "\"STAGING\""
        );
    }
}
