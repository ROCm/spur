// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::{
    GetFairshareFactorsRequest, GetUsageRequest, ListAccountsRequest, ListUsersRequest,
};

use crate::usage_scope::{cell, UsageScope};

/// Display fair-share information by account and user.
#[derive(Parser, Debug)]
// -h is sshare's --noheader (Slurm convention), so disable clap's auto -h and
// re-add --help below as long-only.
#[command(
    name = "sshare",
    about = "Show fair-share information",
    disable_help_flag = true
)]
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

    /// Accepted for Slurm compatibility; has no effect
    #[arg(long)]
    pub noconvert: bool,

    /// Print help
    #[arg(long, action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// Controller address (accounting is served on the same port)
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
    let args = crate::clap_exit::parse_or_exit::<SshareArgs>(&args);

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to controller")?;
    let mut client = spur_proto::accounting_client(channel);

    // Get accounts
    let accounts_resp = client
        .list_accounts(ListAccountsRequest { shares: true })
        .await
        .context("failed to list accounts")?;
    let accounts_reply = accounts_resp.into_inner();
    let accounts = accounts_reply.accounts;

    // Get users
    let users_resp = client
        .list_users(ListUsersRequest {
            account: args.account.clone().unwrap_or_default(),
            user: String::new(),
            associations: true,
        })
        .await
        .context("failed to list users")?;
    let users_reply = users_resp.into_inner();
    let users = users_reply.users;

    // Get usage data
    let usage_resp = client
        .get_usage(GetUsageRequest {
            user: String::new(),
            account: String::new(),
            since: None,
            job_scoped: false,
        })
        .await
        .context("failed to get usage")?;
    let usage = usage_resp.into_inner();

    // Share ratios divide by the accounts and users the controller returned.
    // A pinned directory is not the cluster, so those ratios stay blank.
    let (accounts_open, shares_open) = share_ratios_visible(
        accounts_reply.partial,
        users_reply.scoped_to_user.as_deref(),
        users_reply.partial,
    );

    let (fairshare_map, factors_scoped_to, factors_partial): (
        HashMap<(String, String), f64>,
        Option<String>,
        bool,
    ) = match client
        .get_fairshare_factors(GetFairshareFactorsRequest {
            halflife_days: 0,
            job_scoped: false,
        })
        .await
    {
        Ok(resp) => {
            let resp = resp.into_inner();
            (
                resp.entries
                    .into_iter()
                    .map(|e| ((e.user, e.account), e.factor))
                    .collect(),
                resp.scoped_to_user,
                resp.partial,
            )
        }
        Err(e) => {
            // The account-row fallback is the only approximation this warning
            // can be about. A single-user pin and a partial directory blank it
            // for different reasons.
            let usage_open = usage.scoped_to_user.is_none() && !usage.partial;
            let single_user = usage.scoped_to_user.is_some();
            if account_fair_shown(false, usage_open, accounts_open, single_user) {
                eprintln!(
                    "sshare: warning: could not fetch fair-share factors from controller: {e}\n\
                         Values shown are local approximations and may differ from the scheduler."
                );
            } else if single_user {
                eprintln!(
                    "sshare: warning: could not fetch fair-share factors from controller: {e}\n\
                         FairShare is blank; the usage reply is only this caller."
                );
            } else {
                eprintln!(
                    "sshare: warning: could not fetch fair-share factors from controller: {e}\n\
                         FairShare is blank; the account list is not the cluster share total."
                );
            }
            (HashMap::new(), None, false)
        }
    };
    let scope = report_usage_scope(
        usage.scoped_to_user.as_deref(),
        factors_scoped_to.as_deref(),
        usage.partial,
        factors_partial,
        accounts_reply.partial,
        users_reply.partial,
    );

    let account_fairshare: HashMap<&str, f64> = {
        let mut sums: HashMap<&str, (f64, usize)> = HashMap::new();
        for ((_, acct), factor) in &fairshare_map {
            if let Some(a) = accounts.iter().find(|a| a.name == *acct) {
                let e = sums.entry(a.name.as_str()).or_default();
                e.0 += factor;
                e.1 += 1;
            }
        }
        sums.into_iter()
            .map(|(acct, (sum, count))| (acct, sum / count as f64))
            .collect()
    };

    // Compute total shares for normalization
    let total_shares: f64 = accounts.iter().map(|a| a.fairshare_weight).sum();
    let total_shares = if total_shares <= 0.0 {
        1.0
    } else {
        total_shares
    };

    // Build lookup maps from entries (server guarantees one entry per user+account)
    let (w_cpu, w_gpu) = if usage.has_billing_weights {
        (usage.billing_weight_cpu, usage.billing_weight_gpu)
    } else {
        (1.0, 1.0)
    };

    let mut account_cpu_secs: HashMap<&str, f64> = HashMap::new();
    let mut account_gpu_secs: HashMap<&str, f64> = HashMap::new();
    let mut account_billing: HashMap<&str, f64> = HashMap::new();
    let mut user_account_cpu_secs: HashMap<(&str, &str), f64> = HashMap::new();
    let mut user_account_gpu_secs: HashMap<(&str, &str), f64> = HashMap::new();
    let mut user_account_billing: HashMap<(&str, &str), f64> = HashMap::new();
    for entry in &usage.entries {
        *account_cpu_secs.entry(&entry.account).or_default() += entry.cpu_seconds;
        *account_gpu_secs.entry(&entry.account).or_default() += entry.gpu_seconds;
        let bill = w_cpu * entry.cpu_seconds + w_gpu * entry.gpu_seconds;
        *account_billing.entry(&entry.account).or_default() += bill;
        user_account_cpu_secs.insert((&entry.user, &entry.account), entry.cpu_seconds);
        user_account_gpu_secs.insert((&entry.user, &entry.account), entry.gpu_seconds);
        user_account_billing.insert(
            (&entry.user, &entry.account),
            w_cpu * entry.cpu_seconds + w_gpu * entry.gpu_seconds,
        );
    }

    // Compute total usage for normalization (weighted billing)
    let cluster_usage: f64 = account_billing.values().sum();
    let cluster_usage = if cluster_usage <= 0.0 {
        1.0
    } else {
        cluster_usage
    };

    if args.long {
        if !args.noheader {
            println!(
                "{:<15} {:<15} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                "Account",
                "User",
                "RawShares",
                "NormShares",
                "RawUsage",
                "NormUsage",
                "FairShare",
                "CPURawUsage",
                "GPURawUsage"
            );
            println!("{}", "-".repeat(117));
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
        let raw_usage = account_billing
            .get(account.name.as_str())
            .copied()
            .unwrap_or(0.0);
        let norm_usage = raw_usage / cluster_usage;
        let account_factor = account_fairshare.get(account.name.as_str()).copied();
        let fair_fallback = if norm_usage > 0.001 {
            (norm_shares / norm_usage).min(10.0)
        } else {
            (norm_shares / 0.001).min(10.0)
        };
        let fair_share = account_factor.unwrap_or(fair_fallback);
        let totals = scope.totals_visible();
        // A missing usage row is a true zero only for an account the controller
        // named in `visible_accounts`. Printing 0 for any other account would
        // claim it went unused.
        let in_reply = account_billing.contains_key(account.name.as_str());
        let in_scope = account_in_usage_scope(&usage.visible_accounts, &account.name, in_reply);
        let account_usage = account_usage_shown(scope.account_rows_visible(), totals, in_scope);
        let fair_shown = account_fair_shown(
            account_factor.is_some(),
            totals,
            accounts_open,
            scope.is_single_user(),
        );

        let cpu_raw = account_cpu_secs
            .get(account.name.as_str())
            .copied()
            .unwrap_or(0.0);
        let gpu_raw = account_gpu_secs
            .get(account.name.as_str())
            .copied()
            .unwrap_or(0.0);

        let raw_long = cell(account_usage, 12, || format!("{raw_usage:>12.1}"));
        let raw_short = cell(account_usage, 12, || format!("{raw_usage:>12.0}"));
        let norm_shares_cell = cell(accounts_open, 12, || format!("{norm_shares:>12.6}"));
        let norm = cell(totals, 12, || format!("{norm_usage:>12.6}"));
        let fair = cell(fair_shown, 12, || format!("{fair_share:>12.6}"));
        let cpu = cell(account_usage, 12, || format!("{cpu_raw:>12.1}"));
        let gpu = cell(account_usage, 12, || format!("{gpu_raw:>12.1}"));

        // Account-level row
        if args.long {
            println!(
                "{:<15} {:<15} {:>12} {} {} {} {} {} {}",
                account.name,
                "",
                raw_shares as u32,
                norm_shares_cell,
                raw_long,
                norm,
                fair,
                cpu,
                gpu,
            );
        } else {
            println!(
                "{:<15} {:<15} {:>12} {} {} {} {}",
                account.name, "", raw_shares as u32, norm_shares_cell, raw_short, norm, fair,
            );
        }

        if user_rows_shown(&scope, in_scope) {
            let members: Vec<_> = users.iter().filter(|u| u.account == account.name).collect();
            for user in &members {
                // Filter by user if specified
                if let Some(ref filter_user) = args.user {
                    if &user.name != filter_user {
                        continue;
                    }
                }
                // Hidden users stay in `members`, so a visible user's NormShares is
                // still their fraction of the account. Slurm omits the hidden rows.
                if !scope.user_visible(&user.name) {
                    continue;
                }

                let user_usage = user_account_billing
                    .get(&(user.name.as_str(), account.name.as_str()))
                    .copied()
                    .unwrap_or(0.0);
                let user_norm_usage = user_usage / cluster_usage;
                // Each user within an account gets an equal sub-share
                let user_count = members.len().max(1) as f64;
                let user_norm_shares = norm_shares / user_count;
                let user_factor = fairshare_map
                    .get(&(user.name.clone(), account.name.clone()))
                    .copied();
                let user_fair_share = user_factor.unwrap_or_else(|| {
                    if user_norm_usage > 0.001 {
                        (user_norm_shares / user_norm_usage).min(10.0)
                    } else {
                        (user_norm_shares / 0.001).min(10.0)
                    }
                });

                let user_cpu = user_account_cpu_secs
                    .get(&(user.name.as_str(), account.name.as_str()))
                    .copied()
                    .unwrap_or(0.0);
                let user_gpu = user_account_gpu_secs
                    .get(&(user.name.as_str(), account.name.as_str()))
                    .copied()
                    .unwrap_or(0.0);

                let mine = scope.user_visible(&user.name);
                let fair_shown = user_fair_shown(mine, user_factor.is_some(), totals, shares_open);
                let raw_long = cell(mine, 12, || format!("{user_usage:>12.1}"));
                let raw_short = cell(mine, 12, || format!("{user_usage:>12.0}"));
                let user_shares = cell(shares_open, 12, || format!("{user_norm_shares:>12.6}"));
                let norm = cell(mine && totals, 12, || format!("{user_norm_usage:>12.6}"));
                let fair = cell(fair_shown, 12, || format!("{user_fair_share:>12.6}"));
                let cpu = cell(mine, 12, || format!("{user_cpu:>12.1}"));
                let gpu = cell(mine, 12, || format!("{user_gpu:>12.1}"));

                if args.long {
                    println!(
                        " {:<14} {:<15} {:>12} {} {} {} {} {} {}",
                        "",
                        user.name,
                        raw_shares as u32,
                        user_shares,
                        raw_long,
                        norm,
                        fair,
                        cpu,
                        gpu,
                    );
                } else {
                    println!(
                        " {:<14} {:<15} {:>12} {} {} {} {}",
                        "", user.name, raw_shares as u32, user_shares, raw_short, norm, fair,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Usage totals follow the usage and fairshare replies. A partial account or
/// user directory changes share ratios (`share_ratios_visible`); folding it in
/// here would blank usage that the controller actually returned in full.
fn report_usage_scope(
    usage_scoped: Option<&str>,
    factors_scoped: Option<&str>,
    usage_partial: bool,
    factors_partial: bool,
    _accounts_partial: bool,
    _users_partial: bool,
) -> UsageScope {
    UsageScope::from_replies([usage_scoped, factors_scoped])
        .with_partial([usage_partial, factors_partial])
}

/// A pinned caller keeps their own rows even where they have no usage, as
/// Slurm's `sshare` does. A coordinator's partial reply lists users only in
/// accounts it covers.
fn user_rows_shown(scope: &UsageScope, in_scope: bool) -> bool {
    scope.totals_visible() || scope.is_single_user() || in_scope
}

fn account_usage_shown(account_rows: bool, totals: bool, in_scope: bool) -> bool {
    account_rows && (totals || in_scope)
}

/// An account with no usage row is still in scope when the controller listed it.
/// A usage entry counts too, so an empty list keeps the rows the reply returned.
fn account_in_usage_scope(visible_accounts: &[String], account: &str, in_reply: bool) -> bool {
    in_reply || visible_accounts.iter().any(|name| name == account)
}

/// `(account NormShares, per-user NormShares)`.
/// A pinned account list is not the cluster total, so both ratios are wrong.
/// A pinned user list still has that total, but the per-user split counts
/// only the caller.
fn share_ratios_visible(
    accounts_partial: bool,
    users_scoped: Option<&str>,
    users_partial: bool,
) -> (bool, bool) {
    let account_shares = !accounts_partial;
    let users_complete =
        UsageScope::from_replies([users_scoped]).totals_visible() && !users_partial;
    (account_shares, account_shares && users_complete)
}

/// A controller factor is the account's when the reply is not a single user.
/// The local fallback divides by NormShares, which a partial account list inflates.
fn account_fair_shown(
    has_factor: bool,
    usage_open: bool,
    account_shares: bool,
    single_user: bool,
) -> bool {
    if has_factor {
        !single_user
    } else {
        usage_open && account_shares
    }
}

/// The caller's own factor is real. The local approximation divides by a
/// share ratio, which a pinned account or user list inflates.
fn user_fair_shown(mine: bool, has_factor: bool, usage_open: bool, user_shares: bool) -> bool {
    mine && (has_factor || (usage_open && user_shares))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noconvert_is_accepted() {
        // Slurm scripts pass --noconvert to keep output machine-parseable.
        let args = SshareArgs::try_parse_from(["sshare", "--noconvert"]).unwrap();
        assert!(args.noconvert);
    }

    #[test]
    fn short_h_means_noheader() {
        let args = SshareArgs::try_parse_from(["sshare", "-h"]).expect("-h is accepted");
        assert!(args.noheader);
    }

    #[test]
    fn a_pinned_caller_keeps_their_rows_in_an_account_they_have_not_used() {
        let pinned = report_usage_scope(Some("alice"), None, false, false, false, false);
        assert!(user_rows_shown(&pinned, false));
    }

    #[test]
    fn a_partial_reply_omits_users_of_accounts_outside_it() {
        let partial = report_usage_scope(None, None, true, false, false, false);
        assert!(!user_rows_shown(&partial, false));
        assert!(user_rows_shown(&partial, true));
    }

    #[test]
    fn a_partial_user_directory_does_not_hide_complete_usage() {
        let scope = report_usage_scope(None, None, false, false, false, true);
        assert!(scope.totals_visible());
        assert!(scope.account_rows_visible());
        assert!(account_usage_shown(
            scope.account_rows_visible(),
            scope.totals_visible(),
            false
        ));
    }

    #[test]
    fn a_named_account_with_no_usage_row_is_a_true_zero() {
        let visible = vec!["ml".to_string()];
        let scope = report_usage_scope(None, None, true, false, false, false);
        assert!(account_in_usage_scope(&visible, "ml", false));
        assert!(!account_in_usage_scope(&visible, "physics", false));
        assert!(account_in_usage_scope(&[], "physics", true));
        assert!(!account_in_usage_scope(&[], "physics", false));
        assert!(account_usage_shown(
            scope.account_rows_visible(),
            scope.totals_visible(),
            account_in_usage_scope(&visible, "ml", false),
        ));
        assert!(!account_usage_shown(
            scope.account_rows_visible(),
            scope.totals_visible(),
            account_in_usage_scope(&visible, "physics", false),
        ));
    }

    #[test]
    fn a_partial_usage_reply_still_hides_accounts_outside_it() {
        let scope = report_usage_scope(None, None, true, false, false, true);
        assert!(!scope.totals_visible());
        assert!(scope.account_rows_visible());
        assert!(!account_usage_shown(
            scope.account_rows_visible(),
            scope.totals_visible(),
            false
        ));
        assert!(account_usage_shown(
            scope.account_rows_visible(),
            scope.totals_visible(),
            true
        ));
    }

    #[test]
    fn pinned_directories_hide_the_share_ratios_they_inflate() {
        assert_eq!(share_ratios_visible(false, None, false), (true, true));
        assert_eq!(share_ratios_visible(true, None, false), (false, false));
        assert_eq!(
            share_ratios_visible(false, Some("alice"), false),
            (true, false)
        );
        assert_eq!(share_ratios_visible(true, Some(""), true), (false, false));
        assert!(account_usage_shown(true, true, false));
        assert!(account_usage_shown(true, false, true));
        assert!(!account_usage_shown(true, false, false));
        assert!(!account_usage_shown(false, false, true));
    }

    #[test]
    fn local_fairshare_is_hidden_when_its_share_ratio_is() {
        assert!(account_fair_shown(true, true, false, false));
        assert!(!account_fair_shown(true, false, true, true));
        assert!(account_fair_shown(true, false, false, false));
        assert!(!account_fair_shown(false, true, false, false));
        assert!(account_fair_shown(false, true, true, false));

        assert!(user_fair_shown(true, true, false, false));
        assert!(!user_fair_shown(true, false, true, false));
        assert!(user_fair_shown(true, false, true, true));
        assert!(!user_fair_shown(false, true, true, true));
    }

    #[test]
    fn help_is_still_reachable_by_its_long_name() {
        let err =
            SshareArgs::try_parse_from(["sshare", "--help"]).expect_err("--help stops parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
