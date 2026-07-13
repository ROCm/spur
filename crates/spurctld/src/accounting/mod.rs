// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod db;
mod fairshare;
mod grpc;
mod notifier;
mod reconcile;

pub use grpc::accounting_server;
pub use notifier::{AccountingNotifier, JobStartRecord};
pub use reconcile::spawn_loop as spawn_reconcile_loop;

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;

use spur_core::accounting::{AccountLimits, TresRecord};

/// Compute fairshare factors directly from the database.
///
/// Reused by both the gRPC `GetFairshareFactors` RPC and the controller's
/// in-process `FairshareCache`.
pub async fn fairshare_factors(
    pool: &PgPool,
    halflife_days: u32,
) -> anyhow::Result<HashMap<(String, String), f64>> {
    let halflife_days = if halflife_days == 0 {
        14
    } else {
        halflife_days.clamp(1, 365)
    };
    let now = chrono::Utc::now();
    let since = now - chrono::Duration::days(halflife_days as i64 * 4);

    let usage = db::get_usage(pool, None, None, since).await?;
    let accounts = db::list_accounts(pool).await?;

    let account_weights: HashMap<String, f64> = accounts
        .into_iter()
        .map(|a| (a.name, a.fairshare_weight as f64))
        .collect();

    Ok(fairshare::compute_fairshare(
        &usage,
        &account_weights,
        halflife_days,
        now,
    ))
}

/// Load association defaults, the full user→account membership set, and
/// per-association resource limits backing the controller's `AssociationCache`.
pub async fn association_maps(
    pool: &PgPool,
) -> anyhow::Result<(
    HashMap<(String, String), String>,
    HashMap<String, String>,
    HashSet<(String, String)>,
    HashMap<(String, String), AccountLimits>,
)> {
    let users = db::list_users(pool, None).await?;

    let mut default_qos = HashMap::new();
    let mut default_account = HashMap::new();
    let mut memberships = HashSet::new();
    for u in users {
        memberships.insert((u.name.clone(), u.account.clone()));
        if let Some(qos) = u.default_qos {
            default_qos.insert((u.name.clone(), u.account), qos);
        }
        if let Some(acct) = u.default_account {
            default_account.insert(u.name, acct);
        }
    }

    let limits = db::list_associations(pool)
        .await?
        .into_iter()
        .map(|a| {
            let key = (a.user_name.clone(), a.account.clone());
            (key, account_limits_from_record(a))
        })
        .collect();

    Ok((default_qos, default_account, memberships, limits))
}

fn account_limits_from_record(r: db::AssociationRecord) -> AccountLimits {
    let opt_u32 = |v: Option<i32>| v.filter(|&x| x > 0).map(|x| x as u32);
    let opt_tres = |s: Option<String>| s.filter(|s| !s.is_empty()).map(|s| TresRecord::parse(&s));

    AccountLimits {
        max_running_jobs: opt_u32(r.max_running_jobs),
        max_submit_jobs: opt_u32(r.max_submit_jobs),
        max_tres_per_job: opt_tres(r.max_tres_per_job),
        grp_tres: opt_tres(r.grp_tres),
        max_wall_minutes: opt_u32(r.max_wall_min),
    }
}
