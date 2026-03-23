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

    /// Accounting daemon address
    #[arg(
        long,
        env = "SPUR_ACCOUNTING_ADDR",
        default_value = "http://localhost:6819",
        global = true
    )]
    pub accounting: String,
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

    let mut client = SlurmAccountingClient::connect(args.accounting.clone())
        .await
        .context("failed to connect to spurdbd")?;

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

    for account in &accounts {
        let acct_cpu = usage.cpu_hours.get(&account.name).copied().unwrap_or(0.0);
        let acct_gpu = usage.gpu_hours.get(&account.name).copied().unwrap_or(0.0);
        let acct_jobs = usage.job_count.get(&account.name).copied().unwrap_or(0);

        // Account summary row
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

        // Per-user rows under this account
        let account_users: Vec<_> = users.iter().filter(|u| u.account == account.name).collect();
        for user in &account_users {
            let user_cpu = usage.cpu_hours.get(&user.name).copied().unwrap_or(0.0);
            let user_gpu = usage.gpu_hours.get(&user.name).copied().unwrap_or(0.0);
            let user_jobs = usage.job_count.get(&user.name).copied().unwrap_or(0);

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

    for user in &users {
        let cpu = usage.cpu_hours.get(&user.name).copied().unwrap_or(0.0);
        let gpu = usage.gpu_hours.get(&user.name).copied().unwrap_or(0.0);
        let jobs = usage.job_count.get(&user.name).copied().unwrap_or(0);

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

    let total_jobs: u64 = usage.job_count.values().sum();
    let total_cpu: f64 = usage.cpu_hours.values().sum();

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
        let jobs = usage.job_count.get(&account.name).copied().unwrap_or(0);
        let cpu = usage.cpu_hours.get(&account.name).copied().unwrap_or(0.0);
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

    let total_jobs: u64 = usage.job_count.values().sum();
    let total_cpu: f64 = usage.cpu_hours.values().sum();

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
        let jobs = usage.job_count.get(&user.name).copied().unwrap_or(0);
        let cpu = usage.cpu_hours.get(&user.name).copied().unwrap_or(0.0);
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
