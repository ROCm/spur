// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared parsing for the Slurm-style `-j/--jobs` job-ID argument used by
//! `squeue`, `sprio`, and `sacct`.

use spur_core::job::JobId;

/// Parse a comma-separated `-j/--jobs` value into numeric job IDs.
///
/// Step suffixes (`30.0`, `30.batch`) match the base job ID, since Spur
/// schedules and accounts whole jobs rather than individual steps. Unparseable
/// entries are dropped.
pub fn parse_job_ids(spec: &str) -> Vec<JobId> {
    spec.split(',')
        .filter_map(|tok| {
            let base = tok.trim().split('.').next().unwrap_or_default();
            base.parse::<JobId>().ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists_steps_and_junk() {
        assert_eq!(parse_job_ids("30"), vec![30]);
        assert_eq!(parse_job_ids("30,31,32"), vec![30, 31, 32]);
        assert_eq!(parse_job_ids(" 30 , 31 "), vec![30, 31]);
        assert_eq!(parse_job_ids("30.0"), vec![30]);
        assert_eq!(parse_job_ids("30.batch,31.0"), vec![30, 31]);
        assert_eq!(parse_job_ids("30,abc,31"), vec![30, 31]);
        assert!(parse_job_ids("").is_empty());
        assert!(parse_job_ids("abc").is_empty());
    }
}
