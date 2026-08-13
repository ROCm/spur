// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::GetJobsRequest;

use crate::format_engine;

/// View information about jobs in the scheduling queue.
#[derive(Parser, Debug)]
// -h is squeue's --noheader (Slurm convention), so disable clap's auto -h and
// re-add --help below as long-only.
#[command(
    name = "squeue",
    about = "View the job queue",
    disable_help_flag = true
)]
pub struct SqueueArgs {
    /// Show only jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Show only jobs in this partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Show only jobs in these states (comma-separated)
    #[arg(short = 't', long)]
    pub states: Option<String>,

    /// Show only these job IDs (comma-separated)
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Show only this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Show only jobs with this name
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Show only jobs allocated on these nodes (hostlist expression)
    #[arg(short = 'w', long = "nodelist")]
    pub nodelist: Option<String>,

    /// Output format string
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Long format (more columns)
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
    pub noheader: bool,

    /// Print help
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// Sort by field(s), comma-separated, each optionally prefixed with + (asc) or - (desc)
    #[arg(short = 'S', long, allow_hyphen_values = true)]
    pub sort: Option<String>,

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
    let args = SqueueArgs::try_parse_from(&args)?;

    // Determine format
    let fmt = if let Some(ref f) = args.format {
        f.clone()
    } else if args.long {
        "%.18i %.9P %.8j %.8u %.8T %.10M %.9l %.6D %R".to_string()
    } else {
        format_engine::SQUEUE_DEFAULT_FORMAT.to_string()
    };

    let fields = format_engine::parse_format(&fmt, &format_engine::squeue_header);

    // Parse state filter — default to Pending+Running+Completing when no filter specified (Slurm default)
    let states = match args.states.as_deref() {
        Some(s) => parse_states_arg(s)?,
        None => default_squeue_states(),
    };

    // Parse the sort spec before any network I/O so an invalid -S surfaces its own error
    // rather than a downstream connect failure.
    let sort_keys = match args.sort.as_deref() {
        Some(s) => parse_sort_arg(s)?,
        None => default_sort_keys(),
    };

