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
    #[arg(short = 'u', long, overrides_with = "allusers")]
    pub user: Option<String>,

    /// Show jobs for all users you may read (the default); the last of -a and -u wins
    #[arg(short = 'a', long, overrides_with_all = ["allusers", "user"])]
    pub allusers: bool,

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

    /// Accepted for Slurm compatibility; history already has one row per job
    #[arg(short = 'X', long, overrides_with = "allocations")]
    pub allocations: bool,

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

/// `--format` names and their letters. Names can share a letter (`AllocCPUS` is
/// `NCPUS`); the first name listed is that letter's header in the built-in layouts.
const SACCT_FORMAT_FIELDS: &[(&str, char)] = &[
    ("JobID", 'i'),
    ("JobIDRaw", 'I'),
    ("JobName", 'j'),
    ("User", 'u'),
    ("Account", 'a'),
    ("Partition", 'P'),
    ("State", 'T'),
    ("Elapsed", 'M'),
    ("ElapsedRaw", 'K'),
    ("NNodes", 'D'),
    ("ExitCode", 'x'),
    ("DerivedExitCode", 'X'),
    ("Start", 'S'),
    ("End", 'E'),
    ("Submit", 'V'),
    ("TimeLimit", 'l'),
    ("NodeList", 'n'),
    ("NCPUS", 'C'),
    ("AllocCPUS", 'C'),
    ("ReqMem", 'R'),
    ("QOS", 'Q'),
    ("Reservation", 'v'),
    ("PreemptedBy", 'y'),
    ("PreemptMode", 'm'),
    ("PreemptQOS", 'q'),
    ("Borrowed", 'W'),
    ("ReqGPUS", 'G'),
    ("Reason", 'r'),
    ("ReasonDetail", 'z'),
    ("EndedBy", 'e'),
];

pub fn sacct_header(spec: char) -> &'static str {
    SACCT_FORMAT_FIELDS
        .iter()
        .find(|&&(_, field_spec)| field_spec == spec)
        .map_or("?", |&(name, _)| name)
}

/// Shared letter for every [`SLURM_ONLY_FORMAT_FIELDS`] name; it renders as `N/A`.
const NOT_PROVIDED: char = '-';

/// Slurm `sacct` fields Spur does not provide. They print as an `N/A` column under
/// their Slurm name, so a Slurm script asking for one keeps its column positions.
const SLURM_ONLY_FORMAT_FIELDS: &[&str] = &[
    "AdminComment",
    "AllocNodes",
    "AllocTRES",
    "AssocID",
    "AveCPU",
    "AveCPUFreq",
    "AveDiskRead",
    "AveDiskWrite",
    "AvePages",
    "AvePids",
    "AveRSS",
    "AveVMSize",
    "Billing",
    "BillingRaw",
    "BlockID",
    "Cluster",
    "Comment",
    "Constraints",
    "ConsumedEnergy",
    "ConsumedEnergyRaw",
    "Container",
    "CPUTime",
    "CPUTimeRAW",
    "DBIndex",
    "Eligible",
    "Exclusive",
    "Extra",
    "FailedNode",
    "Flags",
    "GID",
    "Group",
    "Layout",
    "Licenses",
    "MaxDiskRead",
    "MaxDiskReadNode",
    "MaxDiskReadTask",
    "MaxDiskWrite",
    "MaxDiskWriteNode",
    "MaxDiskWriteTask",
    "MaxPages",
    "MaxPagesNode",
    "MaxPagesTask",
    "MaxPids",
    "MaxPidsNode",
    "MaxPidsTask",
    "MaxRSS",
    "MaxRSSNode",
    "MaxRSSTask",
    "MaxVMSize",
    "MaxVMSizeNode",
    "MaxVMSizeTask",
    "McsLabel",
    "MinCPU",
    "MinCPUNode",
    "MinCPUTask",
    "NTasks",
    "OriginalSLUID",
    "OverSubscribe",
    "Planned",
    "PlannedCPU",
    "PlannedCPURAW",
    "Priority",
    "QOSRAW",
    "QOSREQ",
    "ReqCPUFreq",
    "ReqCPUFreqGov",
    "ReqCPUFreqMax",
    "ReqCPUFreqMin",
    "ReqCPUS",
    "ReqNodes",
    "ReqTRES",
    "ReservationId",
    "ReqReservation",
    "Restarts",
    "SegmentSize",
    "SLUID",
    "SlurmRC",
    "StdErr",
    "StdIn",
    "StdOut",
    "SubmitLine",
    "Suspended",
    "SystemComment",
    "SystemCPU",
    "TimelimitRaw",
    "TotalCPU",
    "TRESUsageInAve",
    "TRESUsageInMax",
    "TRESUsageInMaxNode",
    "TRESUsageInMaxTask",
    "TRESUsageInMin",
    "TRESUsageInMinNode",
    "TRESUsageInMinTask",
    "TRESUsageInTot",
    "TRESUsageOutAve",
    "TRESUsageOutMax",
    "TRESUsageOutMaxNode",
    "TRESUsageOutMaxTask",
    "TRESUsageOutMin",
    "TRESUsageOutMinNode",
    "TRESUsageOutMinTask",
    "TRESUsageOutTot",
    "UID",
    "UserCPU",
    "WCKey",
    "WCKeyID",
    "WorkDir",
];

