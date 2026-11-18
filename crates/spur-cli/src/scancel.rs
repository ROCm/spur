// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{CancelJobRequest, SignalJobRequest, SignalTarget};

/// Covers the controller waiting on every node's agent to deliver the signal,
/// each bounded at a few seconds, plus a forward to the leader.
const SIGNAL_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type Client = SlurmControllerClient<crate::authclient::AuthChannel>;

/// What scancel does to each selected job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Cancel,
    Signal { signal: i32, target: SignalTarget },
}

/// Cancel or signal pending or running jobs.
#[derive(Parser, Debug)]
#[command(name = "scancel", about = "Cancel or signal jobs")]
pub struct ScancelArgs {
    /// Job IDs to cancel or signal
    pub job_ids: Vec<u32>,

    /// Cancel or signal all jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Cancel or signal jobs in this partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Cancel or signal jobs in this state
    #[arg(short = 't', long)]
    pub state: Option<String>,

    /// Cancel or signal jobs with this name
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Cancel or signal jobs for this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Cancel or signal jobs in this reservation
    #[arg(short = 'R', long)]
    pub reservation: Option<String>,

    /// Signal to send (default: cancel). Any signal other than KILL leaves the
    /// job running; it goes to the job's srun steps unless -b or -f is given
    #[arg(short = 's', long)]
    pub signal: Option<String>,

    /// With --signal, signal only the batch script's shell process, not its
    /// children or any step
    #[arg(short = 'b', long)]
    pub batch: bool,

    /// With --signal, signal the steps, the batch script and its children.
    /// Takes precedence over --batch
    #[arg(short = 'f', long)]
    pub full: bool,

    /// Quiet mode
    #[arg(short = 'Q', long)]
    pub quiet: bool,

    /// Interactive: confirm each cancellation or signal
    #[arg(short = 'i', long)]
    pub interactive: bool,

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
    let args = crate::clap_exit::parse_or_exit::<ScancelArgs>(&args);

    if !has_selection(&args) {
        bail!("scancel: no job IDs or filters specified");
    }
    reject_empty_filters(&args)?;

    let action = parse_action(args.signal.as_deref(), args.batch, args.full)?;

    // Requester identity for authorization on each cancel. The server checks
    // this against the job owner (root/empty bypass), so it must be the caller.
    let requester = match &args.user {
        Some(user) => user.clone(),
        None => crate::interactive::current_user()?,
    };
    let filter_user = filter_user(&args, &requester);

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    let job_ids = if args.job_ids.is_empty() {
        select_jobs(&mut client, &args, &action, filter_user).await?
    } else {
        args.job_ids.clone()
    };

