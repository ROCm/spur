// SPDX-License-Identifier: Apache-2.0

//! Shared Slurm-style time rendering for the CLI printers.

/// Slurm timestamp form (`2026-08-27T07:03:07`), or `N/A` when unset.
pub fn format_timestamp(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "N/A".into(),
    }
}

/// Zero-padded elapsed form (`HH:MM:SS`, or `D-HH:MM:SS` past a day), as used
/// by `sacct`, `sstat`, and `scontrol show job`.
pub fn format_duration_dhms(total_seconds: i64) -> String {
    let total_seconds = total_seconds.unsigned_abs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{}-{:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_rolls_over_into_days() {
        assert_eq!(format_duration_dhms(0), "00:00:00");
        assert_eq!(format_duration_dhms(59), "00:00:59");
        assert_eq!(format_duration_dhms(3661), "01:01:01");
        assert_eq!(format_duration_dhms(90061), "1-01:01:01");
    }

    #[test]
    fn format_duration_treats_negative_as_magnitude() {
        assert_eq!(format_duration_dhms(-3661), "01:01:01");
    }

    #[test]
    fn format_timestamp_renders_na_for_unset_and_epoch() {
        assert_eq!(format_timestamp(None), "N/A");
        assert_eq!(
            format_timestamp(Some(&prost_types::Timestamp {
                seconds: 0,
                nanos: 0
            })),
            "N/A"
        );
    }

    #[test]
    fn format_timestamp_renders_slurm_form() {
        assert_eq!(
            format_timestamp(Some(&prost_types::Timestamp {
                seconds: 1_756_281_787,
                nanos: 0
            })),
            "2025-08-27T08:03:07"
        );
    }
}