    // Parse job ID filter
    let job_ids = args
        .jobs
        .as_ref()
        .map(|s| {
            s.split(',')
                .filter_map(|j| j.trim().parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Expand the -w node filter before any network I/O so a bad hostlist
    // surfaces its own error rather than a downstream connect failure.
    let nodes = match args.nodelist.as_deref() {
        Some(s) => expand_node_filter(s)?,
        None => Vec::new(),
    };

    // Connect and fetch
    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    let response = client
        .get_jobs(GetJobsRequest {
            states: states.iter().map(|s| *s as i32).collect(),
            user: args.user.unwrap_or_default(),
            partition: args.partition.unwrap_or_default(),
            account: args.account.unwrap_or_default(),
            job_ids,
            name: args.name.unwrap_or_default(),
            nodes,
        })
        .await
        .context("failed to get jobs")?;

    let mut jobs = response.into_inner().jobs;
    sort_jobs(&mut jobs, &sort_keys);

    // Print header
    if !args.noheader {
        println!("{}", format_engine::format_header(&fields));
    }

    // Print rows
    for job in &jobs {
        let row = format_engine::format_row(&fields, &|spec| resolve_job_field(job, spec));
        println!("{}", row);
    }

    Ok(())
}

fn resolve_job_field(job: &spur_proto::proto::JobInfo, spec: char) -> String {
    match spec {
        'i' => job.job_id.to_string(),
        'j' | 'n' => job.name.clone(),
        'u' => job.user.clone(),
        'P' => job.partition.clone(),
        't' => state_code(job.state),
        'T' => state_name(job.state),
        'M' => format_runtime(job),
        'l' => format_time_limit(job),
        'D' => job.num_nodes.to_string(),
        'R' => {
            if job.state == spur_proto::proto::JobState::JobPending as i32 {
                format!("({})", job.state_reason)
            } else {
                job.nodelist.clone()
            }
        }
        'C' => job.cpus_per_task.to_string(),
        'N' => job.nodelist.clone(),
        'a' => job.account.clone(),
        'p' => job.priority.to_string(),
        'q' => job.qos.clone(),
        'r' => crate::exit_fmt::render_reason(&job.state_reason, job.exit_signal),
        'Z' => job.work_dir.clone(),
        'o' => job.command.clone(),
        'S' => format_timestamp(job.start_time.as_ref()),
        'V' => format_timestamp(job.submit_time.as_ref()),
        'v' => job.reservation.clone(),
        'e' => format_timestamp(job.end_time.as_ref()),
        'k' => job.comment.clone(),
        'A' => job.job_id.to_string(),
        // Generic resources (GRES) requested, e.g. "gpu:8" or "gpu:mi300x:4/node".
        'b' => {
            if job.req_gpus_detail.is_empty() {
                "N/A".into()
            } else {
                job.req_gpus_detail.clone()
            }
        }
        _ => "?".into(),
    }
}

/// One field in a sort spec, with direction.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SortKey {
    spec: char,
    descending: bool,
}

/// Slurm's documented default job sort: partition asc, state asc, priority desc.
/// A trailing jobid asc keeps equal-priority jobs deterministically ordered.
fn default_sort_keys() -> Vec<SortKey> {
    vec![
        SortKey {
            spec: 'P',
            descending: false,
        },
        SortKey {
            spec: 't',
            descending: false,
        },
        SortKey {
            spec: 'p',
            descending: true,
        },
        SortKey {
            spec: 'i',
            descending: false,
        },
    ]
}

/// Parse `-S` / `--sort`: comma-separated fields, each optionally prefixed with
/// `+` (asc, default) or `-` (desc). Reuses the format-spec letters.
fn parse_sort_arg(s: &str) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for token in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (descending, rest) = match token.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, token.strip_prefix('+').unwrap_or(token)),
        };
        let mut chars = rest.chars();
        let spec = match (chars.next(), chars.next()) {
            (Some(c), None) => c,
            _ => anyhow::bail!("Invalid sort specification: {token}"),
        };
        if format_engine::squeue_header(spec) == "?" {
            anyhow::bail!("Invalid sort specification: {token}");
        }
        keys.push(SortKey { spec, descending });
    }
    if keys.is_empty() {
        anyhow::bail!("Invalid sort specification: (empty)");
    }
    Ok(keys)
}