    let mut failed = false;
    for job_id in job_ids {
        if let Err(status) = send(&mut client, &action, job_id, &requester).await {
            failed |= report_refusal(job_id, &status, &action, args.quiet);
        }
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve a filter selection to the jobs to act on.
async fn select_jobs(
    client: &mut Client,
    args: &ScancelArgs,
    action: &Action,
    user: String,
) -> Result<Vec<u32>> {
    let states = match args.state.as_deref() {
        Some(state) => filter_states(Some(state))?,
        None => default_states(action),
    };

    let response = client
        .get_jobs(spur_proto::proto::GetJobsRequest {
            states,
            user,
            partition: args.partition.clone().unwrap_or_default(),
            account: args.account.clone().unwrap_or_default(),
            job_ids: Vec::new(),
            name: args.name.clone().unwrap_or_default(),
            nodes: Vec::new(),
            qos: String::new(),
            reservation: args.reservation.clone().unwrap_or_default(),
        })
        .await
        .context("failed to get jobs")?;

    // Filter-based selection targets only live jobs. Terminal jobs matched by
    // the filter (e.g. a user's already-finished jobs under `scancel -u`) are
    // skipped rather than sent to the server, which would reject each one and
    // emit a spurious per-job error. Matches Slurm.
    Ok(response
        .into_inner()
        .jobs
        .iter()
        .filter(|job| is_cancellable(job.state))
        .map(|job| job.job_id)
        .collect())
}

/// Cancel through `CancelJob`; deliver a signal through `SignalJob`, which
/// leaves the job running.
async fn send(
    client: &mut Client,
    action: &Action,
    job_id: u32,
    user: &str,
) -> Result<(), tonic::Status> {
    match *action {
        Action::Cancel => {
            client
                .cancel_job(CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: user.to_string(),
                })
                .await?;
        }
        Action::Signal { signal, target } => {
            let mut request = tonic::Request::new(SignalJobRequest {
                job_id,
                signal,
                target: target as i32,
                user: user.to_string(),
            });
            request.set_timeout(SIGNAL_RPC_TIMEOUT);
            client.signal_job(request).await?;
        }
    }
    Ok(())
}

/// Print a refused action unless it is ignored, and return whether it fails the command.
fn report_refusal(job_id: u32, status: &tonic::Status, action: &Action, quiet: bool) -> bool {
    if should_report(status.code(), action, quiet) {
        eprintln!("{}", refusal_text(job_id, status, action));
    }
    counts_as_failure(status.code(), action)
}

fn refusal_text(job_id: u32, status: &tonic::Status, action: &Action) -> String {
    match action {
        Action::Cancel => format!(
            "scancel: error cancelling job {job_id}: {}",
            status.message()
        ),
        Action::Signal { .. } if status.code() == tonic::Code::Unimplemented => format!(
            "scancel: error signalling job {job_id}: the controller does not support signalling \
             jobs; upgrade spurctld, or use plain scancel to end the job"
        ),
        Action::Signal { .. } => format!(
            "scancel: error signalling job {job_id}: {}",
            status.message()
        ),
    }
}

/// Slurm's scancel stays silent on an ignored refusal, and `-Q` also hides a
/// job that is already gone when signalling it.
fn should_report(code: tonic::Code, action: &Action, quiet: bool) -> bool {
    counts_as_failure(code, action) && !(quiet && already_gone(code))
}

/// Whether a refused action makes scancel exit non-zero. Like Slurm, a job that is
/// unknown or already finished does not fail a cancel (it is gone either way), but
/// does fail a signal, and anything else, such as another user's job, fails both.
fn counts_as_failure(code: tonic::Code, action: &Action) -> bool {
    !(*action == Action::Cancel && already_gone(code))
}

fn already_gone(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::NotFound | tonic::Code::FailedPrecondition
    )
}

/// Whether a job in the given proto state can still be cancelled. Unknown
/// state values are treated as cancellable so the server remains the
/// authority on rejection rather than the client silently dropping them.
fn is_cancellable(proto_state: i32) -> bool {
    match spur_core::job::JobState::from_proto_i32(proto_state) {
        Some(state) => !state.is_terminal(),
        None => true,
    }
}

/// Whether the caller named anything to cancel. `--state` alone does not
/// count: it narrows a selection rather than making one.
fn has_selection(args: &ScancelArgs) -> bool {
    !args.job_ids.is_empty()
        || args.user.is_some()
        || args.name.is_some()
        || args.partition.is_some()
        || args.account.is_some()
        || args.reservation.is_some()
}

/// Reject a filter flag given an empty value. An empty filter string reaches the
/// server as "no filter for this field", so `scancel --reservation=` (e.g. from an
/// unset shell variable) would silently widen the selection to every cancellable
/// job instead of a reservation's — and the non-`-u` filters also drop the
/// caller-only scope. Fail before any network I/O rather than cancel broadly.
fn reject_empty_filters(args: &ScancelArgs) -> Result<()> {
    for (flag, value) in [
        ("--reservation", &args.reservation),
        ("--account", &args.account),
        ("--partition", &args.partition),
        ("--name", &args.name),
        ("--user", &args.user),
    ] {
        if value.as_deref() == Some("") {
            bail!("scancel: {flag} requires a non-empty value");
        }
    }
    Ok(())
}

