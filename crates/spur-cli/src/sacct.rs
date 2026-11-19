// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_core::accounting::{MAX_JOB_HISTORY_FETCH, MAX_JOB_HISTORY_ROWS};
use spur_proto::proto::GetJobHistoryRequest;

use crate::exit_fmt::format_exit;
use crate::format_engine;
use crate::timearg::{datetime_to_proto, parse_time_arg};
use crate::timefmt::format_duration_dhms as format_duration;

/// Display accounting data for jobs.
#[derive(Parser, Debug)]
#[command(name = "sacct", about = "Display accounting data for jobs")]
pub struct SacctArgs {
    /// Show jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Show jobs for this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Start time filter (e.g., "2024-01-01", "now-7days")
    #[arg(short = 'S', long)]
    pub starttime: Option<String>,

    /// End time filter
    #[arg(short = 'E', long)]
    pub endtime: Option<String>,

    /// Show only these states (comma-separated)
    #[arg(short = 's', long)]
    pub state: Option<String>,

    /// Show only these job IDs
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Output format
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Long format
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Brief format
    #[arg(short = 'b', long)]
    pub brief: bool,

    /// Don't print header
    #[arg(short = 'n', long)]
    pub noheader: bool,

    /// Accepted for Slurm compatibility; has no effect
    #[arg(long)]
    pub noconvert: bool,

    /// Output '|' delimited with a trailing '|'
    #[arg(short = 'p', long, overrides_with = "parsable2")]
    pub parsable: bool,

    /// Output '|' delimited without a trailing '|'
    #[arg(short = 'P', long, overrides_with = "parsable")]
    pub parsable2: bool,

    /// Maximum rows to print, at least 1. Without -j the default is 100. With
    /// -j and no --limit, every requested id is returned, up to 10000.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: Option<u32>,

    /// Controller address (accounting is served on the same port)
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

impl SacctArgs {
    fn output_style(&self) -> format_engine::OutputStyle {
        let layout = if self.parsable2 {
            format_engine::RowLayout::Delimited
        } else if self.parsable {
            format_engine::RowLayout::DelimitedTrailing
        } else {
            format_engine::RowLayout::Aligned
        };

        format_engine::OutputStyle::new(self.noheader, layout)
    }
}

const SACCT_DEFAULT_FORMAT: &str = "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x %18r";
const SACCT_LONG_FORMAT: &str =
    "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x %.10X %.19S %.19E %.10l %8y %8m %12q %18r";
const SACCT_BRIEF_FORMAT: &str = "%.8i %.8T %6x";

pub fn sacct_header(spec: char) -> &'static str {
    match spec {
        'i' => "JobID",
        'j' => "JobName",
        'u' => "User",
        'a' => "Account",
        'P' => "Partition",
        'T' => "State",
        't' => "State",
        'M' => "Elapsed",
        'D' => "NNodes",
        'x' => "ExitCode",
        'X' => "DerivedExitCode",
        'S' => "Start",
        'E' => "End",
        'V' => "Submit",
        'l' => "TimeLimit",
        'n' => "NodeList",
        'C' => "NCPUS",
        'R' => "ReqMem",
        'Q' => "QOS",
        'y' => "PreemptedBy",
        'm' => "PreemptMode",
        'q' => "PreemptQOS",
        'W' => "Borrowed",
        'G' => "ReqGPUS",
        'r' => "Reason",
        'z' => "ReasonDetail",
        'e' => "EndedBy",
        _ => "?",
    }
}

