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

/// Account definitions. A non-coordinator is denied when `accounts` is private;
/// the caller then builds rows from the usage it is allowed to see.
async fn account_directory(client: &mut AcctClient) -> Result<Vec<spur_proto::proto::AccountInfo>> {
    match client.list_accounts(ListAccountsRequest {}).await {
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

    let scope =
        UsageScope::from_replies([usage.scoped_to_user.as_deref()]).with_partial([usage.partial]);
    for account in &accounts {
        let &(acct_cpu, acct_gpu, acct_jobs) = acct_agg
            .get(account.name.as_str())
            .unwrap_or(&(0.0, 0.0, 0));
        let account_usage = scope.account_rows_visible();

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
            let mine = scope.user_visible(&user.name);

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

    let scope =
        UsageScope::from_replies([usage.scoped_to_user.as_deref()]).with_partial([usage.partial]);
    for user in &users {
        let &(cpu, gpu, jobs) = user_agg
            .get(&(user.name.as_str(), user.account.as_str()))
            .unwrap_or(&(0.0, 0.0, 0));
        let mine = scope.user_visible(&user.name);

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

#[cfg(test)]
mod tests {
    use super::*;

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