/// The `user` filter for get_jobs. `-u` wins; otherwise scope to the caller only
/// when no other selector was named, so `scancel -A acct` (or `-R res`) spans the
/// whole account/reservation instead of silently narrowing to self. The server
/// still authorizes each cancel.
fn filter_user(args: &ScancelArgs, requester: &str) -> String {
    match &args.user {
        Some(user) => user.clone(),
        None if args.partition.is_some()
            || args.account.is_some()
            || args.name.is_some()
            || args.reservation.is_some() =>
        {
            String::new()
        }
        None => requester.to_string(),
    }
}

fn filter_states(state: Option<&str>) -> Result<Vec<i32>> {
    let Some(states) = state else {
        return Ok(cancellable_states());
    };

    let states = states
        .split(',')
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .map(|state| {
            parse_state(state)
                .map(|state| state as i32)
                .ok_or_else(|| anyhow::anyhow!("scancel: invalid job state: {state}"))
        })
        .collect::<Result<Vec<_>>>()?;

    // Comma-only or whitespace-only input leaves no tokens after normalization.
    if states.is_empty() {
        bail!("scancel: invalid job state: (empty)");
    }

    Ok(states)
}

/// A signal needs a running job, so without `--state` only those are selected
/// rather than failing the command on every pending one.
fn default_states(action: &Action) -> Vec<i32> {
    match action {
        Action::Cancel => cancellable_states(),
        Action::Signal { .. } => vec![spur_proto::proto::JobState::JobRunning as i32],
    }
}

fn cancellable_states() -> Vec<i32> {
    spur_core::job::JobState::ALL
        .iter()
        .filter(|state| !state.is_terminal())
        .map(|state| state.to_proto_i32())
        .collect()
}

/// Map `-s`, `-b` and `-f` to what to do. No signal, or KILL, cancels and the
/// target flags do not apply; `-f` wins over `-b`, as in Slurm.
fn parse_action(signal: Option<&str>, batch: bool, full: bool) -> Result<Action> {
    let Some(text) = signal else {
        return Ok(Action::Cancel);
    };
    let signal = parse_signal(text)?;
    if signal == SIGKILL {
        return Ok(Action::Cancel);
    }
    let target = match (full, batch) {
        (true, _) => SignalTarget::Full,
        (false, true) => SignalTarget::Batch,
        (false, false) => SignalTarget::Steps,
    };
    Ok(Action::Signal { signal, target })
}

const SIGKILL: i32 = 9;
const MAX_SIGNAL: i32 = 31;

/// Linux signal numbers, which is what the agents deliver.
const SIGNAL_NAMES: [(&str, i32); 31] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", SIGKILL),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
    ("STKFLT", 16),
];

/// Accepts a number or a name with or without the `SIG` prefix, in any case.
fn parse_signal(text: &str) -> Result<i32> {
    let invalid = || anyhow::anyhow!("scancel: invalid signal: {text}");
    let upper = text.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    let signal = match name.parse::<i32>() {
        Ok(number) => number,
        Err(_) => SIGNAL_NAMES
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, number)| *number)
            .ok_or_else(invalid)?,
    };
    if !(1..=MAX_SIGNAL).contains(&signal) {
        return Err(invalid());
    }
    Ok(signal)
}

