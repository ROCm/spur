// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::{
    GetFairshareFactorsRequest, GetJobsRequest, GetPartitionsRequest, JobState,
};

use crate::usage_scope::{cell, UsageScope};

/// View job priority breakdown for pending jobs.
#[derive(Parser, Debug)]
// -h is sprio's --noheader (Slurm convention), so disable clap's auto -h and
// re-add --help below as long-only.
#[command(
    name = "sprio",
    about = "View job priority factors",
    disable_help_flag = true
)]
pub struct SprioArgs {
    /// Show only these job IDs (comma-separated)
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Show only jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Long format (more detail)
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
    pub noheader: bool,

    /// Print help
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<SprioArgs>(&args);

    let job_ids = args
        .jobs
        .as_deref()
        .map(crate::job_id_arg::parse_job_ids)
        .unwrap_or_default();

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel.clone());

    // Get pending jobs only (priority is relevant for pending jobs)
    let response = client
        .get_jobs(GetJobsRequest {
            states: vec![JobState::JobPending as i32],
            user: args.user.unwrap_or_default(),
            partition: String::new(),
            account: String::new(),
            job_ids,
            name: String::new(),
            nodes: Vec::new(),
            qos: String::new(),
            reservation: String::new(),
        })
        .await
        .context("failed to get jobs")?;

    let jobs = response.into_inner().jobs;

    // Get partitions for tier lookup
    let partitions_resp = client
        .get_partitions(GetPartitionsRequest {
            name: String::new(),
        })
        .await
        .context("failed to get partitions")?;

    let partitions = partitions_resp.into_inner().partitions;

    // Fetch fairshare factors from the accounting service (same endpoint)
    let (fairshare_map, scope): (HashMap<(String, String), f64>, UsageScope) = {
        let mut acct_client = spur_proto::accounting_client(channel);
        match acct_client
            .get_fairshare_factors(GetFairshareFactorsRequest { halflife_days: 0 })
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                (
                    resp.entries
                        .into_iter()
                        .map(|e| ((e.user, e.account), e.factor))
                        .collect(),
                    UsageScope::from_replies([resp.scoped_to_user.as_deref()]),
                )
            }
            Err(e) => {
                eprintln!(
                    "sprio: warning: could not fetch fair-share factors: {e}\n\
                     FAIRSHARE column will show 1.0 (neutral)."
                );
                (HashMap::new(), UsageScope::default())
            }
        }
    };

    let now = chrono::Utc::now();

    if args.long {
        if !args.noheader {
            println!(
                "{:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10} {:>10}",
                "JOBID", "USER", "PRIORITY", "AGE", "FAIRSHARE", "PARTITION", "QOS", "EFFECTIVE"
            );
        }

        for job in &jobs {
            let submit_secs = job
                .submit_time
                .as_ref()
                .map(|t| t.seconds)
                .unwrap_or(now.timestamp());
            let age_minutes = (now.timestamp() - submit_secs) / 60;
            let age_factor = 1.0 + (age_minutes as f64 / 10080.0).min(1.0);

            let partition_tier = partitions
                .iter()
                .find(|p| p.name == job.partition)
                .map(|p| p.priority_tier)
                .unwrap_or(1);

            let base = job.priority;
            let fair_share = fair_share_for(&scope, &fairshare_map, &job.user, &job.account);
            let effective = fair_share.map(|fair_share| {
                (base as f64 * fair_share.min(10.0) * age_factor * partition_tier.max(1) as f64)
                    as u32
            });

            println!(
                "{:>10} {:>10} {:>10} {:>10.4} {} {:>12} {:>10} {}",
                job.job_id,
                job.user,
                base,
                age_factor,
                cell(fair_share.is_some(), 10, || format!(
                    "{:>10.4}",
                    fair_share.unwrap_or_default()
                )),
                format!("{}(T{})", job.partition, partition_tier),
                if job.qos.is_empty() {
                    "normal"
                } else {
                    &job.qos
                },
                cell(effective.is_some(), 10, || format!(
                    "{:>10}",
                    effective.unwrap_or_default()
                )),
            );
        }
    } else {
        if !args.noheader {
            println!(
                "{:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
                "JOBID", "USER", "PRIORITY", "AGE", "FAIRSHARE", "PARTITION"
            );
        }

        for job in &jobs {
            let submit_secs = job
                .submit_time
                .as_ref()
                .map(|t| t.seconds)
                .unwrap_or(now.timestamp());
            let age_minutes = (now.timestamp() - submit_secs) / 60;
            let age_factor = 1.0 + (age_minutes as f64 / 10080.0).min(1.0);

            let partition_tier = partitions
                .iter()
                .find(|p| p.name == job.partition)
                .map(|p| p.priority_tier)
                .unwrap_or(1);

            let fair_share = fair_share_for(&scope, &fairshare_map, &job.user, &job.account);

            println!(
                "{:>10} {:>10} {:>10} {:>10.4} {} {:>10}",
                job.job_id,
                job.user,
                job.priority,
                age_factor,
                cell(fair_share.is_some(), 10, || format!(
                    "{:>10.4}",
                    fair_share.unwrap_or_default()
                )),
                partition_tier,
            );
        }
    }

    Ok(())
}

/// `None` when a pinned reply withheld this job's user: the neutral `1.0`
/// would read as "exactly on share" and feed EFFECTIVE a made-up value.
fn fair_share_for(
    scope: &UsageScope,
    factors: &HashMap<(String, String), f64>,
    user: &str,
    account: &str,
) -> Option<f64> {
    if !scope.user_visible(user) {
        return None;
    }
    Some(
        factors
            .get(&(user.to_string(), account.to_string()))
            .copied()
            .unwrap_or(1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_share_is_withheld_only_for_another_user_under_a_pinned_reply() {
        let factors = HashMap::from([(("alice".to_string(), "ml".to_string()), 2.5)]);
        let open = UsageScope::default();
        assert_eq!(fair_share_for(&open, &factors, "alice", "ml"), Some(2.5));
        assert_eq!(fair_share_for(&open, &factors, "bob", "ml"), Some(1.0));

        let pinned = UsageScope::from_replies([Some("alice")]);
        assert_eq!(fair_share_for(&pinned, &factors, "alice", "ml"), Some(2.5));
        assert_eq!(fair_share_for(&pinned, &factors, "bob", "ml"), None);
    }

    #[test]
    fn short_h_means_noheader() {
        let args = SprioArgs::try_parse_from(["sprio", "-h"]).expect("-h is accepted");
        assert!(args.noheader);
    }

    #[test]
    fn help_is_still_reachable_by_its_long_name() {
        let err = SprioArgs::try_parse_from(["sprio", "--help"]).expect_err("--help stops parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