/// The letter and header for a `--format` name, matched in full and ignoring case.
fn sacct_field(name: &str) -> Option<(char, &'static str)> {
    if let Some(&(field, spec)) = SACCT_FORMAT_FIELDS
        .iter()
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
    {
        return Some((spec, field));
    }
    SLURM_ONLY_FORMAT_FIELDS
        .iter()
        .find(|field| field.eq_ignore_ascii_case(name))
        .map(|&field| (NOT_PROVIDED, field))
}

fn supported_format_fields() -> String {
    SACCT_FORMAT_FIELDS
        .iter()
        .map(|(field, _)| *field)
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<SacctArgs>(&args);
    let style = args.output_style();
    let fields = output_fields(&args)?;
    let (request, page) = job_history_request(&args)?;

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

fn job_history_request(args: &SacctArgs) -> Result<(GetJobHistoryRequest, HistoryPage)> {
    let job_ids = args
        .jobs
        .as_deref()
        .map(crate::job_id_arg::parse_job_ids)
        .transpose()?
        .unwrap_or_default();
    let page = history_page(args.limit, job_ids.len());
    let request = history_request(args, job_ids, &page)?;
    Ok((request, page))
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
        let fields = format_engine::parse_named_format_strict(f, &sacct_field)
            .map_err(|e| anyhow::anyhow!("sacct: {e}\nSupported: {}", supported_format_fields()))?;
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
        // Slurm's JobID gains array and het suffixes (`123_4`); JobIDRaw never does.
        'I' => job.job_id.to_string(),
        'j' => job.name.clone(),
        'u' => job.user.clone(),
        'a' => job.account.clone(),
        'P' => job.partition.clone(),
        'T' => state_name(job.state),
        'M' => format_elapsed(job),
        // The magnitude, as Elapsed prints it, so the two never disagree.
        'K' => job
            .run_time
            .as_ref()
            .map_or(0, |rt| rt.seconds.unsigned_abs())
            .to_string(),
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
        'C' => ncpus(job).to_string(),
        'Q' => job.qos.clone(),
        // Blank without a reservation, as in Slurm and squeue's %v.
        'v' => job.reservation.clone(),
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
        NOT_PROVIDED => "N/A".into(),
        _ => "?".into(),
    }
}