fn sort_jobs(jobs: &mut [spur_proto::proto::JobInfo], keys: &[SortKey]) {
    jobs.sort_by(|a, b| {
        for key in keys {
            let ord = compare_field(a, b, key.spec);
            let ord = if key.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Compare two jobs on a single field. Numeric fields compare numerically;
/// timestamps by epoch seconds; everything else lexically on the rendered value.
fn compare_field(
    a: &spur_proto::proto::JobInfo,
    b: &spur_proto::proto::JobInfo,
    spec: char,
) -> std::cmp::Ordering {
    match spec {
        'i' | 'A' => a.job_id.cmp(&b.job_id),
        'p' => a.priority.cmp(&b.priority),
        'D' => a.num_nodes.cmp(&b.num_nodes),
        'C' => a.cpus_per_task.cmp(&b.cpus_per_task),
        't' | 'T' => state_sort_rank(a.state).cmp(&state_sort_rank(b.state)),
        'M' => run_time_secs(a).cmp(&run_time_secs(b)),
        'l' => time_limit_secs(a).cmp(&time_limit_secs(b)),
        'L' => time_left_secs(a).cmp(&time_left_secs(b)),
        'S' => ts_secs(a.start_time.as_ref()).cmp(&ts_secs(b.start_time.as_ref())),
        'V' => ts_secs(a.submit_time.as_ref()).cmp(&ts_secs(b.submit_time.as_ref())),
        'e' => ts_secs(a.end_time.as_ref()).cmp(&ts_secs(b.end_time.as_ref())),
        'P' => a.partition.cmp(&b.partition),
        'u' => a.user.cmp(&b.user),
        'j' | 'n' => a.name.cmp(&b.name),
        'a' => a.account.cmp(&b.account),
        'q' => a.qos.cmp(&b.qos),
        _ => resolve_job_field(a, spec).cmp(&resolve_job_field(b, spec)),
    }
}

fn state_sort_rank(state: i32) -> u8 {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.sort_rank())
        .unwrap_or(u8::MAX)
}

fn run_time_secs(job: &spur_proto::proto::JobInfo) -> i64 {
    job.run_time.as_ref().map(|d| d.seconds).unwrap_or(0)
}

fn time_limit_secs(job: &spur_proto::proto::JobInfo) -> i64 {
    job.time_limit
        .as_ref()
        .map(|d| d.seconds)
        .unwrap_or(i64::MAX)
}

fn ts_secs(ts: Option<&prost_types::Timestamp>) -> i64 {
    ts.map(|t| t.seconds).unwrap_or(0)
}

/// Unlimited time limit sorts last, matching `-S L`.
fn time_left_secs(job: &spur_proto::proto::JobInfo) -> i64 {
    let limit = time_limit_secs(job);
    if limit == i64::MAX {
        return i64::MAX;
    }
    limit.saturating_sub(run_time_secs(job))
}

fn state_code(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.code().to_string())
        .unwrap_or_else(|| "?".into())
}

fn state_name(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display().to_string())
        .unwrap_or_else(|| "UNKNOWN".into())
}

/// Default `squeue` states when `-t` is omitted: PD, R, S, CG (Slurm parity —
/// suspended jobs remain visible in the default view).
fn default_squeue_states() -> Vec<spur_proto::proto::JobState> {
    vec![
        spur_proto::proto::JobState::JobPending,
        spur_proto::proto::JobState::JobRunning,
        spur_proto::proto::JobState::JobSuspended,
        spur_proto::proto::JobState::JobCompleting,
    ]
}

/// Parse `-t` / `--states` (comma-separated). Whole-string `all` means no state filter.
/// Unknown tokens are rejected (Slurm exits with an error rather than showing all jobs).
fn parse_states_arg(s: &str) -> Result<Vec<spur_proto::proto::JobState>> {
    use spur_core::job::JobState;

    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(Vec::new());
    }

    let tokens: Vec<&str> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        anyhow::bail!("Invalid job state specified: (empty)");
    }

    let mut states = Vec::with_capacity(tokens.len());
    for token in tokens {
        let core = JobState::from_code_or_name(token)
            .ok_or_else(|| anyhow::anyhow!("Invalid job state specified: {token}"))?;
        states.push(core.to_proto());
    }
    Ok(states)
}

/// Expand `-w` / `--nodelist` into concrete node names. Accepts every Slurm
/// hostlist form (`node1,node2`, `node[001-003]`, `node[1,3,5-7]`,
/// `gpu[01-04],cpu[01-02]`). An empty or whitespace-only value is rejected
/// rather than sent as a filter that would silently match nothing.
fn expand_node_filter(s: &str) -> Result<Vec<String>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Invalid node name specified: (empty)");
    }
    let nodes = spur_core::hostlist::expand(trimmed)
        .map_err(|e| anyhow::anyhow!("Invalid node name specified: {e}"))?;
    if nodes.is_empty() {
        anyhow::bail!("Invalid node name specified: {trimmed}");
    }
    Ok(nodes)
}

fn format_runtime(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref rt) = job.run_time {
        format_duration_hms(rt.seconds)
    } else {
        "0:00".into()
    }
}

fn format_time_limit(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref tl) = job.time_limit {
        format_duration_hms(tl.seconds)
    } else {
        "UNLIMITED".into()
    }
}