fn sacct_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "jobid" => Some('i'),
        "jobname" => Some('j'),
        "user" => Some('u'),
        "account" => Some('a'),
        "partition" => Some('P'),
        "state" => Some('T'),
        "elapsed" => Some('M'),
        "nnodes" => Some('D'),
        "exitcode" => Some('x'),
        "derivedexitcode" => Some('X'),
        "start" => Some('S'),
        "end" => Some('E'),
        "submit" => Some('V'),
        "timelimit" => Some('l'),
        "nodelist" => Some('n'),
        "ncpus" => Some('C'),
        "reqmem" => Some('R'),
        "qos" => Some('Q'),
        "preemptedby" => Some('y'),
        "preemptmode" => Some('m'),
        "preemptqos" => Some('q'),
        "borrowed" => Some('W'),
        "reqgpus" => Some('G'),
        "reason" => Some('r'),
        "reasondetail" => Some('z'),
        "endedby" => Some('e'),
        _ => None,
    }
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<SacctArgs>(&args);
    let style = args.output_style();
    let fields = output_fields(&args)?;

    let job_ids = args
        .jobs
        .as_deref()
        .map(crate::job_id_arg::parse_job_ids)
        .transpose()?
        .unwrap_or_default();
    let page = history_page(args.limit, job_ids.len());
    let request = history_request(&args, job_ids, &page)?;

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to controller")?;
    let mut client = spur_proto::accounting_client(channel);

    let response = client
        .get_job_history(request)
        .await
        .context("failed to get job history")?;

    let (jobs, truncated) = take_visible(response.into_inner().jobs, page.show);
    if truncated {
        eprintln!(
            "sacct: warning: results truncated at {}; more jobs matched",
            page.show
        );
    }
    for line in render(style, &fields, &jobs) {
        println!("{line}");
    }

    Ok(())
}

/// Printed rows for `sacct` with no `-j` and no `--limit`.
const DEFAULT_HISTORY_ROWS: u32 = 100;

#[derive(Debug)]
struct HistoryPage {
    show: u32,
    fetch: u32,
}

fn history_page(explicit: Option<u32>, job_ids: usize) -> HistoryPage {
    match explicit {
        Some(limit) => page_with_probe(limit),
        None if job_ids > 0 => page_for_job_ids(job_ids),
        None => page_with_probe(DEFAULT_HISTORY_ROWS),
    }
}

fn page_for_job_ids(job_ids: usize) -> HistoryPage {
    match u32::try_from(job_ids) {
        Ok(count) if count <= MAX_JOB_HISTORY_ROWS => HistoryPage {
            show: count,
            fetch: count,
        },
        _ => page_with_probe(MAX_JOB_HISTORY_ROWS),
    }
}

fn page_with_probe(limit: u32) -> HistoryPage {
    let show = limit.min(MAX_JOB_HISTORY_ROWS);
    HistoryPage {
        show,
        // Stay inside the cap the controller will actually return.
        fetch: show.saturating_add(1).min(MAX_JOB_HISTORY_FETCH),
    }
}

fn history_request(
    args: &SacctArgs,
    job_ids: Vec<u32>,
    page: &HistoryPage,
) -> Result<GetJobHistoryRequest> {
    let states = args
        .state
        .as_deref()
        .map(crate::squeue::parse_states_arg)
        .transpose()?
        .unwrap_or_default();
    let time_arg =
        |t: &Option<String>| t.as_deref().and_then(parse_time_arg).map(datetime_to_proto);
    Ok(GetJobHistoryRequest {
        user: args.user.clone().unwrap_or_default(),
        account: args.account.clone().unwrap_or_default(),
        start_after: time_arg(&args.starttime),
        start_before: time_arg(&args.endtime),
        states: states.into_iter().map(|state| state as i32).collect(),
        job_ids,
        limit: page.fetch,
    })
}

fn take_visible<T>(mut rows: Vec<T>, show: u32) -> (Vec<T>, bool) {
    let show = show as usize;
    if rows.len() <= show {
        return (rows, false);
    }
    rows.truncate(show);
    (rows, true)
}

fn output_fields(args: &SacctArgs) -> Result<Vec<format_engine::FormatToken>> {
    // -o/--format uses Slurm's comma-separated field-name syntax, not %-specifiers.
    if let Some(ref f) = args.format {
        let fields = format_engine::parse_named_format(f, &sacct_field_spec, &sacct_header);
        if fields.is_empty() {
            anyhow::bail!("sacct: no recognized fields in --format='{f}'");
        }
        return Ok(fields);
    }
    let format = if args.long {
        SACCT_LONG_FORMAT
    } else if args.brief {
        SACCT_BRIEF_FORMAT
    } else {
        SACCT_DEFAULT_FORMAT
    };
    Ok(format_engine::parse_format(format, &sacct_header))
}

fn render(
    style: format_engine::OutputStyle,
    fields: &[format_engine::FormatToken],
    jobs: &[spur_proto::proto::JobInfo],
) -> Vec<String> {
    let mut lines = style.header_lines(fields);
    lines.extend(
        jobs.iter()
            .map(|job| style.row(fields, &|spec| resolve_sacct_field(job, spec))),
    );
    lines
}