fn parse_state(s: &str) -> Option<spur_proto::proto::JobState> {
    match s.to_uppercase().as_str() {
        "PD" | "PENDING" => Some(spur_proto::proto::JobState::JobPending),
        "R" | "RUNNING" => Some(spur_proto::proto::JobState::JobRunning),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::JobState;

    fn parse(args: &[&str]) -> ScancelArgs {
        let mut argv = vec!["scancel"];
        argv.extend_from_slice(args);
        ScancelArgs::try_parse_from(argv).expect("args should parse")
    }

    #[test]
    fn every_selection_flag_counts_as_a_selection() {
        for args in [
            vec!["123"],
            vec!["-u", "alice"],
            vec!["-n", "myjob"],
            vec!["-p", "default"],
            vec!["-A", "acct"],
            vec!["-R", "maint"],
        ] {
            assert!(
                has_selection(&parse(&args)),
                "{args:?} should select jobs to cancel"
            );
        }
    }

    #[test]
    fn no_arguments_selects_nothing() {
        assert!(!has_selection(&parse(&[])));
    }

    #[test]
    fn state_alone_selects_nothing() {
        assert!(!has_selection(&parse(&["-t", "RUNNING"])));
    }

    #[test]
    fn explicit_user_is_always_the_filter() {
        assert_eq!(filter_user(&parse(&["-u", "alice"]), "bob"), "alice");
        assert_eq!(
            filter_user(&parse(&["-u", "alice", "-A", "gpu"]), "bob"),
            "alice"
        );
    }

    #[test]
    fn account_or_partition_without_user_spans_all_users() {
        assert_eq!(filter_user(&parse(&["-A", "gpu"]), "bob"), "");
        assert_eq!(filter_user(&parse(&["-p", "batch"]), "bob"), "");
        assert_eq!(filter_user(&parse(&["-n", "train"]), "bob"), "");
        assert_eq!(filter_user(&parse(&["-R", "maint"]), "bob"), "");
    }

    #[test]
    fn reservation_filter_is_forwarded_to_get_jobs() {
        let args = parse(&["-R", "maint"]);
        assert_eq!(args.reservation.as_deref(), Some("maint"));
        assert_eq!(
            parse(&["--reservation", "maint"]).reservation.as_deref(),
            Some("maint")
        );
    }

    #[test]
    fn empty_reservation_is_rejected_before_any_request() {
        // `--reservation=` reaches the server as "no reservation filter" and drops
        // the caller-only scope, so it must fail rather than cancel broadly.
        let err = reject_empty_filters(&parse(&["--reservation="])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "scancel: --reservation requires a non-empty value"
        );
    }

    #[test]
    fn every_empty_filter_value_is_rejected() {
        for flag in [
            "--reservation",
            "--account",
            "--partition",
            "--name",
            "--user",
        ] {
            let err = reject_empty_filters(&parse(&[&format!("{flag}=")])).unwrap_err();
            assert!(
                err.to_string().contains(flag),
                "{flag}= should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn non_empty_filters_pass_validation() {
        assert!(reject_empty_filters(&parse(&["-R", "maint"])).is_ok());
        assert!(reject_empty_filters(&parse(&["123"])).is_ok());
    }

    #[test]
    fn no_selector_defaults_to_the_caller() {
        // Reachable via the job-id path, where the filter user is unused but the
        // fallback must stay the caller rather than an empty (owner-bypass) value.
        assert_eq!(filter_user(&parse(&["123"]), "bob"), "bob");
    }

    #[test]
    fn active_states_are_cancellable() {
        for state in [
            JobState::JobPending,
            JobState::JobRunning,
            JobState::JobCompleting,
            JobState::JobPreempted,
            JobState::JobSuspended,
        ] {
            assert!(
                is_cancellable(state as i32),
                "{state:?} should be cancellable"
            );
        }
    }

    #[test]
    fn default_filter_requests_only_cancellable_states() {
        let states = filter_states(None).unwrap();

        assert!(states.contains(&(JobState::JobPending as i32)));
        assert!(states.contains(&(JobState::JobRunning as i32)));
        assert!(states.contains(&(JobState::JobCompleting as i32)));
        assert!(states.contains(&(JobState::JobPreempted as i32)));
        assert!(states.contains(&(JobState::JobSuspended as i32)));
        assert!(!states.contains(&(JobState::JobCompleted as i32)));
        assert!(!states.contains(&(JobState::JobFailed as i32)));
        assert!(!states.contains(&(JobState::JobCancelled as i32)));
        assert!(!states.contains(&(JobState::JobTimeout as i32)));
        assert!(!states.contains(&(JobState::JobNodeFail as i32)));
        assert!(!states.contains(&(JobState::JobDeadline as i32)));
        assert!(!states.contains(&(JobState::JobOutOfMemory as i32)));
    }

    #[test]
    fn explicit_filter_uses_requested_states() {
        assert_eq!(
            filter_states(Some("PD,R")).unwrap(),
            vec![JobState::JobPending as i32, JobState::JobRunning as i32,]
        );
    }

    #[test]
    fn invalid_explicit_filter_is_rejected() {
        let error = filter_states(Some("PD,BANANA")).unwrap_err();

        assert_eq!(error.to_string(), "scancel: invalid job state: BANANA");
    }

    #[test]
    fn empty_explicit_filter_is_rejected() {
        let error = filter_states(Some(" , ")).unwrap_err();

        assert_eq!(error.to_string(), "scancel: invalid job state: (empty)");
    }

    #[test]
    fn terminal_states_are_not_cancellable() {
        for state in [
            JobState::JobCompleted,
            JobState::JobFailed,
            JobState::JobCancelled,
            JobState::JobTimeout,
            JobState::JobNodeFail,
            JobState::JobDeadline,
            JobState::JobOutOfMemory,
        ] {
            assert!(
                !is_cancellable(state as i32),
                "{state:?} should not be cancellable"
            );
        }
    }

    fn signal(signal: i32) -> Action {
        Action::Signal {
            signal,
            target: SignalTarget::Steps,
        }
    }

    #[test]
    fn a_refused_cancel_of_another_users_job_fails_the_command() {
        assert!(counts_as_failure(
            tonic::Code::PermissionDenied,
            &Action::Cancel
        ));
        assert!(counts_as_failure(
            tonic::Code::PermissionDenied,
            &signal(15)
        ));
    }

    #[test]
    fn a_job_already_gone_fails_a_signal_but_not_a_cancel() {
        for code in [tonic::Code::NotFound, tonic::Code::FailedPrecondition] {
            assert!(
                !counts_as_failure(code, &Action::Cancel),
                "{code:?} with a cancel"
            );
            assert!(
                counts_as_failure(code, &signal(15)),
                "{code:?} with SIGTERM"
            );
        }
    }

    #[test]
    fn an_ignored_refusal_is_not_reported() {
        for code in [tonic::Code::NotFound, tonic::Code::FailedPrecondition] {
            assert!(
                !should_report(code, &Action::Cancel, false),
                "{code:?} with a cancel"
            );
        }
    }

    #[test]
    fn quiet_hides_a_gone_job_on_a_signal_but_not_a_denial() {
        assert!(should_report(tonic::Code::NotFound, &signal(15), false));
        assert!(!should_report(tonic::Code::NotFound, &signal(15), true));
        assert!(should_report(
            tonic::Code::PermissionDenied,
            &Action::Cancel,
            true
        ));
    }

    #[test]
    fn a_controller_without_signal_support_fails_a_signal_even_when_quiet() {
        assert!(counts_as_failure(tonic::Code::Unimplemented, &signal(15)));
        assert!(should_report(tonic::Code::Unimplemented, &signal(15), true));
    }

    #[test]
    fn unknown_state_is_cancellable() {
        // Server stays the authority on rejection for values the client
        // does not recognize.
        assert!(is_cancellable(9999));
    }

    fn action(args: &[&str]) -> Action {
        let args = parse(args);
        parse_action(args.signal.as_deref(), args.batch, args.full).expect("valid signal")
    }

    fn signal_to(signal: i32, target: SignalTarget) -> Action {
        Action::Signal { signal, target }
    }

    #[test]
    fn no_signal_cancels_whatever_the_target_flags() {
        for args in [
            vec!["1"],
            vec!["-b", "1"],
            vec!["-f", "1"],
            vec!["-b", "-f", "1"],
        ] {
            assert_eq!(action(&args), Action::Cancel, "{args:?}");
        }
    }

    #[test]
    fn kill_cancels_by_name_number_and_case() {
        for sig in ["KILL", "SIGKILL", "kill", "sigkill", "Kill", "9"] {
            for flags in [vec![], vec!["-b"], vec!["-f"]] {
                let mut args = vec!["-s", sig, "1"];
                args.extend(flags.iter());
                assert_eq!(action(&args), Action::Cancel, "{args:?}");
            }
        }
    }

    #[test]
    fn a_signal_defaults_to_the_steps_target() {
        assert_eq!(
            action(&["-s", "USR1", "1"]),
            signal_to(10, SignalTarget::Steps)
        );
        assert_eq!(
            action(&["--signal", "TERM", "1"]),
            signal_to(15, SignalTarget::Steps)
        );
    }

    #[test]
    fn batch_selects_the_batch_target() {
        assert_eq!(
            action(&["-s", "USR1", "-b", "1"]),
            signal_to(10, SignalTarget::Batch)
        );
        assert_eq!(
            action(&["-s", "USR1", "--batch", "1"]),
            signal_to(10, SignalTarget::Batch)
        );
    }

    #[test]
    fn full_selects_the_full_target() {
        assert_eq!(
            action(&["-s", "USR1", "-f", "1"]),
            signal_to(10, SignalTarget::Full)
        );
        assert_eq!(
            action(&["-s", "USR1", "--full", "1"]),
            signal_to(10, SignalTarget::Full)
        );
    }

    #[test]
    fn full_wins_over_batch_in_either_order() {
        assert_eq!(
            action(&["-s", "USR1", "-b", "-f", "1"]),
            signal_to(10, SignalTarget::Full)
        );
        assert_eq!(
            action(&["-s", "USR1", "-f", "-b", "1"]),
            signal_to(10, SignalTarget::Full)
        );
        assert_eq!(
            action(&["-s", "USR1", "-bf", "1"]),
            signal_to(10, SignalTarget::Full)
        );
    }

    #[test]
    fn signals_parse_by_name_prefix_number_and_case() {
        for (text, number) in [
            ("USR1", 10),
            ("SIGUSR1", 10),
            ("usr1", 10),
            ("sigusr1", 10),
            ("10", 10),
            ("TERM", 15),
            ("SigTerm", 15),
            ("15", 15),
            ("INT", 2),
            ("HUP", 1),
            ("usr2", 12),
            ("CONT", 18),
            ("31", 31),
        ] {
            assert_eq!(
                parse_action(Some(text), false, false).unwrap(),
                signal_to(number, SignalTarget::Steps),
                "{text}"
            );
        }
    }

    #[test]
    fn an_unknown_or_out_of_range_signal_is_rejected() {
        for text in ["BOGUS", "SIG", "", "0", "-1", "32", "64", "1000"] {
            let err = parse_action(Some(text), false, false).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("scancel: invalid signal: {text}"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn full_and_batch_flags_parse() {
        let args = parse(&["-b", "-f", "1"]);
        assert!(args.batch && args.full);
        let args = parse(&["--full", "1"]);
        assert!(!args.batch && args.full);
    }

    #[test]
    fn signalling_without_state_filter_targets_running_jobs_only() {
        assert_eq!(default_states(&Action::Cancel), cancellable_states());
        assert_eq!(
            default_states(&signal(10)),
            vec![JobState::JobRunning as i32]
        );
    }

    #[test]
    fn refusal_names_the_verb_for_the_action() {
        let status = tonic::Status::permission_denied("not yours");
        assert_eq!(
            refusal_text(7, &status, &Action::Cancel),
            "scancel: error cancelling job 7: not yours"
        );
        assert_eq!(
            refusal_text(7, &status, &signal(10)),
            "scancel: error signalling job 7: not yours"
        );
    }

    #[test]
    fn a_controller_that_predates_signal_job_is_told_to_upgrade() {
        let status = tonic::Status::unimplemented("signal_job");
        let text = refusal_text(7, &status, &signal(10));
        assert!(text.contains("does not support signalling jobs"), "{text}");
        assert!(!text.contains("signal_job"), "{text}");
    }

    #[tokio::test]
    async fn cancel_goes_to_cancel_job_with_the_zero_signal() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let mut client = crate::mock_controller::client(addr).await;

        send(&mut client, &Action::Cancel, 11, "alice")
            .await
            .unwrap();

        let requests = capture.cancel_job_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            (
                requests[0].job_id,
                requests[0].signal,
                requests[0].user.as_str()
            ),
            (11, 0, "alice")
        );
        assert!(capture.signal_job_requests().is_empty());
    }

    #[tokio::test]
    async fn a_signal_goes_to_signal_job_and_never_to_cancel_job() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let mut client = crate::mock_controller::client(addr).await;

        for target in [SignalTarget::Steps, SignalTarget::Batch, SignalTarget::Full] {
            send(&mut client, &signal_to(10, target), 12, "alice")
                .await
                .unwrap();
        }

        let requests = capture.signal_job_requests();
        let seen: Vec<_> = requests
            .iter()
            .map(|r| (r.job_id, r.signal, r.target, r.user.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (12, 10, SignalTarget::Steps as i32, "alice"),
                (12, 10, SignalTarget::Batch as i32, "alice"),
                (12, 10, SignalTarget::Full as i32, "alice"),
            ]
        );
        assert_eq!(capture.cancel_job_calls(), 0);
    }

    #[tokio::test]
    async fn a_refused_signal_surfaces_the_servers_status() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_signal_job_error(tonic::Status::failed_precondition(
            "job 12 has no batch step",
        ));
        let mut client = crate::mock_controller::client(addr).await;
        let action = signal_to(10, SignalTarget::Batch);

        let status = send(&mut client, &action, 12, "alice").await.unwrap_err();

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(status.message(), "job 12 has no batch step");
        assert!(counts_as_failure(status.code(), &action));
    }

    #[tokio::test]
    async fn the_command_signals_each_named_job_with_the_chosen_target() {
        let (addr, capture) = crate::mock_controller::spawn().await;

        main_with_args(
            [
                "scancel",
                "--controller",
                &format!("http://{addr}"),
                "-u",
                "alice",
                "-s",
                "SIGUSR1",
                "-b",
                "-f",
                "21",
                "22",
            ]
            .map(String::from)
            .to_vec(),
        )
        .await
        .unwrap();

        let seen: Vec<_> = capture
            .signal_job_requests()
            .iter()
            .map(|r| (r.job_id, r.signal, r.target, r.user.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (21, 10, SignalTarget::Full as i32, "alice".to_string()),
                (22, 10, SignalTarget::Full as i32, "alice".to_string()),
            ]
        );
        assert_eq!(capture.cancel_job_calls(), 0);
    }

    #[tokio::test]
    async fn the_command_cancels_on_kill_even_with_a_target_flag() {
        let (addr, capture) = crate::mock_controller::spawn().await;

        main_with_args(
            [
                "scancel",
                "--controller",
                &format!("http://{addr}"),
                "-u",
                "alice",
                "-s",
                "KILL",
                "-b",
                "31",
            ]
            .map(String::from)
            .to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(capture.cancel_job_requests().len(), 1);
        assert!(capture.signal_job_requests().is_empty());
    }

    async fn run_scancel(addr: std::net::SocketAddr, args: &[&str]) {
        let mut argv = vec![
            "scancel".to_string(),
            "--controller".to_string(),
            format!("http://{addr}"),
        ];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        main_with_args(argv).await.unwrap();
    }

    #[tokio::test]
    async fn filter_mode_signal_selects_running_jobs_unless_state_overrides() {
        let (addr, capture) = crate::mock_controller::spawn().await;

        run_scancel(addr, &["-u", "alice", "-s", "USR1"]).await;
        run_scancel(addr, &["-u", "alice", "-s", "USR1", "-t", "PD"]).await;
        run_scancel(addr, &["-u", "alice"]).await;

        let states: Vec<_> = capture
            .get_jobs_requests()
            .into_iter()
            .map(|request| request.states)
            .collect();
        assert_eq!(
            states,
            vec![
                vec![JobState::JobRunning as i32],
                vec![JobState::JobPending as i32],
                cancellable_states(),
            ]
        );
    }
}