fn format_duration_hms(total_seconds: i64) -> String {
    let total_seconds = total_seconds.unsigned_abs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{}-{:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

fn format_timestamp(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "N/A".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::JobState as P;

    #[test]
    fn sort_flag_accepts_hyphen_prefixed_value() {
        // -S -i must parse as a descending sort spec, not a flag (allow_hyphen_values).
        let args = SqueueArgs::try_parse_from(["squeue", "-S", "-i"]).unwrap();
        assert_eq!(args.sort.as_deref(), Some("-i"));
    }

    #[test]
    fn long_help_flag_is_preserved() {
        // -h is reclaimed for --noheader, but --help must still print help.
        let err = SqueueArgs::try_parse_from(["squeue", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn short_h_is_noheader_not_help() {
        let args = SqueueArgs::try_parse_from(["squeue", "-h"]).unwrap();
        assert!(args.noheader);
    }

    #[test]
    fn default_squeue_states_includes_completing() {
        let states = default_squeue_states();
        assert_eq!(states.len(), 4);
        assert!(states.contains(&P::JobPending));
        assert!(states.contains(&P::JobRunning));
        assert!(states.contains(&P::JobSuspended));
        assert!(states.contains(&P::JobCompleting));
    }

    #[test]
    fn expand_node_filter_plain_comma_list() {
        assert_eq!(
            expand_node_filter("node1,node2").unwrap(),
            ["node1", "node2"]
        );
    }

    #[test]
    fn expand_node_filter_compacted_range_preserves_padding() {
        assert_eq!(
            expand_node_filter("node[001-003]").unwrap(),
            ["node001", "node002", "node003"]
        );
    }

    #[test]
    fn expand_node_filter_mixed_and_multi_term() {
        assert_eq!(
            expand_node_filter("node[1,3,5-7]").unwrap(),
            ["node1", "node3", "node5", "node6", "node7"]
        );
        assert_eq!(
            expand_node_filter("gpu[01-04],cpu[01-02]").unwrap(),
            ["gpu01", "gpu02", "gpu03", "gpu04", "cpu01", "cpu02"]
        );
    }

    #[test]
    fn expand_node_filter_rejects_empty_and_invalid() {
        assert!(expand_node_filter("").is_err());
        assert!(expand_node_filter("   ").is_err());
        assert!(expand_node_filter("node[1-").is_err());
    }

    #[test]
    fn parse_states_arg_accepts_codes_and_names() {
        let states = parse_states_arg("R,PD").unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0], P::JobRunning);
        assert_eq!(states[1], P::JobPending);
    }

    #[test]
    fn parse_states_arg_all_means_no_filter() {
        assert!(parse_states_arg("all").unwrap().is_empty());
        assert!(parse_states_arg("ALL").unwrap().is_empty());
    }

    #[test]
    fn parse_states_arg_rejects_unknown() {
        let err = parse_states_arg("BOGUS").unwrap_err();
        assert!(err.to_string().contains("BOGUS"));

        let err = parse_states_arg("R,BOGUS").unwrap_err();
        assert!(err.to_string().contains("BOGUS"));
    }

    #[test]
    fn parse_states_arg_rejects_empty_list() {
        assert!(parse_states_arg("").is_err());
        assert!(parse_states_arg("  ,  ").is_err());
    }

    fn job(id: u32, partition: &str, state: P, priority: u32) -> spur_proto::proto::JobInfo {
        spur_proto::proto::JobInfo {
            job_id: id,
            partition: partition.into(),
            state: state as i32,
            priority,
            ..Default::default()
        }
    }

    fn ids(jobs: &[spur_proto::proto::JobInfo]) -> Vec<u32> {
        jobs.iter().map(|j| j.job_id).collect()
    }

    #[test]
    fn parse_sort_arg_directions() {
        let keys = parse_sort_arg("P,-p,+i").unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(
            keys[0],
            SortKey {
                spec: 'P',
                descending: false
            }
        );
        assert_eq!(
            keys[1],
            SortKey {
                spec: 'p',
                descending: true
            }
        );
        assert_eq!(
            keys[2],
            SortKey {
                spec: 'i',
                descending: false
            }
        );
    }

    #[test]
    fn parse_sort_arg_rejects_bad_tokens() {
        assert!(parse_sort_arg("").is_err());
        assert!(parse_sort_arg("ii").is_err());
        assert!(parse_sort_arg("-").is_err());
    }

    #[test]
    fn sort_ascending_and_descending_jobid() {
        let mut jobs = vec![
            job(70, "default", P::JobRunning, 100),
            job(72, "default", P::JobRunning, 100),
            job(71, "default", P::JobRunning, 100),
        ];
        sort_jobs(&mut jobs, &parse_sort_arg("i").unwrap());
        assert_eq!(ids(&jobs), vec![70, 71, 72]);
        sort_jobs(&mut jobs, &parse_sort_arg("-i").unwrap());
        assert_eq!(ids(&jobs), vec![72, 71, 70]);
    }

    #[test]
    fn default_sort_matches_slurm_p_t_negp() {
        // partition asc, then state asc (PD<R), then priority desc, then jobid asc
        let mut jobs = vec![
            job(10, "b", P::JobRunning, 100),
            job(11, "a", P::JobRunning, 50),
            job(12, "a", P::JobPending, 200),
            job(13, "a", P::JobRunning, 50),
        ];
        sort_jobs(&mut jobs, &default_sort_keys());
        assert_eq!(ids(&jobs), vec![12, 11, 13, 10]);
    }

    #[test]
    fn sort_by_priority_desc_then_jobid_tiebreak() {
        let mut jobs = vec![
            job(70, "default", P::JobPending, 100),
            job(71, "default", P::JobPending, 300),
            job(72, "default", P::JobPending, 300),
        ];
        sort_jobs(&mut jobs, &parse_sort_arg("-p,i").unwrap());
        assert_eq!(ids(&jobs), vec![71, 72, 70]);
    }

    #[test]
    fn parse_sort_arg_rejects_unknown_spec() {
        let err = parse_sort_arg("x").unwrap_err();
        assert!(err.to_string().contains("Invalid sort specification"));
        assert!(parse_sort_arg("-z").is_err());
        assert!(parse_sort_arg("i,x").is_err());
    }

    #[test]
    fn state_sort_places_suspended_after_running() {
        let mut jobs = vec![
            job(1, "default", P::JobFailed, 0),
            job(2, "default", P::JobSuspended, 0),
            job(3, "default", P::JobRunning, 0),
            job(4, "default", P::JobPending, 0),
        ];
        sort_jobs(&mut jobs, &parse_sort_arg("t").unwrap());
        assert_eq!(ids(&jobs), vec![4, 3, 2, 1]);
    }

    #[test]
    fn sort_by_timelimit_puts_unlimited_last() {
        let mut a = job(1, "default", P::JobRunning, 0);
        a.time_limit = Some(prost_types::Duration {
            seconds: 600,
            nanos: 0,
        });
        let b = job(2, "default", P::JobRunning, 0);
        let mut c = job(3, "default", P::JobRunning, 0);
        c.time_limit = Some(prost_types::Duration {
            seconds: 60,
            nanos: 0,
        });
        let mut jobs = vec![a, b, c];
        sort_jobs(&mut jobs, &parse_sort_arg("l").unwrap());
        assert_eq!(ids(&jobs), vec![3, 1, 2]);
    }

    #[test]
    fn sort_by_submit_time_ascending() {
        let mut a = job(1, "default", P::JobPending, 0);
        a.submit_time = Some(prost_types::Timestamp {
            seconds: 300,
            nanos: 0,
        });
        let mut b = job(2, "default", P::JobPending, 0);
        b.submit_time = Some(prost_types::Timestamp {
            seconds: 100,
            nanos: 0,
        });
        let mut jobs = vec![a, b];
        sort_jobs(&mut jobs, &parse_sort_arg("V").unwrap());
        assert_eq!(ids(&jobs), vec![2, 1]);
    }

    #[test]
    fn sort_by_partition_orders_lexically() {
        let mut jobs = vec![
            job(1, "gamma", P::JobRunning, 0),
            job(2, "alpha", P::JobRunning, 0),
            job(3, "beta", P::JobRunning, 0),
        ];
        sort_jobs(&mut jobs, &parse_sort_arg("P").unwrap());
        assert_eq!(ids(&jobs), vec![2, 3, 1]);
    }

    #[test]
    fn sort_by_time_left_puts_unlimited_last() {
        let mut a = job(1, "default", P::JobRunning, 0);
        a.time_limit = Some(prost_types::Duration {
            seconds: 600,
            nanos: 0,
        });
        a.run_time = Some(prost_types::Duration {
            seconds: 60,
            nanos: 0,
        });
        let b = job(2, "default", P::JobRunning, 0);
        let mut c = job(3, "default", P::JobRunning, 0);
        c.time_limit = Some(prost_types::Duration {
            seconds: 600,
            nanos: 0,
        });
        c.run_time = Some(prost_types::Duration {
            seconds: 500,
            nanos: 0,
        });
        let mut jobs = vec![a, b, c];
        sort_jobs(&mut jobs, &parse_sort_arg("L").unwrap());
        assert_eq!(ids(&jobs), vec![3, 1, 2]);
    }
}