fn resolve_sacct_field(job: &spur_proto::proto::JobInfo, spec: char) -> String {
    match spec {
        'i' => job.job_id.to_string(),
        'j' => job.name.clone(),
        'u' => job.user.clone(),
        'a' => job.account.clone(),
        'P' => job.partition.clone(),
        'T' | 't' => state_name(job.state),
        'M' => format_elapsed(job),
        'D' => job.num_nodes.to_string(),
        'x' => format_exit(job.exit_code, job.exit_signal),
        'X' => format_exit(job.derived_exit_code, 0),
        'l' => match job.time_limit.as_ref() {
            Some(d) if d.seconds > 0 => format_duration(d.seconds),
            _ => "UNLIMITED".into(),
        },
        'S' => format_timestamp(job.start_time.as_ref()),
        'E' => format_timestamp(job.end_time.as_ref()),
        'V' => format_timestamp(job.submit_time.as_ref()),
        'n' => job.nodelist.clone(),
        'C' => (job.num_tasks * job.cpus_per_task.max(1)).to_string(),
        'Q' => job.qos.clone(),
        'y' => {
            if job.preempted_by == 0 {
                "N/A".into()
            } else {
                job.preempted_by.to_string()
            }
        }
        'm' => {
            if job.preempt_mode.is_empty() {
                "N/A".into()
            } else {
                job.preempt_mode.clone()
            }
        }
        'q' => {
            if job.preempt_qos.is_empty() {
                "N/A".into()
            } else {
                job.preempt_qos.clone()
            }
        }
        // ReqMem had a header and a name lookup but no resolver, so `--format=ReqMem`
        // rendered "?". Suffixed the way scontrol renders it, with a bare 0 for an
        // unset floor.
        'R' => match job.min_memory_node_mb {
            0 => "0".to_string(),
            mb => format!("{mb}M"),
        },
        // Whether the run took borrowed capacity. Same letter as squeue's %W so the
        // two commands agree on how to ask.
        'W' => if job.idle_fill { "yes" } else { "no" }.into(),
        'G' => job.req_gpus.to_string(),
        'r' => {
            if job.end_reason.is_empty() {
                "N/A".into()
            } else {
                job.end_reason.clone()
            }
        }
        'z' => job.end_reason_detail.clone(),
        'e' => {
            if job.ended_by.is_empty() {
                "N/A".into()
            } else {
                job.ended_by.clone()
            }
        }
        _ => "?".into(),
    }
}

fn state_name(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display().to_string())
        .unwrap_or_else(|| "UNKNOWN".into())
}

fn format_elapsed(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref rt) = job.run_time {
        format_duration(rt.seconds)
    } else {
        "00:00:00".into()
    }
}

