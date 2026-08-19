// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared CLI time-argument parsing and proto conversion, used by `sacct`,
//! `sreport`, and `sacctmgr show txn` so `Start=`/`End=`/`-S`/`-E` behave
//! identically across commands.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

/// Parse a time argument string into a DateTime.
/// Supports: "2024-01-01", "2024-01-01T00:00:00", "now-7days", "now-24hours".
pub fn parse_time_arg(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();

    // Relative: "now-Ndays", "now-Nhours". Per Slurm's `now[{+|-}count...]`
    // grammar the count is unsigned, so parse it as such — a stray sign like
    // "now--7days" is rejected rather than silently jumping into the future.
    if let Some(rest) = s.strip_prefix("now-") {
        if let Some(days) = rest
            .strip_suffix("days")
            .or_else(|| rest.strip_suffix("day"))
        {
            let n: u64 = days.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::days(n as i64));
        }
        if let Some(hours) = rest
            .strip_suffix("hours")
            .or_else(|| rest.strip_suffix("hour"))
        {
            let n: u64 = hours.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::hours(n as i64));
        }
    }

    // ISO datetime: "2024-01-01T00:00:00"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(ndt.and_utc());
    }

    // Date only: "2024-01-01"
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return nd.and_hms_opt(0, 0, 0).map(|ndt| ndt.and_utc());
    }

    None
}

/// Convert a DateTime into a protobuf Timestamp.
pub fn datetime_to_proto(dt: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_date_only_as_midnight_utc() {
        let dt = parse_time_arg("2024-01-02").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-02T00:00:00+00:00");
    }

    #[test]
    fn parses_iso_datetime() {
        let dt = parse_time_arg("2024-01-02T03:04:05").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-02T03:04:05+00:00");
    }

    #[test]
    fn parses_relative_days_and_hours() {
        let now = Utc::now();
        let days = parse_time_arg("now-2days").unwrap();
        assert!((now - days).num_hours() >= 47 && (now - days).num_hours() <= 49);

        let hours = parse_time_arg("now-6hours").unwrap();
        assert!((now - hours).num_minutes() >= 359 && (now - hours).num_minutes() <= 361);
    }

    #[test]
    fn rejects_unparseable_input() {
        assert!(parse_time_arg("not-a-time").is_none());
        assert!(parse_time_arg("now-3weeks").is_none());
    }

    #[test]
    fn rejects_stray_negative_offset() {
        // "now--7days" must not silently resolve to a future time.
        assert!(parse_time_arg("now--7days").is_none());
        assert!(parse_time_arg("now--3hours").is_none());
    }

    #[test]
    fn proto_roundtrip_preserves_seconds() {
        let dt = parse_time_arg("2024-01-02T03:04:05").unwrap();
        let ts = datetime_to_proto(dt);
        assert_eq!(ts.seconds, dt.timestamp());
    }
}
