// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared parsing for the Slurm-style `-j/--jobs` job-ID argument used by
//! `squeue`, `sprio`, and `sacct`.

use anyhow::{bail, Result};
use spur_core::job::JobId;

/// Parse a comma-separated `-j/--jobs` value into numeric job IDs.
///
/// Step suffixes (`30.0`, `30.batch`) match the base job ID, since Spur
/// schedules and accounts whole jobs rather than individual steps. A
/// non-numeric token is rejected so a bad `-j` cannot collapse into the
/// empty list, which means "no job-id filter".
pub fn parse_job_ids(spec: &str) -> Result<Vec<JobId>> {
    let mut ids = Vec::new();
    for tok in spec.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let base = tok.split('.').next().unwrap_or(tok);
        match base.parse::<JobId>() {
            Ok(id) => ids.push(id),
            Err(_) => bail!("Invalid job id specified: {tok}"),
        }
    }
    if ids.is_empty() {
        bail!("Invalid job id specified: (empty)");
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists_and_step_suffixes() {
        assert_eq!(parse_job_ids("30").unwrap(), vec![30]);
        assert_eq!(parse_job_ids("30,31,32").unwrap(), vec![30, 31, 32]);
        assert_eq!(parse_job_ids(" 30 , 31 ").unwrap(), vec![30, 31]);
        assert_eq!(parse_job_ids("30.0").unwrap(), vec![30]);
        assert_eq!(parse_job_ids("30.batch,31.0").unwrap(), vec![30, 31]);
        assert_eq!(parse_job_ids("30,").unwrap(), vec![30]);
        assert_eq!(parse_job_ids("30,,31").unwrap(), vec![30, 31]);
    }

    #[test]
    fn rejects_unparseable_ids() {
        let err = parse_job_ids("abc").unwrap_err();
        assert!(err.to_string().contains("Invalid job id specified: abc"));

        let err = parse_job_ids("not-a-jobid").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid job id specified: not-a-jobid"));

        let err = parse_job_ids("30,abc,31").unwrap_err();
        assert!(err.to_string().contains("Invalid job id specified: abc"));
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_job_ids("")
            .unwrap_err()
            .to_string()
            .contains("Invalid job id specified: (empty)"));
        assert!(parse_job_ids("  ,  ").is_err());
    }
}