/// CPU count a job holds: a history row's recorded allocation, else the `cpu=`
/// entry in `req_tres` (already counts `--cpus-per-gpu`), else
/// `num_tasks * cpus_per_task.max(1)` for rows that carry neither.
pub(crate) fn ncpus(job: &spur_proto::proto::JobInfo) -> u32 {
    if let Some(cpus) = job.alloc_cpus {
        return u32::try_from(cpus).unwrap_or(u32::MAX);
    }
    job.req_tres
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(key, _)| *key == "cpu")
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or_else(|| job.num_tasks.saturating_mul(job.cpus_per_task.max(1)))
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
        let mut j = job(0, 0, 0);
        assert_eq!(resolve_sacct_field(&j, 'R'), "0");
        j.min_memory_node_mb = 16000;
        assert_eq!(resolve_sacct_field(&j, 'R'), "16000M");
    }

    #[test]
    fn ncpus_reads_a_history_rows_allocated_cpus() {
        // A history row carries no req_tres; a one-task --cpus-per-gpu=8 job on
        // two GPUs held 16 CPUs.
        let j = JobInfo {
            alloc_cpus: Some(16),
            num_tasks: 1,
            cpus_per_task: 1,
            ..Default::default()
        };
        assert_eq!(ncpus(&j), 16);
        assert_eq!(resolve_sacct_field(&j, 'C'), "16");
    }

    #[test]
    fn ncpus_uses_the_req_tres_cpu_value_when_present() {
        // req_tres already counts --cpus-per-gpu, so it must win over the naive
        // num_tasks * cpus_per_task product.
        let j = JobInfo {
            req_tres: "cpu=16,node=1".into(),
            num_tasks: 1,
            cpus_per_task: 1,
            ..Default::default()
        };
        assert_eq!(ncpus(&j), 16);
        assert_eq!(resolve_sacct_field(&j, 'C'), "16");
    }

    #[test]
    fn ncpus_falls_back_to_tasks_times_cpus_per_task_when_req_tres_is_empty() {
        // Accounting-history rows may predate req_tres and leave it empty.
        let j = JobInfo {
            num_tasks: 4,
            cpus_per_task: 2,
            ..Default::default()
        };
        assert_eq!(ncpus(&j), 8);
    }

    #[test]
    fn ncpus_falls_back_when_req_tres_has_no_cpu_entry_or_fails_to_parse() {
        let no_cpu_key = JobInfo {
            req_tres: "node=1,mem=1000M".into(),
            num_tasks: 2,
            cpus_per_task: 1,
            ..Default::default()
        };
        assert_eq!(ncpus(&no_cpu_key), 2);

        let malformed = JobInfo {
            req_tres: "cpu=bogus".into(),
            num_tasks: 3,
            cpus_per_task: 1,
            ..Default::default()
        };
        assert_eq!(ncpus(&malformed), 3);
    }

    #[test]
    fn reqgpus_resolved() {
        let mut j = job(0, 0, 0);
        j.req_gpus = 4;
        assert_eq!(resolve_sacct_field(&j, 'G'), "4");
    }

    #[test]
    fn every_named_field_resolves_to_a_header_and_a_value() {
        // A letter in the table without a resolver arm renders "?" and nothing else
        // notices, so this walks the whole table.
        let job = JobInfo::default();
        for (name, expected_spec) in SACCT_FORMAT_FIELDS {
            let (spec, header) =
                sacct_field(name).unwrap_or_else(|| panic!("{name} has no field spec"));
            assert_eq!(spec, *expected_spec, "{name}");
            assert_eq!(header, *name);
            assert_ne!(
                resolve_sacct_field(&job, spec),
                "?",
                "{name} maps to '{spec}' which has no value resolver"
            );
        }
    }

    #[test]
    fn sacct_field_is_case_insensitive_and_rejects_unknown_names() {
        for name in ["JobID", "jobid", "JOBID"] {
            assert_eq!(sacct_field(name), Some(('i', "JobID")));
        }
        assert_eq!(sacct_field("maxrss"), Some((NOT_PROVIDED, "MaxRSS")));
        assert_eq!(sacct_field("NotAField"), None);
    }

    #[test]
    fn no_slurm_only_field_is_one_spur_provides() {
        for name in SLURM_ONLY_FORMAT_FIELDS {
            assert!(
                !SACCT_FORMAT_FIELDS
                    .iter()
                    .any(|(field, _)| field.eq_ignore_ascii_case(name)),
                "{name} is in both field tables"
            );
        }
    }

    #[test]
    fn a_slurm_field_spur_does_not_provide_is_an_na_column_under_its_slurm_name() {
        let args = SacctArgs::try_parse_from(["sacct", "-P", "--format=JobID,maxrss,AllocTRES%10"])
            .unwrap();
        let fields = output_fields(&args).unwrap();
        let ran = JobInfo {
            job_id: 7,
            ..Default::default()
        };
        assert_eq!(
            render(args.output_style(), &fields, &[ran]),
            ["JobID|MaxRSS|AllocTRES", "7|N/A|N/A"]
        );
    }

    #[test]
    fn alloc_cpus_is_ncpus_under_its_own_header() {
        let args = SacctArgs::try_parse_from(["sacct", "-P", "--format=NCPUS,AllocCPUS"]).unwrap();
        let fields = output_fields(&args).unwrap();
        let ran = JobInfo {
            alloc_cpus: Some(16),
            ..Default::default()
        };
        assert_eq!(
            render(args.output_style(), &fields, &[ran]),
            ["NCPUS|AllocCPUS", "16|16"]
        );
    }

    #[test]
    fn raw_fields_print_the_bare_id_and_elapsed_seconds() {
        let args = SacctArgs::try_parse_from([
            "sacct",
            "-P",
            "--format=JobID,JobIDRaw,Elapsed,ElapsedRaw",
        ])
        .unwrap();
        let fields = output_fields(&args).unwrap();
        let ran = JobInfo {
            job_id: 42,
            run_time: Some(prost_types::Duration {
                seconds: 3725,
                nanos: 0,
            }),
            ..Default::default()
        };
        let never_started = JobInfo {
            job_id: 43,
            ..Default::default()
        };
        assert_eq!(
            render(args.output_style(), &fields, &[ran, never_started]),
            [
                "JobID|JobIDRaw|Elapsed|ElapsedRaw",
                "42|42|01:02:05|3725",
                "43|43|00:00:00|0"
            ]
        );
    }

    #[test]
    fn every_built_in_layout_letter_has_a_header() {
        let layouts: [&[&str]; 3] = [&[], &["-l"], &["-b"]];
        for flags in layouts {
            let args =
                SacctArgs::try_parse_from(std::iter::once("sacct").chain(flags.iter().copied()))
                    .unwrap();
            for token in output_fields(&args).unwrap() {
                if let format_engine::FormatToken::Field(field) = token {
                    assert_ne!(field.header, "?", "{flags:?} uses '{}'", field.spec);
                }
            }
        }
    }

    #[test]
    fn a_shared_letter_takes_the_first_listed_name_as_its_header() {
        assert_eq!(sacct_header('C'), "NCPUS");
        assert_eq!(sacct_header('?'), "?");
    }

    #[test]
    fn reservation_prints_the_recorded_name_or_nothing() {
        let reserved = JobInfo {
            reservation: "maint".into(),
            ..Default::default()
        };
        assert_eq!(resolve_sacct_field(&reserved, 'v'), "maint");
        assert_eq!(resolve_sacct_field(&JobInfo::default(), 'v'), "");
    }

    #[test]
    fn custom_format_comma_separated_names_populate_requested_columns() {
        // Real Slurm's sacct --format syntax; used to produce blank columns.
        let fields =
            format_engine::parse_named_format_strict("JobID,JobName,Partition,State", &sacct_field)
                .unwrap();
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
        format_engine::parse_named_format_strict("JobID,State,ExitCode,NodeList", &sacct_field)
            .unwrap()
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

    #[test]
    fn format_with_only_unknown_fields_names_the_first() {
        let args =
            SacctArgs::try_parse_from(["sacct", "--format=NotAField,AlsoNotAField"]).unwrap();
        let message = output_fields(&args).unwrap_err().to_string();
        assert!(
            message.starts_with("sacct: Invalid field requested: NotAField\nSupported: "),
            "{message}"
        );
    }

    #[test]
    fn format_with_one_unknown_field_names_it_and_lists_the_supported_ones() {
        let args = SacctArgs::try_parse_from(["sacct", "--format=JobID,Bogus"]).unwrap();
        let message = output_fields(&args).unwrap_err().to_string();
        assert!(
            message.starts_with("sacct: Invalid field requested: Bogus\nSupported: JobID, "),
            "{message}"
        );
    }

    #[test]
    fn unknown_field_with_a_width_reports_the_name_only() {
        let args = SacctArgs::try_parse_from(["sacct", "--format=JobID,Bogus%10"]).unwrap();
        let message = output_fields(&args).unwrap_err().to_string();
        assert!(
            message.starts_with("sacct: Invalid field requested: Bogus\n"),
            "{message}"
        );
    }

    #[test]
    fn a_width_suffix_does_not_make_a_known_field_unknown() {
        let args = SacctArgs::try_parse_from(["sacct", "--format=JobName%20,JobID%-8"]).unwrap();
        let fields = output_fields(&args).unwrap();
        let specs: Vec<char> = fields
            .iter()
            .filter_map(|token| match token {
                format_engine::FormatToken::Field(field) => Some(field.spec),
                format_engine::FormatToken::Literal(_) => None,
            })
            .collect();
        assert_eq!(specs, vec!['j', 'i']);
    }

    #[test]
    fn format_with_only_commas_still_errors() {
        let args = SacctArgs::try_parse_from(["sacct", "--format=,,,"]).unwrap();
        let err = output_fields(&args).unwrap_err();
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

    fn parse(argv: &[&str]) -> SacctArgs {
        SacctArgs::try_parse_from(std::iter::once("sacct").chain(argv.iter().copied())).unwrap()
    }

    fn request(args: &SacctArgs) -> GetJobHistoryRequest {
        job_history_request(args).unwrap().0
    }

    const ISSUE_ARGV: [&[&str]; 3] = [
        &[
            "-a",
            "--allusers",
            "--parsable2",
            "-n",
            "-X",
            "--format=JobID",
            "-S",
            "2026-11-22",
            "-E",
            "2026-11-23",
        ],
        &[
            "--allusers",
            "--parsable2",
            "-n",
            "-X",
            "--format=JobID",
            "-S",
            "2026-11-22",
            "-E",
            "2026-11-23",
        ],
        &[
            "--parsable2",
            "-n",
            "-X",
            "--format=JobID",
            "-S",
            "2026-11-22",
            "-E",
            "2026-11-23",
        ],
    ];

    fn utc_midnight(year: i32, month: u32, day: u32) -> prost_types::Timestamp {
        let seconds = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        prost_types::Timestamp { seconds, nanos: 0 }
    }

    #[test]
    fn the_issue_command_lines_request_every_permitted_user_in_the_window() {
        for argv in ISSUE_ARGV {
            let req = request(&parse(argv));
            assert_eq!(req.user, "", "{argv:?}");
            assert_eq!(
                req.start_after,
                Some(utc_midnight(2026, 11, 22)),
                "{argv:?}"
            );
            assert_eq!(
                req.start_before,
                Some(utc_midnight(2026, 11, 23)),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn the_issue_command_lines_print_bare_job_ids() {
        for argv in ISSUE_ARGV {
            let args = parse(argv);
            let lines = render(
                args.output_style(),
                &output_fields(&args).unwrap(),
                &history(),
            );
            assert_eq!(lines, ["34", "35"], "{argv:?}");
        }
    }

    #[test]
    fn the_last_of_allusers_and_user_wins_as_in_slurm() {
        let cases: [(&[&str], &str); 6] = [
            (&["-u", "alice", "-a"], ""),
            (&["-a", "-u", "alice"], "alice"),
            (&["--user", "alice", "--allusers"], ""),
            (&["--allusers", "--user", "alice"], "alice"),
            (&["-u", "bob", "-a", "-u", "carol"], "carol"),
            (&["-a", "-u", "bob", "-a"], ""),
        ];
        for (argv, user) in cases {
            assert_eq!(request(&parse(argv)).user, user, "{argv:?}");
        }
    }

    #[test]
    fn allusers_and_allocations_tolerate_repeats() {
        for argv in [["-a", "--allusers"], ["-a", "-a"]] {
            assert!(parse(&argv).allusers, "{argv:?}");
        }
        for argv in [["-X", "--allocations"], ["-X", "-X"]] {
            assert!(parse(&argv).allocations, "{argv:?}");
        }
    }

    #[test]
    fn clustered_short_flags_parse() {
        let args = parse(&["-aX"]);
        assert!(args.allusers && args.allocations);

        let args = parse(&["-anPX"]);
        assert!(args.allusers && args.noheader && args.parsable2 && args.allocations);
    }

    #[test]
    fn allocations_does_not_change_the_request() {
        let bases: [&[&str]; 2] = [&["-S", "2026-11-22"], &["-j", "30.0", "-u", "alice"]];
        for base in bases {
            let with_x: Vec<&str> = base.iter().copied().chain(["-X"]).collect();
            assert_eq!(request(&parse(&with_x)), request(&parse(base)), "{base:?}");
        }
        assert_eq!(
            request(&parse(&["-j", "30.0", "-u", "alice", "-X"])).job_ids,
            [30]
        );
    }

    #[test]
    fn help_lists_allusers_and_allocations() {
        let help = <SacctArgs as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        for flag in ["-a, --allusers", "-X, --allocations"] {
            assert!(help.contains(flag), "{flag} missing from:\n{help}");
        }
    }
}