fn format_timestamp(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "Unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::{JobInfo, JobState};

    fn job(exit_code: i32, exit_signal: i32, derived_exit_code: i32) -> JobInfo {
        JobInfo {
            exit_code,
            exit_signal,
            derived_exit_code,
            ..Default::default()
        }
    }

    #[test]
    fn reason_shows_the_recorded_code_and_na_for_older_rows() {
        let mut j = job(127, 0, 0);
        j.end_reason = "NonZeroExitCode".into();
        assert_eq!(resolve_sacct_field(&j, 'r'), "NonZeroExitCode");
        assert_eq!(resolve_sacct_field(&job(0, 0, 0), 'r'), "N/A");
    }

    #[test]
    fn reason_detail_is_blank_when_there_is_none() {
        let mut j = job(127, 0, 0);
        j.end_reason_detail = "exit 127: command not found (shell convention)".into();
        assert_eq!(
            resolve_sacct_field(&j, 'z'),
            "exit 127: command not found (shell convention)"
        );
        assert_eq!(resolve_sacct_field(&job(0, 0, 0), 'z'), "");
    }

    #[test]
    fn ended_by_names_the_canceller_or_na() {
        let mut j = job(0, 0, 0);
        j.ended_by = "alice".into();
        assert_eq!(resolve_sacct_field(&j, 'e'), "alice");
        assert_eq!(resolve_sacct_field(&job(0, 0, 0), 'e'), "N/A");
    }

    #[test]
    fn long_format_ends_with_reason() {
        assert!(SACCT_LONG_FORMAT.ends_with("%18r"));
    }

    #[test]
    fn default_output_ends_with_reason() {
        let args = SacctArgs::try_parse_from(["sacct"]).unwrap();
        let fields = output_fields(&args).unwrap();
        let mut cancelled = job(0, 0, 0);
        cancelled.end_reason = "UserCancel".into();
        let lines = render(args.output_style(), &fields, &[cancelled, job(0, 0, 0)]);
        let last = |line: &str| line.split_whitespace().last().map(str::to_owned);
        assert_eq!(last(&lines[0]).as_deref(), Some("Reason"));
        assert_eq!(last(&lines[2]).as_deref(), Some("UserCancel"));
        assert_eq!(last(&lines[3]).as_deref(), Some("N/A"));
    }

    #[test]
    fn noconvert_is_accepted() {
        // Slurm scripts pass --noconvert to keep output machine-parseable.
        let args = SacctArgs::try_parse_from(["sacct", "--noconvert"]).unwrap();
        assert!(args.noconvert);
    }

    #[test]
    fn exit_and_derived_fields_render() {
        // %x is Slurm's code:signal (signal half must survive from accounting);
        // %X is DerivedExitCode (running max over steps), signal half always 0.
        let j = job(3, 9, 7);
        assert_eq!(resolve_sacct_field(&j, 'x'), "3:9");
        assert_eq!(resolve_sacct_field(&j, 'X'), "7:0");
        assert_eq!(resolve_sacct_field(&job(0, 0, 0), 'x'), "0:0");
        assert_eq!(sacct_header('X'), "DerivedExitCode");
    }

    #[test]
    fn time_limit_field_renders() {
        // %l resolves to a formatted duration, or UNLIMITED when unset/zero.
        let mut j = job(0, 0, 0);
        assert_eq!(resolve_sacct_field(&j, 'l'), "UNLIMITED");
        j.time_limit = Some(prost_types::Duration {
            seconds: 3600,
            nanos: 0,
        });
        assert_eq!(resolve_sacct_field(&j, 'l'), format_duration(3600));
    }

    #[tokio::test]
    async fn unknown_state_fails_before_connecting_to_accounting() {
        for filter in ["COMPLTED", "FAILED,COMPLTED"] {
            let args = ["sacct", "--state", filter, "--controller", "invalid"]
                .map(str::to_string)
                .to_vec();
            let error = main_with_args(args).await.unwrap_err();
            assert!(error.to_string().contains("COMPLTED"), "{error}");
        }
    }

    #[test]
    fn borrowed_field_reports_whether_the_run_took_borrowed_capacity() {
        let borrowed = JobInfo {
            idle_fill: true,
            ..Default::default()
        };
        assert_eq!(resolve_sacct_field(&borrowed, 'W'), "yes");

        let ordinary = JobInfo::default();
        assert_eq!(resolve_sacct_field(&ordinary, 'W'), "no");
    }

    #[test]
    fn reqmem_renders_suffixed_matching_scontrol() {
        // Regression: this field was registered in the header and name tables but had no
        // resolver, so `sacct --format=ReqMem` printed "?". Found by the three-way
        // registration test below.
        let mut j = job(0, 0, 0);
        assert_eq!(resolve_sacct_field(&j, 'R'), "0");
        j.min_memory_node_mb = 16000;
        assert_eq!(resolve_sacct_field(&j, 'R'), "16000M");
    }

    #[test]
    fn reqgpus_resolved() {
        let mut j = job(0, 0, 0);
        j.req_gpus = 4;
        assert_eq!(resolve_sacct_field(&j, 'G'), "4");
    }

    #[test]
    fn every_named_field_resolves_to_a_header_and_a_value() {
        // A field is only usable when all three registrations agree: the name -> letter
        // lookup, the letter -> header name, and the letter -> value resolver. Half-
        // registering one is silent, and shipped documentation once described a field
        // that had a column and a header but no resolver, so this walks the whole set.
        let names = [
            "jobid",
            "jobname",
            "user",
            "account",
            "partition",
            "state",
            "elapsed",
            "nnodes",
            "exitcode",
            "derivedexitcode",
            "start",
            "end",
            "submit",
            "timelimit",
            "nodelist",
            "ncpus",
            "reqmem",
            "qos",
            "preemptedby",
            "preemptmode",
            "preemptqos",
            "borrowed",
            "reason",
            "reasondetail",
            "endedby",
            "reqgpus",
        ];
        let job = JobInfo::default();
        for name in names {
            let spec = sacct_field_spec(name).unwrap_or_else(|| panic!("{name} has no field spec"));
            assert_ne!(
                sacct_header(spec),
                "?",
                "{name} maps to '{spec}' which has no header name"
            );
            // An unresolved letter falls through to "?", so this catches a letter that
            // was added to the tables but never given a value.
            assert_ne!(
                resolve_sacct_field(&job, spec),
                "?",
                "{name} maps to '{spec}' which has no value resolver"
            );
        }
    }

    #[test]
    fn sacct_field_spec_is_case_insensitive_and_rejects_unknown_names() {
        for name in ["JobID", "jobid", "JOBID"] {
            assert_eq!(sacct_field_spec(name), Some('i'));
        }
        assert_eq!(sacct_field_spec("NotAField"), None);
    }

    #[test]
    fn custom_format_comma_separated_names_populate_requested_columns() {
        // Real Slurm's sacct --format syntax; used to produce blank columns.
        let fields = format_engine::parse_named_format(
            "JobID,JobName,Partition,State",
            &sacct_field_spec,
            &sacct_header,
        );
        let field_count = fields
            .iter()
            .filter(|t| matches!(t, format_engine::FormatToken::Field(_)))
            .count();
        assert_eq!(field_count, 4);
        let mut j = job(0, 0, 0);
        j.name = "train".into();
        j.partition = "gpu".into();
        j.job_id = 42;
        let row = format_engine::format_row(&fields, &|spec| resolve_sacct_field(&j, spec));
        assert!(row.contains("42"));
        assert!(row.contains("train"));
        assert!(row.contains("gpu"));
    }

    fn coco_fields() -> Vec<format_engine::FormatToken> {
        format_engine::parse_named_format(
            "JobID,State,ExitCode,NodeList",
            &sacct_field_spec,
            &sacct_header,
        )
    }

    fn history() -> Vec<JobInfo> {
        vec![
            JobInfo {
                job_id: 34,
                state: JobState::JobTimeout as i32,
                exit_code: 143,
                nodelist: "node1".into(),
                ..Default::default()
            },
            JobInfo {
                job_id: 35,
                state: JobState::JobCancelled as i32,
                ..Default::default()
            },
        ]
    }

    fn render_with(flags: &[&str]) -> Vec<String> {
        let args = SacctArgs::try_parse_from(std::iter::once("sacct").chain(flags.iter().copied()))
            .unwrap();
        render(args.output_style(), &coco_fields(), &history())
    }

    #[test]
    fn parsable2_is_pipe_delimited_with_no_trailing_pipe() {
        let expected = [
            "JobID|State|ExitCode|NodeList",
            "34|TIMEOUT|143:0|node1",
            "35|CANCELLED|0:0|",
        ];
        assert_eq!(render_with(&["-P"]), expected);
        assert_eq!(render_with(&["--parsable2"]), expected);
    }

    #[test]
    fn parsable_ends_every_line_with_a_pipe() {
        let expected = [
            "JobID|State|ExitCode|NodeList|",
            "34|TIMEOUT|143:0|node1|",
            "35|CANCELLED|0:0||",
        ];
        assert_eq!(render_with(&["-p"]), expected);
        assert_eq!(render_with(&["--parsable"]), expected);
    }

    #[test]
    fn noheader_drops_the_parsable_header_line() {
        assert_eq!(
            render_with(&["-n", "-P"]),
            ["34|TIMEOUT|143:0|node1", "35|CANCELLED|0:0|"]
        );
    }

    #[test]
    fn the_last_parsable_flag_wins_as_in_slurm() {
        assert_eq!(render_with(&["-p", "-P"]), render_with(&["-P"]));
        assert_eq!(render_with(&["-P", "-p"]), render_with(&["-p"]));
        assert_eq!(
            render_with(&["--parsable2", "--parsable"]),
            render_with(&["-p"])
        );
    }

    #[test]
    fn without_a_parsable_flag_the_table_stays_aligned() {
        let lines = render_with(&[]);
        assert_eq!(lines.len(), 4, "header, rule, two rows: {lines:?}");
        assert!(lines.iter().all(|l| !l.contains('|')), "{lines:?}");
        assert!(lines[1].starts_with("-----"), "{lines:?}");
    }

    #[tokio::test]
    async fn format_with_no_recognized_fields_fails_fast() {
        // Errors before ever connecting, so no server/network is needed here.
        let err = main_with_args(vec![
            "sacct".into(),
            "--format=NotAField,AlsoNotAField".into(),
        ])
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no recognized fields"));
    }

    #[test]
    fn job_id_list_is_fetched_in_full_when_limit_is_omitted() {
        let page = history_page(None, 150);
        assert_eq!((page.show, page.fetch), (150, 150));
    }

    #[test]
    fn an_open_query_fetches_one_past_the_default_page() {
        let page = history_page(None, 0);
        assert_eq!((page.show, page.fetch), (100, 101));
    }

    #[test]
    fn an_open_query_asks_the_server_for_the_probe_row() {
        let args = SacctArgs::try_parse_from(["sacct"]).unwrap();
        let page = history_page(args.limit, 0);
        let req = history_request(&args, vec![], &page).unwrap();
        assert_eq!(req.limit, DEFAULT_HISTORY_ROWS + 1);
    }

    #[test]
    fn a_job_id_list_asks_the_server_for_every_parsed_id() {
        let args = SacctArgs::try_parse_from(["sacct", "-j", "30.0,30.1"]).unwrap();
        let ids = crate::job_id_arg::parse_job_ids(args.jobs.as_deref().unwrap()).unwrap();
        let page = history_page(args.limit, ids.len());
        let req = history_request(&args, ids, &page).unwrap();
        assert_eq!(req.job_ids, vec![30, 30]);
        assert_eq!(req.limit, 2);
    }

    #[test]
    fn history_request_carries_the_user_state_and_time_filters() {
        let args = SacctArgs::try_parse_from([
            "sacct",
            "-u",
            "alice",
            "-A",
            "research",
            "-s",
            "FAILED,CD",
            "-S",
            "2026-07-01",
        ])
        .unwrap();
        let req = history_request(&args, vec![], &history_page(args.limit, 0)).unwrap();
        assert_eq!(req.user, "alice");
        assert_eq!(req.account, "research");
        assert_eq!(req.states.len(), 2);
        assert!(req.start_after.is_some());
        assert!(req.start_before.is_none());
    }

    #[test]
    fn an_explicit_limit_wins_over_a_longer_job_id_list() {
        let page = history_page(Some(10), 150);
        assert_eq!((page.show, page.fetch), (10, 11));
    }

    #[test]
    fn a_zero_limit_is_rejected() {
        let err = SacctArgs::try_parse_from(["sacct", "--limit", "0"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--limit"), "{msg}");
        assert!(msg.contains('1'), "{msg}");
    }

    #[test]
    fn a_job_id_list_at_the_cap_needs_no_probe_row() {
        let page = history_page(None, MAX_JOB_HISTORY_ROWS as usize);
        assert_eq!(
            (page.show, page.fetch),
            (MAX_JOB_HISTORY_ROWS, MAX_JOB_HISTORY_ROWS)
        );
    }

    #[test]
    fn a_job_id_list_past_the_cap_asks_for_one_probe_row() {
        let page = history_page(None, (MAX_JOB_HISTORY_ROWS as usize) + 5_000);
        assert_eq!(
            (page.show, page.fetch),
            (MAX_JOB_HISTORY_ROWS, MAX_JOB_HISTORY_FETCH)
        );
    }

    #[test]
    fn an_explicit_limit_above_the_cap_keeps_one_probe_row() {
        let page = history_page(Some(MAX_JOB_HISTORY_ROWS + 10_000), 0);
        assert_eq!(
            (page.show, page.fetch),
            (MAX_JOB_HISTORY_ROWS, MAX_JOB_HISTORY_FETCH)
        );
    }

    #[test]
    fn take_visible_drops_the_probe_row_and_reports_the_cut() {
        let (rows, truncated) = take_visible(vec!["a", "b", "c"], 2);
        assert_eq!(rows, ["a", "b"]);
        assert!(truncated);
        let (rows, truncated) = take_visible(vec!["a", "b"], 2);
        assert_eq!(rows, ["a", "b"]);
        assert!(!truncated);
    }
}
