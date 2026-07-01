// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod db;
mod fairshare;
mod grpc;
mod notifier;

pub use grpc::accounting_server;
pub use notifier::AccountingNotifier;

use std::collections::HashMap;

use sqlx::PgPool;

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
