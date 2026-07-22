// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{GetUsageRequest, ListAccountsRequest, ListUsersRequest};

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
    let args = SreportArgs::try_parse_from(&args)?;

    let since = args
        .start
        .as_deref()
        .and_then(parse_time_arg)
        .map(datetime_to_proto);

    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to controller")?;
    let mut client = spur_proto::accounting_client(channel);

    match &args.command {
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

type AcctClient = SlurmAccountingClient<tonic::transport::Channel>;

async fn report_account_utilization_by_user(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let accounts_resp = client
        .list_accounts(ListAccountsRequest {})
        .await
        .context("failed to list accounts")?;
    let accounts = accounts_resp.into_inner().accounts;

    let users_resp = client
        .list_users(ListUsersRequest {
            account: String::new(),
            user: String::new(),
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        let header = format!(
            "{:<20}{}{:<15}{}{:>12}{}{:>12}{}{:>10}",
            "Account",
            delimiter,
            "User",
            delimiter,
            "CPU Hours",
            delimiter,
            "GPU Hours",
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
        a.0 += e.cpu_hours;
        a.1 += e.gpu_hours;
        a.2 += e.job_count;
        user_agg.insert(
            (&e.user, &e.account),
            (e.cpu_hours, e.gpu_hours, e.job_count),
        );
    }

    for account in &accounts {
        let &(acct_cpu, acct_gpu, acct_jobs) = acct_agg
            .get(account.name.as_str())
            .unwrap_or(&(0.0, 0.0, 0));

        println!(
            "{:<20}{}{:<15}{}{:>12.1}{}{:>12.1}{}{:>10}",
            account.name,
            delimiter,
            "",
            delimiter,
            acct_cpu,
            delimiter,
            acct_gpu,
            delimiter,
            acct_jobs
        );

        let account_users: Vec<_> = users.iter().filter(|u| u.account == account.name).collect();
        for user in &account_users {
            let &(user_cpu, user_gpu, user_jobs) = user_agg
                .get(&(user.name.as_str(), account.name.as_str()))
                .unwrap_or(&(0.0, 0.0, 0));

            println!(
                " {:<19}{}{:<15}{}{:>12.1}{}{:>12.1}{}{:>10}",
                "",
                delimiter,
                user.name,
                delimiter,
                user_cpu,
                delimiter,
                user_gpu,
                delimiter,
                user_jobs
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
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
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
            "CPU Hours",
            delimiter,
            "GPU Hours",
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
            (e.cpu_hours, e.gpu_hours, e.job_count),
        );
    }

    for user in &users {
        let &(cpu, gpu, jobs) = user_agg
            .get(&(user.name.as_str(), user.account.as_str()))
            .unwrap_or(&(0.0, 0.0, 0));

        println!(
            "{:<15}{}{:<20}{}{:>12.1}{}{:>12.1}{}{:>10}",
            user.name, delimiter, user.account, delimiter, cpu, delimiter, gpu, delimiter, jobs
        );
    }

    Ok(())
}

async fn report_job_sizes_by_account(
    client: &mut AcctClient,
    since: Option<prost_types::Timestamp>,
    args: &SreportArgs,
) -> Result<()> {
    let accounts_resp = client
        .list_accounts(ListAccountsRequest {})
        .await
        .context("failed to list accounts")?;
    let accounts = accounts_resp.into_inner().accounts;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let mut acct_agg: std::collections::HashMap<&str, (f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        let a = acct_agg.entry(&e.account).or_default();
        a.0 += e.cpu_hours;
        a.1 += e.job_count;
    }
    let total_cpu: f64 = acct_agg.values().map(|v| v.0).sum();
    let total_jobs: u64 = acct_agg.values().map(|v| v.1).sum();

    let delimiter = if args.parsable { "|" } else { "  " };

    if !args.noheader {
        println!(
            "{:<20}{}{:>10}{}{:>12}{}{:>8}",
            "Account", delimiter, "Jobs", delimiter, "CPU Hours", delimiter, "% of Tot"
        );
        if !args.parsable {
            println!("{}", "-".repeat(56));
        }
    }

    for account in &accounts {
        let &(cpu, jobs) = acct_agg.get(account.name.as_str()).unwrap_or(&(0.0, 0));
        let pct = if total_cpu > 0.0 {
            (cpu / total_cpu) * 100.0
        } else {
            0.0
        };

        println!(
            "{:<20}{}{:>10}{}{:>12.1}{}{:>7.1}%",
            account.name, delimiter, jobs, delimiter, cpu, delimiter, pct
        );
    }

    if !args.parsable {
        println!("{}", "-".repeat(56));
        println!(
            "{:<20}{}{:>10}{}{:>12.1}{}{:>7.1}%",
            "TOTAL", delimiter, total_jobs, delimiter, total_cpu, delimiter, 100.0
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
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    let mut user_agg: std::collections::HashMap<(&str, &str), (f64, u64)> =
        std::collections::HashMap::new();
    for e in &usage.entries {
        let u = user_agg.entry((&e.user, &e.account)).or_default();
        u.0 += e.cpu_hours;
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
            "CPU Hours",
            delimiter,
            "% of Tot"
        );
        if !args.parsable {
            println!("{}", "-".repeat(71));
        }
    }

    for user in &users {
        let &(cpu, jobs) = user_agg
            .get(&(user.name.as_str(), user.account.as_str()))
            .unwrap_or(&(0.0, 0));
        let pct = if total_cpu > 0.0 {
            (cpu / total_cpu) * 100.0
        } else {
            0.0
        };

        println!(
            "{:<15}{}{:<20}{}{:>10}{}{:>12.1}{}{:>7.1}%",
            user.name, delimiter, user.account, delimiter, jobs, delimiter, cpu, delimiter, pct
        );
    }

    if !args.parsable {
        println!("{}", "-".repeat(71));
        println!(
            "{:<15}{}{:<20}{}{:>10}{}{:>12.1}{}{:>7.1}%",
            "TOTAL", delimiter, "", delimiter, total_jobs, delimiter, total_cpu, delimiter, 100.0
        );
    }

    Ok(())
}

/// Parse a time argument string into a DateTime.
fn parse_time_arg(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, NaiveDateTime, Utc};
    let s = s.trim();

    // Relative: "now-Ndays", "now-Nhours"
    if let Some(rest) = s.strip_prefix("now-") {
        if let Some(days) = rest
            .strip_suffix("days")
            .or_else(|| rest.strip_suffix("day"))
        {
            let n: i64 = days.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::days(n));
        }
        if let Some(hours) = rest
            .strip_suffix("hours")
            .or_else(|| rest.strip_suffix("hour"))
        {
            let n: i64 = hours.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::hours(n));
        }
    }

    // ISO datetime
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(ndt.and_utc());
    }

    // Date only
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return nd.and_hms_opt(0, 0, 0).map(|ndt| ndt.and_utc());
    }

    None
}

fn datetime_to_proto(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}
