// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Quota policy reconciler.
//!
//! Projects every SPUR account into the native Kubernetes tenancy + quota objects that enforce it
//! (Namespace, ResourceQuota, LimitRange, Role, RoleBinding — the mapping is in [`crate::quota`]).
//! Reads accounts (with their `grp_tres` allocation) and members over the SlurmAccounting gRPC, then
//! server-side-applies the objects with `force`, so the reconciler both fills drift and reverts an
//! admin's hand-edit (the "SPUR-managed" contract). Runs on a level-triggered interval loop; a
//! converged cluster re-applies the same objects (a no-op) and self-heals otherwise.

use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, ResourceQuota};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{Patch, PatchParams};
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use tonic::transport::Channel;
use tracing::{info, warn};

use spur_core::accounting::TresRecord;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{AccountInfo, ListAccountsRequest, ListUsersRequest, UserInfo};

use crate::quota::{self, AccountQuota, MANAGED_BY};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Reconcile loop. Returns `Err` only on a fatal connect error so the caller (`run_with_retry`)
/// restarts it with backoff; per-tick failures are logged and retried on the next interval.
pub async fn run(client: Client, controller_addr: String) -> anyhow::Result<()> {
    let url = if controller_addr.starts_with("http") {
        controller_addr
    } else {
        format!("http://{controller_addr}")
    };
    let mut acct = SlurmAccountingClient::connect(url)
        .await
        .context("connect to spurctld accounting service")?;
    info!("quota reconciler started");

    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        interval.tick().await;
        if let Err(e) = reconcile_once(&client, &mut acct).await {
            warn!(error = %e, "quota reconcile tick failed; retrying next interval");
        }
    }
}

/// One reconcile pass: project every account into its k8s objects.
async fn reconcile_once(
    client: &Client,
    acct: &mut SlurmAccountingClient<Channel>,
) -> anyhow::Result<()> {
    let accounts = acct
        .list_accounts(ListAccountsRequest {})
        .await
        .context("ListAccounts RPC")?
        .into_inner()
        .accounts;

    for a in accounts {
        let users = acct
            .list_users(ListUsersRequest {
                account: a.name.clone(),
            })
            .await
            .context("ListUsers RPC")?
            .into_inner()
            .users;
        let aq = build_account_quota(&a, &users);
        // Isolate per-account failures so one bad account can't stall the rest of the reconcile.
        if let Err(e) = apply_account(client, &aq).await {
            warn!(account = %a.name, error = %e, "failed to reconcile account quota");
        }
    }
    Ok(())
}

/// Build the account's projected quota from its `AccountInfo` (the `grp_tres` allocation) and its
/// member users. Pure — unit-tested.
pub fn build_account_quota(account: &AccountInfo, users: &[UserInfo]) -> AccountQuota {
    AccountQuota {
        account: account.name.clone(),
        // A malformed allocation string leaves the account uncapped rather than stalling the loop.
        grp_tres: TresRecord::parse(&account.grp_tres).unwrap_or_default(),
        members: users.iter().map(|u| u.name.clone()).collect(),
    }
}

/// Apply the Namespace + ResourceQuota + LimitRange + Role + RoleBinding for one account. The
/// Namespace is applied first so the namespaced objects have a home on a fresh cluster.
async fn apply_account(client: &Client, aq: &AccountQuota) -> anyhow::Result<()> {
    let ns = quota::account_namespace(&aq.account);
    apply(
        &Api::<Namespace>::all(client.clone()),
        &quota::namespace(&aq.account),
    )
    .await?;
    apply(
        &Api::<ResourceQuota>::namespaced(client.clone(), &ns),
        &quota::resource_quota(aq),
    )
    .await?;
    apply(
        &Api::<LimitRange>::namespaced(client.clone(), &ns),
        &quota::limit_range(&aq.account),
    )
    .await?;
    apply(
        &Api::<Role>::namespaced(client.clone(), &ns),
        &quota::role(&aq.account),
    )
    .await?;
    apply(
        &Api::<RoleBinding>::namespaced(client.clone(), &ns),
        &quota::role_binding(aq),
    )
    .await?;
    Ok(())
}

/// Server-side apply one object (create-or-update; `force` reverts drift from other field managers).
/// k8s-openapi types don't serialize their `apiVersion`/`kind`, which SSA requires in the body, so
/// they're injected from the type's `Resource` metadata.
async fn apply<K>(api: &Api<K>, obj: &K) -> anyhow::Result<()>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + std::fmt::Debug,
{
    let name = obj
        .meta()
        .name
        .clone()
        .context("SPUR quota object has no name")?;
    let mut body = serde_json::to_value(obj)?;
    body["apiVersion"] = json!(K::api_version(&()).into_owned());
    body["kind"] = json!(K::kind(&()).into_owned());
    api.patch(
        &name,
        &PatchParams::apply(MANAGED_BY).force(),
        &Patch::Apply(body),
    )
    .await
    .with_context(|| format!("server-side apply {name}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::accounting::TresType;

    fn account(name: &str, grp_tres: &str) -> AccountInfo {
        AccountInfo {
            name: name.into(),
            grp_tres: grp_tres.into(),
            ..Default::default()
        }
    }
    fn user(name: &str, account: &str) -> UserInfo {
        UserInfo {
            name: name.into(),
            account: account.into(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_account_quota_from_grp_tres_and_members() {
        let aq = build_account_quota(
            &account("physics", "cpu=16,mem=32768,gres/gpu=8"),
            &[user("alice", "physics"), user("bob", "physics")],
        );
        assert_eq!(aq.account, "physics");
        assert_eq!(aq.grp_tres.get(TresType::Cpu), 16);
        assert_eq!(aq.grp_tres.get(TresType::Memory), 32768);
        assert_eq!(aq.grp_tres.get(TresType::Gpu), 8);
        assert_eq!(aq.members, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn empty_grp_tres_yields_uncapped_allocation() {
        // An account with no allocation string -> empty TresRecord -> ResourceQuota with no caps.
        let aq = build_account_quota(&account("open", ""), &[]);
        assert_eq!(aq.grp_tres.get(TresType::Cpu), 0);
        assert!(quota::quota_hard(&aq.grp_tres).is_empty());
        assert!(aq.members.is_empty());
    }
}
