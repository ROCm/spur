// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{GetUsageRequest, ListAccountsRequest, ListUsersRequest};

use crate::timearg::{datetime_to_proto, parse_time_arg};
use crate::usage_scope::{cell, UsageScope};

/// Generate usage reports from accounting data.
#[derive(Parser, Debug)]
#[command(name = "sreport", about = "Generate usage reports")]
pub struct SreportArgs {
    #[command(subcommand)]
    pub command: SreportCommand,

    /// Start time filter (e.g., "2024-01-01", "now-7days")
    #[arg(short = 's', long = "start", global = true)]
    pub start: Option<String>,

    /// End time filter
    #[arg(short = 'e', long = "end", global = true)]
    pub end: Option<String>,

    /// Don't print header
    #[arg(long, global = true)]
    pub noheader: bool,

    /// Accepted for Slurm compatibility; has no effect
    #[arg(long, global = true)]
    pub noconvert: bool,

    /// Parsable output
    #[arg(short = 'p', long, global = true)]
    pub parsable: bool,

    /// Controller address (accounting is served on the same port)
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    pub controller: String,
}

#[derive(Subcommand, Debug)]
pub enum SreportCommand {
    /// Lifetime preemption outcomes by requesting user and account
    Preemptions {
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        account: Option<String>,
        /// Show only unused counts, grouped by reason and initiator
        #[arg(long)]
        by_cause: bool,
        /// User/account pairs per page (1-1000)
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        /// Continue from a previous report's next-page token
        #[arg(long)]
        page_token: Option<String>,
    },
    /// Cluster utilization reports
    Cluster {
        /// Report type: AccountUtilizationByUser, UserUtilizationByAccount
        report_type: String,
    },
    /// Job-based reports
    Job {
        /// Report type: SizesByAccount, SizesByUser
        report_type: String,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<SreportArgs>(&args);

    if matches!(args.command, SreportCommand::Preemptions { .. })
        && (args.start.is_some() || args.end.is_some())
    {
        anyhow::bail!("preemption reports are lifetime totals; --start/--end are not supported");
    }
    let since = args
        .start
        .as_deref()
        .and_then(parse_time_arg)
        .map(datetime_to_proto);

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to controller")?;
    let mut client = spur_proto::accounting_client(channel);

    match &args.command {
        SreportCommand::Preemptions {
            user,
            account,
            limit,
            page_token,
            by_cause,
        } => {
            let mut request = tonic::Request::new(spur_proto::proto::GetPreemptionUsageRequest {
                user: user.clone().unwrap_or_default(),
                account: account.clone().unwrap_or_default(),
                limit: *limit,
                page_token: page_token.clone().unwrap_or_default(),
                by_cause: *by_cause,
            });
            request.set_timeout(std::time::Duration::from_secs(15));
            let response = client
                .get_preemption_usage(request)
                .await
                .map_err(preemption_report_error)?
                .into_inner();
            if let Some(user) = &response.scoped_to_user {
                eprintln!("Visible usage for {user} only");
            }
            if response.partial {
                eprintln!("Visible usage only; account totals may be incomplete");
            }
            print_preemptions(
                &mut std::io::stdout().lock(),
                &response,
                *by_cause,
                args.parsable,
                args.noheader,
            )?;
            if !response.next_page_token.is_empty() {
                eprintln!("Next page token: {}", response.next_page_token);
            }
            Ok(())
        }
        SreportCommand::Cluster { report_type } => match report_type.to_lowercase().as_str() {
            "accountutilizationbyuser" | "accountutilization" => {
                report_account_utilization_by_user(&mut client, since, &args).await
            }
            "userutilizationbyaccount" | "userutilization" => {
                report_user_utilization_by_account(&mut client, since, &args).await
            }
            other => {
                eprintln!(
                        "sreport: unknown cluster report '{}'. Available: AccountUtilizationByUser, UserUtilizationByAccount",
                        other
                    );
                std::process::exit(1);
            }
        },
        SreportCommand::Job { report_type } => match report_type.to_lowercase().as_str() {
            "sizesbyaccount" | "sizes" => {
                report_job_sizes_by_account(&mut client, since, &args).await
            }
            "sizesbyuser" => report_job_sizes_by_user(&mut client, since, &args).await,
            other => {
                eprintln!(
                    "sreport: unknown job report '{}'. Available: SizesByAccount, SizesByUser",
                    other
                );
                std::process::exit(1);
            }
        },
    }
}

type AcctClient = SlurmAccountingClient<crate::authclient::AuthChannel>;

fn preemption_report_error(error: tonic::Status) -> anyhow::Error {
    if error.code() == tonic::Code::Unimplemented {
        anyhow::anyhow!("controller does not support preemption reports; upgrade the controller")
    } else {
        anyhow::Error::new(error).context("failed to get preemption report")
    }
}

fn print_preemptions(
    out: &mut impl std::io::Write,
    response: &spur_proto::proto::GetPreemptionUsageResponse,
    by_cause: bool,
    parsable: bool,
    noheader: bool,
) -> Result<()> {
    if by_cause {
        if !response.causes_included {
            anyhow::bail!("controller does not support --by-cause; upgrade the controller");
        }
        return print_preemption_causes(out, response, parsable, noheader);
    }
    if !noheader {
        if parsable {
            writeln!(out, "User|Account|Used|Unused|Open")?;
        } else {
            writeln!(
                out,
                "{:<20} {:<20} {:>12} {:>12} {:>12}",
                "User", "Account", "Used", "Unused", "Open"
            )?;
        }
    }
    for entry in &response.entries {
        if parsable {
            writeln!(
                out,
                "{}|{}|{}|{}|{}",
                entry.user, entry.account, entry.used, entry.unused, entry.open
            )?;
        } else {
            writeln!(
                out,
                "{:<20} {:<20} {:>12} {:>12} {:>12}",
                entry.user, entry.account, entry.used, entry.unused, entry.open
            )?;
        }
    }
    Ok(())
}

fn print_preemption_causes(
    out: &mut impl std::io::Write,
    response: &spur_proto::proto::GetPreemptionUsageResponse,
    parsable: bool,
    noheader: bool,
) -> Result<()> {
    if !noheader {
        if parsable {
            writeln!(out, "User|Account|Reason|Initiator|Unused")?;
        } else {
            writeln!(
                out,
                "{:<20} {:<20} {:<20} {:<12} {:>12}",
                "User", "Account", "Reason", "Initiator", "Unused"
            )?;
        }
    }
    for entry in &response.entries {
        let unclassified = spur_proto::proto::UnusedPreemptionCount {
            reason: "unclassified".into(),
            initiator: "unknown".into(),
            count: entry.unclassified_unused,
        };
        for cause in entry
            .unused_by_cause
            .iter()
            .chain(std::iter::once(&unclassified))
        {
            if cause.count == 0 {
                continue;
            }
            if parsable {
                writeln!(
                    out,
                    "{}|{}|{}|{}|{}",
                    entry.user, entry.account, cause.reason, cause.initiator, cause.count
                )?;
            } else {
                writeln!(
                    out,
                    "{:<20} {:<20} {:<20} {:<12} {:>12}",
                    entry.user, entry.account, cause.reason, cause.initiator, cause.count
                )?;
            }
        }
    }
    Ok(())
}

/// Account definitions. A non-coordinator is denied when `accounts` is private;
/// the caller then builds rows from the usage it is allowed to see.
async fn account_directory(client: &mut AcctClient) -> Result<Vec<spur_proto::proto::AccountInfo>> {
    match client
        .list_accounts(ListAccountsRequest { shares: false })
        .await
    {
        Ok(resp) => Ok(resp.into_inner().accounts),
        Err(status) if status.code() == tonic::Code::PermissionDenied => Ok(Vec::new()),
        Err(status) => Err(status).context("failed to list accounts"),
    }
}

async fn report_account_utilization_by_user(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let mut accounts = account_directory(client).await?;

    let users_resp = client
        .list_users(ListUsersRequest {
            account: String::new(),
            user: String::new(),
            associations: true,
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
            job_scoped: false,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();
    if accounts.is_empty() {
        let mut names: Vec<String> = usage.entries.iter().map(|e| e.account.clone()).collect();
        names.sort();
        names.dedup();
        accounts = names
            .into_iter()
            .map(|name| spur_proto::proto::AccountInfo {
                name,
                ..Default::default()
            })
            .collect();
    }

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        let header = format!(
            "{:<20}{}{:<15}{}{:>12}{}{:>12}{}{:>10}",
            "Account",
            delimiter,
            "User",
            delimiter,
            "CPU Seconds",
            delimiter,
            "GPU Seconds",
            delimiter,
            "Jobs"
        );
        println!("{}", header);
        if !args.parsable {
            println!("{}", "-".repeat(75));
        }
    }

    // Build lookup maps (server guarantees one entry per user+account)
    let mut acct_agg: std::collections::HashMap<&str, (f64, f64, u64)> =
        std::collections::HashMap::new();
    let mut user_agg: std::collections::HashMap<(&str, &str), (f64, f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        let a = acct_agg.entry(&e.account).or_default();
        a.0 += e.cpu_seconds;
        a.1 += e.gpu_seconds;
        a.2 += e.job_count;
        user_agg.insert(
            (&e.user, &e.account),
            (e.cpu_seconds, e.gpu_seconds, e.job_count),
        );
    }

    let scope = utilization_scope(&usage);
    for account in &accounts {
        let &(acct_cpu, acct_gpu, acct_jobs) = acct_agg
            .get(account.name.as_str())
            .unwrap_or(&(0.0, 0.0, 0));
        let visible = account_visible(
            &usage.visible_accounts,
            &account.name,
            acct_agg.contains_key(account.name.as_str()),
        );
        let account_usage = account_utilization_shown(&scope, visible);

        println!(
            "{:<20}{}{:<15}{}{}{}{}{}{}",
            account.name,
            delimiter,
            "",
            delimiter,
            cell(account_usage, 12, || format!("{acct_cpu:>12.0}")),
            delimiter,
            cell(account_usage, 12, || format!("{acct_gpu:>12.0}")),
            delimiter,
            cell(account_usage, 10, || format!("{acct_jobs:>10}")),
        );

        let account_users: Vec<_> = users.iter().filter(|u| u.account == account.name).collect();
        for user in &account_users {
            let &(user_cpu, user_gpu, user_jobs) = user_agg
                .get(&(user.name.as_str(), account.name.as_str()))
                .unwrap_or(&(0.0, 0.0, 0));
            let mine = user_utilization_shown(&scope, &user.name, visible);

            println!(
                " {:<19}{}{:<15}{}{}{}{}{}{}",
                "",
                delimiter,
                user.name,
                delimiter,
                cell(mine, 12, || format!("{user_cpu:>12.0}")),
                delimiter,
                cell(mine, 12, || format!("{user_gpu:>12.0}")),
                delimiter,
                cell(mine, 10, || format!("{user_jobs:>10}")),
            );
        }
    }

    Ok(())
}

async fn report_user_utilization_by_account(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let users_resp = client
        .list_users(ListUsersRequest {
            account: String::new(),
            user: String::new(),
            associations: true,
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
            job_scoped: false,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        println!(
            "{:<15}{}{:<20}{}{:>12}{}{:>12}{}{:>10}",
            "User",
            delimiter,
            "Account",
            delimiter,
            "CPU Seconds",
            delimiter,
            "GPU Seconds",
            delimiter,
            "Jobs"
        );
        if !args.parsable {
            println!("{}", "-".repeat(75));
        }
    }

    let mut user_agg: std::collections::HashMap<(&str, &str), (f64, f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        user_agg.insert(
            (&e.user, &e.account),
            (e.cpu_seconds, e.gpu_seconds, e.job_count),
        );
    }

    let scope = utilization_scope(&usage);
    for user in &users {
        let &(cpu, gpu, jobs) = user_agg
            .get(&(user.name.as_str(), user.account.as_str()))
            .unwrap_or(&(0.0, 0.0, 0));
        let visible = account_visible(
            &usage.visible_accounts,
            &user.account,
            user_agg.contains_key(&(user.name.as_str(), user.account.as_str())),
        );
        let mine = user_utilization_shown(&scope, &user.name, visible);

        println!(
            "{:<15}{}{:<20}{}{}{}{}{}{}",
            user.name,
            delimiter,
            user.account,
            delimiter,
            cell(mine, 12, || format!("{cpu:>12.0}")),
            delimiter,
            cell(mine, 12, || format!("{gpu:>12.0}")),
            delimiter,
            cell(mine, 10, || format!("{jobs:>10}")),
        );
    }

    Ok(())
}

async fn report_job_sizes_by_account(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let accounts = account_directory(client).await?;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
            job_scoped: true,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let mut acct_agg: std::collections::HashMap<&str, (f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        let a = acct_agg.entry(&e.account).or_default();
        a.0 += e.cpu_seconds;
        a.1 += e.job_count;
    }
    let total_cpu: f64 = acct_agg.values().map(|v| v.0).sum();
    let total_jobs: u64 = acct_agg.values().map(|v| v.1).sum();

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        println!(
            "{:<20}{}{:>10}{}{:>12}{}{:>8}",
            "Account", delimiter, "Jobs", delimiter, "CPU Seconds", delimiter, "% of Tot"
        );
        if !args.parsable {
            println!("{}", "-".repeat(56));
        }
    }

    let restricted = usage.scoped_to_user.is_some() || usage.partial || accounts.is_empty();
    let mut names: Vec<String> = if restricted {
        acct_agg.keys().map(|name| (*name).to_string()).collect()
    } else {
        accounts
            .iter()
            .map(|account| account.name.clone())
            .collect()
    };
    if !restricted {
        for name in acct_agg.keys() {
            if !names.iter().any(|have| have == *name) {
                names.push((*name).to_string());
            }
        }
    }
    names.sort();
    for name in &names {
        let &(cpu, jobs) = acct_agg.get(name.as_str()).unwrap_or(&(0.0, 0));
        let pct = if total_cpu > 0.0 {
            (cpu / total_cpu) * 100.0
        } else {
            0.0
        };

        println!(
            "{:<20}{}{jobs:>10}{}{cpu:>12.0}{}{pct:>7.1}%",
            name, delimiter, delimiter, delimiter,
        );
    }

    if !args.parsable {
        println!("{}", "-".repeat(56));
        let tot_pct = if total_cpu > 0.0 { 100.0 } else { 0.0 };
        println!(
            "{:<20}{}{total_jobs:>10}{}{total_cpu:>12.0}{}{tot_pct:>7.1}%",
            "TOTAL", delimiter, delimiter, delimiter,
        );
    }

    Ok(())
}

async fn report_job_sizes_by_user(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let users_resp = client
        .list_users(ListUsersRequest {
            account: String::new(),
            user: String::new(),
            associations: true,
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
            job_scoped: true,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let mut user_agg: std::collections::HashMap<(&str, &str), (f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        let u = user_agg.entry((&e.user, &e.account)).or_default();
        u.0 += e.cpu_seconds;
        u.1 += e.job_count;
    }
    let total_cpu: f64 = user_agg.values().map(|v| v.0).sum();
    let total_jobs: u64 = user_agg.values().map(|v| v.1).sum();

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        println!(
            "{:<15}{}{:<20}{}{:>10}{}{:>12}{}{:>8}",
            "User",
            delimiter,
            "Account",
            delimiter,
            "Jobs",
            delimiter,
            "CPU Seconds",
            delimiter,
            "% of Tot"
        );
        if !args.parsable {
            println!("{}", "-".repeat(71));
        }
    }

    let restricted = usage.scoped_to_user.is_some() || usage.partial || users.is_empty();
    let mut rows: Vec<(String, String, u64, f64)> = if restricted {
        user_agg
            .iter()
            .map(|((user, account), (cpu, jobs))| {
                ((*user).to_string(), (*account).to_string(), *jobs, *cpu)
            })
            .collect()
    } else {
        users
            .iter()
            .map(|user| {
                let (cpu, jobs) = user_agg
                    .get(&(user.name.as_str(), user.account.as_str()))
                    .copied()
                    .unwrap_or((0.0, 0));
                (user.name.clone(), user.account.clone(), jobs, cpu)
            })
            .collect()
    };
    if !restricted {
        for ((user, account), (cpu, jobs)) in &user_agg {
            if !rows.iter().any(|row| row.0 == *user && row.1 == *account) {
                rows.push(((*user).to_string(), (*account).to_string(), *jobs, *cpu));
            }
        }
    }
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for (user, account, jobs, cpu) in &rows {
        let pct = if total_cpu > 0.0 {
            (cpu / total_cpu) * 100.0
        } else {
            0.0
        };
        println!(
            "{:<15}{}{:<20}{}{jobs:>10}{}{cpu:>12.0}{}{pct:>7.1}%",
            user, delimiter, account, delimiter, delimiter, delimiter,
        );
    }

    if !args.parsable {
        println!("{}", "-".repeat(71));
        let tot_pct = if total_cpu > 0.0 { 100.0 } else { 0.0 };
        println!(
            "{:<15}{}{:<20}{}{total_jobs:>10}{}{total_cpu:>12.0}{}{tot_pct:>7.1}%",
            "TOTAL", delimiter, "", delimiter, delimiter, delimiter,
        );
    }

    Ok(())
}

fn utilization_scope(usage: &spur_proto::proto::GetUsageResponse) -> UsageScope {
    UsageScope::from_replies([usage.scoped_to_user.as_deref()])
        .with_partial([usage.partial])
        .with_caller(usage.partial_caller.as_deref())
}

/// An account total is real on a complete reply, and on a coordinator's own
/// accounts when the reply is partial. A single-user pin has no account total.
fn account_utilization_shown(scope: &UsageScope, account_visible: bool) -> bool {
    scope.account_rows_visible() && (scope.totals_visible() || account_visible)
}

/// A user cell is real for the caller, for every user on a complete reply,
/// and for users in a coordinator's accounts. Anyone else stays blank: a
/// missing row there is hidden usage, not a zero.
fn user_utilization_shown(scope: &UsageScope, user: &str, account_visible: bool) -> bool {
    scope.user_visible(user) && (scope.totals_visible() || scope.is_caller(user) || account_visible)
}

/// `visible_accounts` is what makes a missing row a true zero. A usage entry
/// counts too, so a reply that left the list empty still shows rows it returned.
fn account_visible(visible_accounts: &[String], account: &str, has_usage: bool) -> bool {
    has_usage || visible_accounts.iter().any(|name| name == account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preemption_report_unimplemented_explains_upgrade_for_all_report_modes() {
        let error = preemption_report_error(tonic::Status::unimplemented("unknown method"));
        assert!(error.to_string().contains("upgrade the controller"));
        let error = preemption_report_error(tonic::Status::unavailable("offline"));
        assert_eq!(
            error
                .downcast_ref::<tonic::Status>()
                .expect("status")
                .code(),
            tonic::Code::Unavailable
        );
    }

    #[test]
    fn preemption_report_page_arguments() {
        let args = SreportArgs::try_parse_from([
            "sreport",
            "preemptions",
            "--user",
            "alice",
            "--account",
            "research",
            "--limit",
            "25",
            "--parsable",
            "--by-cause",
        ])
        .expect("arguments");
        assert!(matches!(
            args.command,
            SreportCommand::Preemptions {
                limit: 25,
                user: Some(_),
                account: Some(_),
                by_cause: true,
                ..
            }
        ));
        for limit in ["0", "1001"] {
            assert!(
                SreportArgs::try_parse_from(["sreport", "preemptions", "--limit", limit]).is_err()
            );
        }
    }

    #[test]
    fn preemption_reports_separate_totals_from_nonzero_causes() {
        use spur_proto::proto::{
            GetPreemptionUsageResponse, PreemptionUsageEntry, UnusedPreemptionCount,
        };
        let mut response = GetPreemptionUsageResponse {
            entries: vec![
                PreemptionUsageEntry {
                    user: "alice".into(),
                    account: "research".into(),
                    used: 2,
                    unused: 3,
                    open: 1,
                    unused_by_cause: vec![
                        UnusedPreemptionCount {
                            reason: "cancelled".into(),
                            initiator: "user".into(),
                            count: 2,
                        },
                        UnusedPreemptionCount {
                            reason: "launch_failed".into(),
                            initiator: "system".into(),
                            count: 0,
                        },
                    ],
                    unclassified_unused: 1,
                },
                PreemptionUsageEntry {
                    user: "bob".into(),
                    account: "research".into(),
                    used: 4,
                    open: 1,
                    ..Default::default()
                },
            ],
            causes_included: true,
            ..Default::default()
        };
        for (by_cause, header, rows) in [
            (
                false,
                "User|Account|Used|Unused|Open\n",
                "alice|research|2|3|1\nbob|research|4|0|1\n",
            ),
            (
                true,
                "User|Account|Reason|Initiator|Unused\n",
                "alice|research|cancelled|user|2\nalice|research|unclassified|unknown|1\n",
            ),
        ] {
            for noheader in [false, true] {
                for parsable in [false, true] {
                    let mut out = Vec::new();
                    print_preemptions(&mut out, &response, by_cause, parsable, noheader)
                        .expect("report");
                    let actual = String::from_utf8(out).expect("utf8");
                    let expected = format!("{}{rows}", if noheader { "" } else { header });
                    if parsable {
                        assert_eq!(actual, expected);
                    } else {
                        let normalized = actual
                            .lines()
                            .map(|line| line.split_whitespace().collect::<Vec<_>>().join("|"))
                            .collect::<Vec<_>>()
                            .join("\n")
                            + "\n";
                        assert_eq!(normalized, expected);
                    }
                }
            }
        }
        response.causes_included = false;
        let mut out = Vec::new();
        assert!(print_preemptions(&mut out, &response, true, true, false).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn preemption_cause_page_without_unused_has_no_rows() {
        let response = spur_proto::proto::GetPreemptionUsageResponse {
            entries: vec![spur_proto::proto::PreemptionUsageEntry {
                user: "alice".into(),
                account: "research".into(),
                used: 2,
                open: 1,
                ..Default::default()
            }],
            causes_included: true,
            next_page_token: r#"["alice","research"]"#.into(),
            ..Default::default()
        };
        for parsable in [false, true] {
            let mut out = Vec::new();
            print_preemptions(&mut out, &response, true, parsable, true).expect("empty page");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn partial_utilization_fills_visible_accounts_and_blanks_the_rest() {
        let visible = vec!["ml".to_string()];
        assert!(account_visible(&visible, "ml", false));
        assert!(!account_visible(&visible, "physics", false));
        assert!(account_visible(&visible, "physics", true));

        let partial = UsageScope::from_replies([None]).with_partial([true]);
        assert!(account_utilization_shown(&partial, true));
        assert!(!account_utilization_shown(&partial, false));
        assert!(user_utilization_shown(&partial, "bob", true));
        assert!(!user_utilization_shown(&partial, "bob", false));

        let pinned = UsageScope::from_replies([Some("alice")]);
        assert!(!account_utilization_shown(&pinned, true));
        assert!(user_utilization_shown(&pinned, "alice", false));
        assert!(!user_utilization_shown(&pinned, "bob", true));

        let open = UsageScope::from_replies([None]);
        assert!(account_utilization_shown(&open, false));
        assert!(user_utilization_shown(&open, "bob", false));
    }

    #[test]
    fn a_coordinator_keeps_their_own_zero_outside_the_accounts_they_coordinate() {
        let carol = utilization_scope(&spur_proto::proto::GetUsageResponse {
            partial: true,
            visible_accounts: vec!["physics".into()],
            partial_caller: Some("carol".into()),
            ..Default::default()
        });
        assert!(user_utilization_shown(&carol, "carol", false));
        assert!(!user_utilization_shown(&carol, "bob", false));
    }

    #[test]
    fn noconvert_is_accepted() {
        // Slurm scripts pass --noconvert to keep output machine-parseable.
        let before = SreportArgs::try_parse_from([
            "sreport",
            "--noconvert",
            "cluster",
            "AccountUtilizationByUser",
        ])
        .unwrap();
        assert!(before.noconvert);

        let after = SreportArgs::try_parse_from([
            "sreport",
            "cluster",
            "AccountUtilizationByUser",
            "--noconvert",
        ])
        .unwrap();
        assert!(after.noconvert);
    }
}
