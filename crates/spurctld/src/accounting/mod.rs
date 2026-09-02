// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod db;
mod fairshare;
mod grpc;
mod notifier;
mod reconcile;
pub(crate) mod txn;

pub use db::JobStartRecord;
pub(crate) use grpc::{accounting_server, AccountingService};
pub use notifier::AccountingNotifier;
pub use reconcile::spawn_loop as spawn_reconcile_loop;
pub use reconcile::spawn_txn_purge_loop;
pub use reconcile::RECONCILE_INTERVAL_SECS;
pub(crate) use txn::{TxnAction, TxnEntity, TxnOutcome, TxnRecord, TxnSource};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use spur_core::accounting::{AccountLimits, TresRecord};
use spur_core::config::SlurmConfig;

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

/// Start accounting for the controller.
///
/// Returns the gRPC service to register (`None` when accounting is disabled).
/// The database connect, migration, and everything downstream of them — the
/// job notifier, the reconcile and purge loops, and the four cache-refresh
/// loops — run in a background task so startup never blocks on the database and,
/// crucially, so a controller that boots while the database is unreachable
/// keeps retrying and wires everything up once it returns. Without that retry a
/// cold start with the database down would leave every cache unloaded for the
/// life of the process, holding every inherited job that names a QOS or account
/// (see `ClusterManager::accounting_block`).
pub(crate) fn start(
    config: &SlurmConfig,
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
) -> Option<AccountingService> {
    if config.accounting.database_url.is_empty() {
        tracing::info!("accounting disabled (database_url not configured)");
        return None;
    }

    let service = AccountingService::unavailable("connecting to accounting database");
    let bringup = service.clone();
    let url = config.accounting.database_url.clone();
    let params = ActivationParams::from_config(config);
    tokio::spawn(async move {
        let pool = connect_with_retry(&url, &bringup).await;
        activate(pool, &bringup, &cluster, &raft, &params);
    });
    Some(service)
}

/// The config values `activate` needs, copied out so the bring-up task does not
/// have to hold the whole `SlurmConfig`.
struct ActivationParams {
    fairshare_halflife_days: u32,
    refresh_secs: u64,
    grp_wall_window_days: u32,
    txn_retention_days: Option<u32>,
}

impl ActivationParams {
    fn from_config(config: &SlurmConfig) -> Self {
        Self {
            fairshare_halflife_days: config.scheduler.fairshare_halflife_days,
            refresh_secs: config.accounting.fairshare_refresh_secs as u64,
            grp_wall_window_days: config.accounting.grp_wall_window_days,
            txn_retention_days: config.accounting.txn_retention_days.filter(|d| *d > 0),
        }
    }
}

/// Which phase of bring-up failed, so the service can report a specific cause
/// while it keeps retrying.
enum BringupError {
    Connect(sqlx::Error),
    Migrate(anyhow::Error),
}

impl BringupError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Connect(_) => "database connection failed",
            Self::Migrate(_) => "database migration failed",
        }
    }
}

impl std::fmt::Display for BringupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "{e}"),
            Self::Migrate(e) => write!(f, "{e}"),
        }
    }
}

async fn connect_and_migrate(url: &str) -> Result<PgPool, BringupError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
        .map_err(BringupError::Connect)?;
    db::migrate(&pool).await.map_err(BringupError::Migrate)?;
    Ok(pool)
}

