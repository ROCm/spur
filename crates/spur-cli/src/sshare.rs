use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{GetUsageRequest, ListAccountsRequest, ListUsersRequest};

/// Display fair-share information by account and user.
#[derive(Parser, Debug)]
#[command(name = "sshare", about = "Show fair-share information")]
pub struct SshareArgs {
    /// Show only this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Show only this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Long format (more columns)
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
    pub noheader: bool,

    /// Accounting daemon address
    #[arg(
        long,
        env = "SPUR_ACCOUNTING_ADDR",
        default_value = "http://localhost:6819"
    )]
    pub accounting: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SshareArgs::try_parse_from(&args)?;

    let mut client = SlurmAccountingClient::connect(args.accounting.clone())
        .await
        .context("failed to connect to spurdbd")?;

    // Get accounts
    let accounts_resp = client
        .list_accounts(ListAccountsRequest {})
        .await
        .context("failed to list accounts")?;
    let accounts = accounts_resp.into_inner().accounts;

    // Get users
    let users_resp = client
        .list_users(ListUsersRequest {
            account: args.account.clone().unwrap_or_default(),
        })
        .await
        .context("failed to list users")?;
    let users = users_resp.into_inner().users;

    // Get usage data
    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: args.user.clone().unwrap_or_default(),
            account: args.account.clone().unwrap_or_default(),
            since: None,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    // Compute total shares for normalization
    let total_shares: f64 = accounts.iter().map(|a| a.fairshare_weight).sum();
    let total_shares = if total_shares <= 0.0 {
        1.0
    } else {
        total_shares
    };

    // Compute total usage for normalization
    let total_cpu_usage: f64 = usage.cpu_hours.values().sum();
    let total_cpu_usage = if total_cpu_usage <= 0.0 {
        1.0
    } else {
        total_cpu_usage
    };

    if args.long {
        if !args.noheader {
            println!(
                "{:<15} {:<15} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                "Account",
                "User",
                "RawShares",
                "NormShares",
                "RawUsage",
                "NormUsage",
                "FairShare",
                "GrpCPUHrs"
            );
            println!("{}", "-".repeat(101));
        }
    } else if !args.noheader {
        println!(
            "{:<15} {:<15} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "Account", "User", "RawShares", "NormShares", "RawUsage", "NormUsage", "FairShare"
        );
        println!("{}", "-".repeat(93));
    }

    for account in &accounts {
        // Filter by account if specified
        if let Some(ref filter_acct) = args.account {
            if &account.name != filter_acct {
                continue;
            }
        }

        let raw_shares = account.fairshare_weight;
        let norm_shares = raw_shares / total_shares;
        let raw_usage = usage.cpu_hours.get(&account.name).copied().unwrap_or(0.0);
        let norm_usage = raw_usage / total_cpu_usage;
        let fair_share = if norm_usage > 0.001 {
            norm_shares / norm_usage
        } else {
            // No usage = maximum fair share (capped)
            norm_shares / 0.001
        };
        let fair_share = fair_share.min(10.0);

        // Account-level row
        if args.long {
            println!(
                "{:<15} {:<15} {:>12} {:>12.6} {:>12.1} {:>12.6} {:>12.6} {:>12.1}",
                account.name,
                "",
                raw_shares as u32,
                norm_shares,
                raw_usage,
                norm_usage,
                fair_share,
                raw_usage,
            );
        } else {
            println!(
                "{:<15} {:<15} {:>12} {:>12.6} {:>12.1} {:>12.6} {:>12.6}",
                account.name, "", raw_shares as u32, norm_shares, raw_usage, norm_usage, fair_share,
            );
        }

        // User-level rows under this account
        let account_users: Vec<_> = users.iter().filter(|u| u.account == account.name).collect();
        for user in &account_users {
            // Filter by user if specified
            if let Some(ref filter_user) = args.user {
                if &user.name != filter_user {
                    continue;
                }
            }

            let user_usage = usage.cpu_hours.get(&user.name).copied().unwrap_or(0.0);
            let user_norm_usage = user_usage / total_cpu_usage;
            // Each user within an account gets an equal sub-share
            let user_count = account_users.len().max(1) as f64;
            let user_norm_shares = norm_shares / user_count;
            let user_fair_share = if user_norm_usage > 0.001 {
                user_norm_shares / user_norm_usage
            } else {
                user_norm_shares / 0.001
            };
            let user_fair_share = user_fair_share.min(10.0);

            if args.long {
                println!(
                    " {:<14} {:<15} {:>12} {:>12.6} {:>12.1} {:>12.6} {:>12.6} {:>12.1}",
                    "",
                    user.name,
                    raw_shares as u32,
                    user_norm_shares,
                    user_usage,
                    user_norm_usage,
                    user_fair_share,
                    user_usage,
                );
            } else {
                println!(
                    " {:<14} {:<15} {:>12} {:>12.6} {:>12.1} {:>12.6} {:>12.6}",
                    "",
                    user.name,
                    raw_shares as u32,
                    user_norm_shares,
                    user_usage,
                    user_norm_usage,
                    user_fair_share,
                );
            }
        }
    }

    Ok(())
}