/// Connect and migrate, retrying with capped exponential backoff until it
/// succeeds. Each failure updates the service's unavailability reason so a
/// client hitting accounting mid-outage sees the current cause.
async fn connect_with_retry(url: &str, service: &AccountingService) -> PgPool {
    const FIRST_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    let mut backoff = FIRST_BACKOFF;
    loop {
        match connect_and_migrate(url).await {
            Ok(pool) => {
                tracing::info!("accounting database connected");
                return pool;
            }
            Err(e) => {
                service.mark_unavailable(e.reason());
                tracing::warn!(
                    error = %e,
                    retry_in_secs = backoff.as_secs(),
                    "accounting database unavailable; retrying in background"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Wire a freshly connected pool into the controller: install it into the gRPC
/// service, set the job notifier, and spawn the reconcile, purge, and
/// cache-refresh loops. Runs once per successful connect — at startup when the
/// database is up, or later when a cold-start outage clears.
fn activate(
    pool: PgPool,
    service: &AccountingService,
    cluster: &Arc<ClusterManager>,
    raft: &Arc<RaftHandle>,
    params: &ActivationParams,
) {
    service.install_pool(pool.clone());
    cluster.set_accounting(AccountingNotifier::new(pool.clone()));

    spawn_reconcile_loop(
        pool.clone(),
        cluster.clone(),
        raft.clone(),
        Duration::from_secs(RECONCILE_INTERVAL_SECS),
    );

    if let Some(days) = params.txn_retention_days {
        spawn_txn_purge_loop(pool.clone(), raft.clone(), days, Duration::from_secs(3600));
    }

    cluster.fairshare_cache().spawn_refresh_loop(
        pool.clone(),
        params.fairshare_halflife_days,
        params.refresh_secs,
    );
    cluster
        .qos_cache()
        .spawn_refresh_loop(pool.clone(), params.refresh_secs);
    cluster
        .association_cache()
        .spawn_refresh_loop(pool.clone(), params.refresh_secs);
    cluster.grp_wall_cache().spawn_refresh_loop(
        pool,
        params.refresh_secs,
        params.grp_wall_window_days,
    );
}

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

/// Canonicalize an `adminlevel` to Slurm's spelling, or `None` if it is not a level.
///
/// Slurm prints `Administrator` for the highest level and its parser also takes `Admin` and
/// `SuperUser`, so all three must resolve to the same thing — recognising only one spelling would
/// leave a stored level that looks like a privilege and confers nothing.
pub fn canonical_admin_level(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "none" => Some("None"),
        "operator" => Some("Operator"),
        "admin" | "administrator" | "superuser" => Some("Administrator"),
        _ => None,
    }
}

/// Whether a stored `admin_level` is the level that confers control-plane privilege.
pub fn admin_level_is_admin(raw: &str) -> bool {
    canonical_admin_level(raw) == Some("Administrator")
}

/// Fold one user-row's admin_level into the per-user map, keeping the highest: admin wins over
/// any lower level so a later non-admin row for a multi-account user can't clobber it. Stored
/// canonical, so rows predating the column's normalization cannot leave several spellings of one
/// level in the cache; a value that is no level at all is kept verbatim, to stay visible.
fn merge_admin_level(map: &mut HashMap<String, String>, user: &str, level: &str) {
    if level.is_empty() || level.eq_ignore_ascii_case("none") {
        return;
    }
    let entry = map.entry(user.to_owned()).or_default();
    if entry.is_empty() || admin_level_is_admin(level) {
        *entry = canonical_admin_level(level).unwrap_or(level).to_owned();
    }
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
    HashMap<(String, String), HashSet<String>>,
    HashMap<String, String>,
)> {
    let users = db::list_users(pool, None, None).await?;

    let mut default_qos = HashMap::new();
    let mut default_account = HashMap::new();
    let mut memberships = HashSet::new();
    let mut allowed_qos = HashMap::new();
    let mut admin_level = HashMap::new();
    for u in users {
        let key = (u.name.clone(), u.account.clone());
        memberships.insert(key.clone());
        merge_admin_level(&mut admin_level, &u.name, &u.admin_level);
        if let Some(qos) = u.default_qos {
            default_qos.insert(key.clone(), qos);
        }
        if let Some(acct) = u.default_account {
            default_account.insert(u.name, acct);
        }
        if let Some(list) = u.allowed_qos {
            let set: HashSet<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if !set.is_empty() {
                allowed_qos.insert(key, set);
            }
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

    Ok((
        default_qos,
        default_account,
        memberships,
        limits,
        allowed_qos,
        admin_level,
    ))
}

fn account_limits_from_record(r: db::AssociationRecord) -> AccountLimits {
    use spur_core::accounting::limit_from_db;
    // Values are validated by `add_user` before being stored, so a parse
    // failure here means the DB row predates that check or was edited
    // out-of-band; treat it as unset rather than poisoning the whole load.
    let opt_tres = |s: Option<String>| {
        s.filter(|s| !s.is_empty()).and_then(|s| {
            TresRecord::parse(&s)
                .inspect_err(
                    |e| tracing::warn!(tres = %s, error = %e, "dropping unparseable stored TRES"),
                )
                .ok()
        })
    };

    AccountLimits {
        max_running_jobs: limit_from_db(r.max_running_jobs),
        max_submit_jobs: limit_from_db(r.max_submit_jobs),
        grp_submit_jobs: limit_from_db(r.grp_submit_jobs),
        max_tres_per_job: opt_tres(r.max_tres_per_job),
        grp_tres: opt_tres(r.grp_tres),
        max_wall_minutes: limit_from_db(r.max_wall_min),
    }
}

#[cfg(test)]
mod tests {
    use super::merge_admin_level;
    use std::collections::HashMap;

    #[test]
    fn admin_wins_regardless_of_row_order() {
        // Admin then Operator: Admin must survive the later non-admin row.
        let mut m = HashMap::new();
        merge_admin_level(&mut m, "carol", "Admin");
        merge_admin_level(&mut m, "carol", "Operator");
        assert_eq!(m.get("carol").map(String::as_str), Some("Administrator"));

        // Operator then Admin: Admin must still win.
        let mut m = HashMap::new();
        merge_admin_level(&mut m, "carol", "Operator");
        merge_admin_level(&mut m, "carol", "Admin");
        assert_eq!(m.get("carol").map(String::as_str), Some("Administrator"));
    }

    /// Rows written before the column was normalized must not leave several spellings of one level
    /// in the cache.
    #[test]
    fn legacy_spellings_fold_to_one_canonical_level() {
        for raw in ["admin", "Admin", "Administrator", "SuperUser"] {
            let mut m = HashMap::new();
            merge_admin_level(&mut m, "carol", raw);
            assert_eq!(
                m.get("carol").map(String::as_str),
                Some("Administrator"),
                "raw {raw:?}"
            );
        }

        let mut m = HashMap::new();
        merge_admin_level(&mut m, "dave", "operator");
        assert_eq!(m.get("dave").map(String::as_str), Some("Operator"));
    }

    #[test]
    fn none_and_empty_levels_are_dropped() {
        let mut m = HashMap::new();
        merge_admin_level(&mut m, "dave", "none");
        merge_admin_level(&mut m, "dave", "");
        assert!(!m.contains_key("dave"));
    }

    fn record_with_submit(grp_submit_jobs: Option<i32>) -> super::db::AssociationRecord {
        super::db::AssociationRecord {
            user_name: "alice".into(),
            account: "research".into(),
            max_running_jobs: None,
            max_submit_jobs: None,
            grp_submit_jobs,
            max_tres_per_job: None,
            grp_tres: None,
            max_wall_min: None,
        }
    }

    #[test]
    fn account_limits_preserve_zero_as_block_all() {
        let limits = super::account_limits_from_record(record_with_submit(Some(0)));
        assert_eq!(limits.grp_submit_jobs, Some(0));
    }

    #[test]
    fn account_limits_map_null_and_negative_to_unset() {
        let from_null = super::account_limits_from_record(record_with_submit(None));
        assert_eq!(from_null.grp_submit_jobs, None);

        // A stray negative predates the sentinel flip; treat it as unset.
        let from_negative = super::account_limits_from_record(record_with_submit(Some(-1)));
        assert_eq!(from_negative.grp_submit_jobs, None);
    }

    #[test]
    fn account_limits_pass_through_positive() {
        let limits = super::account_limits_from_record(record_with_submit(Some(5)));
        assert_eq!(limits.grp_submit_jobs, Some(5));
    }

    #[test]
    fn bringup_error_reason_names_the_failed_phase() {
        let connect = super::BringupError::Connect(sqlx::Error::PoolClosed);
        assert_eq!(connect.reason(), "database connection failed");
        let migrate = super::BringupError::Migrate(anyhow::anyhow!("bad migration"));
        assert_eq!(migrate.reason(), "database migration failed");
    }
}
