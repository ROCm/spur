// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::format_engine;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::*;

/// Accounting management commands.
#[derive(Parser, Debug)]
#[command(name = "sacctmgr", about = "Spur accounting manager")]
pub struct SacctmgrArgs {
    #[command(subcommand)]
    pub command: SacctmgrCommand,

    /// Controller address (accounting is served on the same port)
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    pub controller: String,

    /// Immediate mode (no confirmation)
    #[arg(short = 'i', long, global = true)]
    pub immediate: bool,
}

#[derive(Subcommand, Debug)]
pub enum SacctmgrCommand {
    /// Add entities
    Add {
        /// Entity type: account, user, qos
        entity: String,
        /// key=value pairs
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Delete entities
    Delete {
        /// Entity type: account, user, qos
        entity: String,
        /// key=value pairs (name= or where clause)
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Modify entities
    Modify {
        /// Entity type: account, user, qos
        entity: String,
        /// key=value pairs
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// List/show entities
    Show {
        /// Entity type: account, user, qos, association
        entity: String,
        /// Optional filter
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// List entities (alias for show)
    List {
        entity: String,
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SacctmgrArgs::try_parse_from(&args)?;
    let addr = args.controller.clone();

    match args.command {
        SacctmgrCommand::Add { entity, params } => add(&entity, &params, &addr).await,
        SacctmgrCommand::Delete { entity, params } => delete(&entity, &params, &addr).await,
        SacctmgrCommand::Modify { entity, params } => modify(&entity, &params, &addr).await,
        SacctmgrCommand::Show { entity, params } | SacctmgrCommand::List { entity, params } => {
            show(&entity, &params, &addr).await
        }
    }
}

fn parse_params(params: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    // Handle both "key=value" and "key value" forms
    let mut iter = params.iter();
    while let Some(param) = iter.next() {
        if let Some((key, value)) = param.split_once('=') {
            map.insert(key.to_lowercase(), value.to_string());
        } else if param.starts_with("where") || param.starts_with("set") {
            // Skip Slurm-style "where" and "set" keywords
            continue;
        } else {
            // Try next param as value
            let key = param.to_lowercase();
            if let Some(value) = iter.next() {
                map.insert(key, value.clone());
            }
        }
    }
    map
}

const QOS_KEYS: &[&str] = &[
    "name",
    "qos",
    "description",
    "priority",
    "preemptmode",
    "usagefactor",
    "maxjobsperuser",
    "maxjobspu",
    "maxwall",
    "maxtresperjob",
    "maxsubmitjobsperuser",
    "maxtresperuser",
    "grptres",
    "grpwall",
];

/// Reject keys the command does not understand, so a mistyped or unsupported
/// field errors loudly instead of being silently dropped (a dropped limit
/// reads as "set" but never enforces).
fn reject_unknown_keys(
    p: &std::collections::HashMap<String, String>,
    allowed: &[&str],
) -> Result<()> {
    if let Some(key) = p.keys().find(|k| !allowed.contains(&k.as_str())) {
        bail!(
            "sacctmgr: unknown field '{key}'. Supported: {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

async fn connect(addr: &str) -> Result<SlurmAccountingClient<tonic::transport::Channel>> {
    let channel = spur_client::connect_channel(addr)
        .await
        .context("failed to connect to controller")?;
    Ok(spur_proto::accounting_client(channel))
}

/// Parse a numeric limit value where `0` is a keyword meaning "no limit".
/// Fails loudly instead of silently defaulting to `0` so a typo or
/// out-of-range value never accidentally lifts a limit.
fn parse_limit(key: &str, val: &str) -> Result<u32> {
    val.parse()
        .map_err(|_| anyhow::anyhow!("invalid value for {key}=: '{val}'"))
}

/// Same as `parse_limit`, but for wall-time fields that also accept Slurm's
/// `d-hh:mm:ss`/`hh:mm:ss` duration syntax (see `parse_wall_time`).
fn parse_wall_limit(key: &str, val: &str) -> Result<u32> {
    parse_wall_time(val).ok_or_else(|| anyhow::anyhow!("invalid value for {key}=: '{val}'"))
}

/// Fields shared by `add user` and `modify user` (both upsert via the same
/// `AddUserRequest`).
#[derive(Debug, PartialEq)]
struct UserUpsertFields {
    name: String,
    account: String,
    admin: String,
    default_qos: String,
    max_running_jobs: u32,
    max_submit_jobs: u32,
    max_tres_per_job: String,
    grp_tres: String,
    max_wall_minutes: u32,
}

/// Parse the key=value params for `add user`/`modify user` into the shared
/// upsert shape. Numeric/TRES aliases mirror `add account`'s
/// `maxrunningjobs`/`maxjobs` and `add qos`'s `maxwall` for consistency
/// across entities.
fn build_add_user_request(
    p: &std::collections::HashMap<String, String>,
) -> Result<UserUpsertFields> {
    let name = p
        .get("name")
        .or_else(|| p.get("user"))
        .ok_or_else(|| anyhow::anyhow!("name= required"))?
        .clone();
    let account = p
        .get("account")
        .or_else(|| p.get("defaultaccount"))
        .ok_or_else(|| anyhow::anyhow!("account= required"))?
        .clone();
    let admin = p
        .get("adminlevel")
        .cloned()
        .unwrap_or_else(|| "none".into());
    let default_qos = p.get("defaultqos").cloned().unwrap_or_default();
    let max_running_jobs: u32 = p
        .get("maxrunningjobs")
        .or_else(|| p.get("maxjobs"))
        .map(|v| parse_limit("maxjobs", v))
        .transpose()?
        .unwrap_or(0);
    let max_submit_jobs: u32 = p
        .get("maxsubmitjobs")
        .map(|v| parse_limit("maxsubmitjobs", v))
        .transpose()?
        .unwrap_or(0);
    let max_tres_per_job = p.get("maxtresperjob").cloned().unwrap_or_default();
    let grp_tres = p.get("grptres").cloned().unwrap_or_default();
    let max_wall_minutes: u32 = p
        .get("maxwall")
        .or_else(|| p.get("maxwallduration"))
        .map(|v| parse_wall_limit("maxwall", v))
        .transpose()?
        .unwrap_or(0);

    Ok(UserUpsertFields {
        name,
        account,
        admin,
        default_qos,
        max_running_jobs,
        max_submit_jobs,
        max_tres_per_job,
        grp_tres,
        max_wall_minutes,
    })
}

fn list_user_filters(p: &std::collections::HashMap<String, String>) -> (String, String) {
    (
        p.get("account").cloned().unwrap_or_default(),
        p.get("name")
            .or_else(|| p.get("user"))
            .cloned()
            .unwrap_or_default(),
    )
}

async fn add(entity: &str, params: &[String], addr: &str) -> Result<()> {
    let p = parse_params(params);

    match entity.to_lowercase().as_str() {
        "account" => {
            let name = p
                .get("name")
                .or_else(|| p.get("account"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;
            let desc = p.get("description").cloned().unwrap_or_default();
            let org = p.get("organization").cloned().unwrap_or_default();
            let parent = p.get("parent").cloned().unwrap_or_default();
            let fairshare: f64 = p
                .get("fairshare")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0);
            let max_jobs: u32 = p
                .get("maxrunningjobs")
                .or_else(|| p.get("maxjobs"))
                .map(|v| parse_limit("maxjobs", v))
                .transpose()?
                .unwrap_or(0);

            let mut client = connect(addr).await?;
            client
                .create_account(CreateAccountRequest {
                    name: name.clone(),
                    description: desc.clone(),
                    organization: org.clone(),
                    parent_account: parent.clone(),
                    fairshare_weight: fairshare,
                    max_running_jobs: max_jobs,
                })
                .await
                .context("CreateAccount RPC failed")?;

            println!(
                " Adding Account(s)\n  Name       = {}\n  Descr      = {}\n  Org        = {}\n  Parent     = {}\n  Fairshare  = {}",
                name,
                desc,
                org,
                if parent.is_empty() { "root" } else { &parent },
                fairshare
            );
            println!(" Account added.");
            Ok(())
        }
        "user" => {
            let fields = build_add_user_request(&p)?;

            let mut client = connect(addr).await?;
            client
                .add_user(AddUserRequest {
                    user: fields.name.clone(),
                    account: fields.account.clone(),
                    admin_level: fields.admin.clone(),
                    is_default: p
                        .get("defaultaccount")
                        .map(|da| da == &fields.account)
                        .unwrap_or(true),
                    default_qos: fields.default_qos.clone(),
                    max_running_jobs: fields.max_running_jobs,
                    max_submit_jobs: fields.max_submit_jobs,
                    max_tres_per_job: fields.max_tres_per_job.clone(),
                    grp_tres: fields.grp_tres.clone(),
                    max_wall_minutes: fields.max_wall_minutes,
                })
                .await
                .context("AddUser RPC failed")?;

            println!(
                " Adding User(s)\n  Name       = {}\n  Account    = {}\n  Admin      = {}",
                fields.name, fields.account, fields.admin
            );
            if !fields.default_qos.is_empty() {
                println!("  DefQOS     = {}", fields.default_qos);
            }
            if fields.max_running_jobs > 0 {
                println!("  MaxJobs    = {}", fields.max_running_jobs);
            }
            if fields.max_submit_jobs > 0 {
                println!("  MaxSubmit  = {}", fields.max_submit_jobs);
            }
            if fields.max_wall_minutes > 0 {
                println!("  MaxWall    = {} min", fields.max_wall_minutes);
            }
            if !fields.max_tres_per_job.is_empty() {
                println!("  MaxTRES    = {}", fields.max_tres_per_job);
            }
            if !fields.grp_tres.is_empty() {
                println!("  GrpTRES    = {}", fields.grp_tres);
            }
            println!(" User added.");
            Ok(())
        }
        "qos" => {
            reject_unknown_keys(&p, QOS_KEYS)?;
            let name = p
                .get("name")
                .or_else(|| p.get("qos"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;
            let desc = p.get("description").cloned().unwrap_or_default();
            let priority: i32 = p.get("priority").and_then(|v| v.parse().ok()).unwrap_or(0);
            let preempt = p
                .get("preemptmode")
                .cloned()
                .unwrap_or_else(|| "off".into());
            let usage_factor: f64 = p
                .get("usagefactor")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0);
            let max_jobs: u32 = p
                .get("maxjobsperuser")
                .or_else(|| p.get("maxjobspu"))
                .map(|v| parse_limit("maxjobsperuser", v))
                .transpose()?
                .unwrap_or(0);
            let max_wall: u32 = p
                .get("maxwall")
                .map(|v| parse_wall_limit("maxwall", v))
                .transpose()?
                .unwrap_or(0);
            let max_tres = p.get("maxtresperjob").cloned().unwrap_or_default();
            let grp_wall: u32 = p
                .get("grpwall")
                .map(|v| parse_wall_limit("grpwall", v))
                .transpose()?
                .unwrap_or(0);

            let mut client = connect(addr).await?;
            client
                .create_qos(CreateQosRequest {
                    name: name.clone(),
                    description: desc,
                    priority,
                    preempt_mode: preempt.clone(),
                    usage_factor,
                    max_jobs_per_user: max_jobs,
                    max_wall_minutes: max_wall,
                    max_tres_per_job: max_tres,
                    max_submit_jobs_per_user: p
                        .get("maxsubmitjobsperuser")
                        .map(|v| parse_limit("maxsubmitjobsperuser", v))
                        .transpose()?
                        .unwrap_or(0),
                    max_tres_per_user: p.get("maxtresperuser").cloned().unwrap_or_default(),
                    grp_tres: p.get("grptres").cloned().unwrap_or_default(),
                    grp_wall_minutes: grp_wall,
                })
                .await
                .context("CreateQos RPC failed")?;

            println!(
                " Adding QOS(s)\n  Name       = {}\n  Priority   = {}\n  Preempt    = {}",
                name, priority, preempt
            );
            if max_wall > 0 {
                println!("  MaxWall    = {} min", max_wall);
            }
            if max_jobs > 0 {
                println!("  MaxJobsPU  = {}", max_jobs);
            }
            println!(" QOS added.");
            Ok(())
        }
        other => bail!(
            "sacctmgr: unknown entity type '{}'. Use: account, user, qos",
            other
        ),
    }
}

async fn delete(entity: &str, params: &[String], addr: &str) -> Result<()> {
    let p = parse_params(params);

    match entity.to_lowercase().as_str() {
        "account" => {
            let name = p
                .get("name")
                .or_else(|| p.get("account"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;

            let mut client = connect(addr).await?;
            client
                .delete_account(DeleteAccountRequest { name: name.clone() })
                .await
                .context("DeleteAccount RPC failed")?;

            println!(" Deleting account: {}", name);
            println!(" Done.");
            Ok(())
        }
        "user" => {
            let name = p
                .get("name")
                .or_else(|| p.get("user"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;
            let account = p.get("account").cloned().unwrap_or_default();

            let mut client = connect(addr).await?;
            client
                .remove_user(RemoveUserRequest {
                    user: name.clone(),
                    account: account.clone(),
                })
                .await
                .context("RemoveUser RPC failed")?;

            let acct_display = if account.is_empty() { "all" } else { &account };
            println!(" Deleting user {} from account {}", name, acct_display);
            println!(" Done.");
            Ok(())
        }
        "qos" => {
            let name = p
                .get("name")
                .or_else(|| p.get("qos"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;

            let mut client = connect(addr).await?;
            client
                .delete_qos(DeleteQosRequest { name: name.clone() })
                .await
                .context("DeleteQos RPC failed")?;

            println!(" Deleting QOS: {}", name);
            println!(" Done.");
            Ok(())
        }
        other => bail!("sacctmgr: unknown entity type '{}'", other),
    }
}

async fn modify(entity: &str, params: &[String], addr: &str) -> Result<()> {
    let p = parse_params(params);

    // Modify is an upsert — same RPCs as add, just re-sends the record
    match entity.to_lowercase().as_str() {
        "account" => {
            let name = p
                .get("name")
                .or_else(|| p.get("account"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;

            let mut client = connect(addr).await?;
            client
                .create_account(CreateAccountRequest {
                    name: name.clone(),
                    description: p.get("description").cloned().unwrap_or_default(),
                    organization: p.get("organization").cloned().unwrap_or_default(),
                    parent_account: p.get("parent").cloned().unwrap_or_default(),
                    fairshare_weight: p
                        .get("fairshare")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1.0),
                    max_running_jobs: p
                        .get("maxrunningjobs")
                        .or_else(|| p.get("maxjobs"))
                        .map(|v| parse_limit("maxjobs", v))
                        .transpose()?
                        .unwrap_or(0),
                })
                .await
                .context("CreateAccount (modify) RPC failed")?;

            println!(" Modified account '{}'.", name);
            Ok(())
        }
        "qos" => {
            reject_unknown_keys(&p, QOS_KEYS)?;
            let name = p
                .get("name")
                .or_else(|| p.get("qos"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;

            let mut client = connect(addr).await?;
            client
                .create_qos(CreateQosRequest {
                    name: name.clone(),
                    description: p.get("description").cloned().unwrap_or_default(),
                    priority: p.get("priority").and_then(|v| v.parse().ok()).unwrap_or(0),
                    preempt_mode: p
                        .get("preemptmode")
                        .cloned()
                        .unwrap_or_else(|| "off".into()),
                    usage_factor: p
                        .get("usagefactor")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1.0),
                    max_jobs_per_user: p
                        .get("maxjobsperuser")
                        .or_else(|| p.get("maxjobspu"))
                        .map(|v| parse_limit("maxjobsperuser", v))
                        .transpose()?
                        .unwrap_or(0),
                    max_wall_minutes: p
                        .get("maxwall")
                        .map(|v| parse_wall_limit("maxwall", v))
                        .transpose()?
                        .unwrap_or(0),
                    max_tres_per_job: p.get("maxtresperjob").cloned().unwrap_or_default(),
                    max_submit_jobs_per_user: p
                        .get("maxsubmitjobsperuser")
                        .map(|v| parse_limit("maxsubmitjobsperuser", v))
                        .transpose()?
                        .unwrap_or(0),
                    max_tres_per_user: p.get("maxtresperuser").cloned().unwrap_or_default(),
                    grp_tres: p.get("grptres").cloned().unwrap_or_default(),
                    grp_wall_minutes: p
                        .get("grpwall")
                        .map(|v| parse_wall_limit("grpwall", v))
                        .transpose()?
                        .unwrap_or(0),
                })
                .await
                .context("CreateQos (modify) RPC failed")?;

            println!(" Modified QOS '{}'.", name);
            Ok(())
        }
        "user" => {
            let fields = build_add_user_request(&p)?;

            let mut client = connect(addr).await?;
            client
                .add_user(AddUserRequest {
                    user: fields.name.clone(),
                    account: fields.account.clone(),
                    admin_level: fields.admin.clone(),
                    is_default: p
                        .get("defaultaccount")
                        .map(|da| da == &fields.account)
                        .unwrap_or(true),
                    default_qos: fields.default_qos.clone(),
                    max_running_jobs: fields.max_running_jobs,
                    max_submit_jobs: fields.max_submit_jobs,
                    max_tres_per_job: fields.max_tres_per_job.clone(),
                    grp_tres: fields.grp_tres.clone(),
                    max_wall_minutes: fields.max_wall_minutes,
                })
                .await
                .context("AddUser (modify) RPC failed")?;

            println!(" Modified user '{}'.", fields.name);
            if !fields.default_qos.is_empty() {
                println!("  DefQOS     = {}", fields.default_qos);
            }
            if fields.max_running_jobs > 0 {
                println!("  MaxJobs    = {}", fields.max_running_jobs);
            }
            if fields.max_submit_jobs > 0 {
                println!("  MaxSubmit  = {}", fields.max_submit_jobs);
            }
            if fields.max_wall_minutes > 0 {
                println!("  MaxWall    = {} min", fields.max_wall_minutes);
            }
            if !fields.max_tres_per_job.is_empty() {
                println!("  MaxTRES    = {}", fields.max_tres_per_job);
            }
            if !fields.grp_tres.is_empty() {
                println!("  GrpTRES    = {}", fields.grp_tres);
            }
            Ok(())
        }
        other => bail!("sacctmgr: unknown entity type '{}'", other),
    }
}

async fn show(entity: &str, params: &[String], addr: &str) -> Result<()> {
    let p = parse_params(params);

    match entity.to_lowercase().as_str() {
        "account" | "accounts" => {
            let mut client = connect(addr).await?;
            let resp = client
                .list_accounts(ListAccountsRequest {})
                .await
                .context("ListAccounts RPC failed")?;

            let accounts = resp.into_inner().accounts;

            println!(
                "{:<20} {:<30} {:<15} {:<10} {:<10}",
                "Account", "Descr", "Org", "Parent", "Share"
            );
            println!("{}", "-".repeat(85));

            if accounts.is_empty() {
                println!(
                    "{:<20} {:<30} {:<15} {:<10} {:<10}",
                    "(no accounts configured)", "", "", "", ""
                );
            } else {
                for a in &accounts {
                    let parent = if a.parent_account.is_empty() {
                        ""
                    } else {
                        &a.parent_account
                    };
                    println!(
                        "{:<20} {:<30} {:<15} {:<10} {:<10}",
                        a.name, a.description, a.organization, parent, a.fairshare_weight as u32,
                    );
                }
            }
            Ok(())
        }
        "user" | "users" => {
            let (account_filter, user_filter) = list_user_filters(&p);

            let mut client = connect(addr).await?;
            let resp = client
                .list_users(ListUsersRequest {
                    account: account_filter,
                    user: user_filter,
                })
                .await
                .context("ListUsers RPC failed")?;

            let users = resp.into_inner().users;

            println!(
                "{:<15} {:<20} {:<10} {:<20} {:<15}",
                "User", "Account", "Admin", "Default Acct", "Def QOS"
            );
            println!("{}", "-".repeat(80));

            for u in &users {
                println!(
                    "{:<15} {:<20} {:<10} {:<20} {:<15}",
                    u.name, u.account, u.admin_level, u.default_account, u.default_qos,
                );
            }
            Ok(())
        }
        "qos" => {
            let fields = format_engine::resolve_format(
                p.get("format").map(String::as_str),
                QOS_DEFAULT_FORMAT,
                QOS_ALL_FORMAT,
                &qos_field_spec,
                &qos_header,
                "Name, Description, Priority, Preempt, UsageFactor, \
                 GrpTRES, MaxTRES, MaxTRESPU, MaxJobsPU, MaxSubmitPU, MaxWall, GrpWall",
            )?;

            let mut client = connect(addr).await?;
            let mut qos_list = client
                .list_qos(ListQosRequest {})
                .await
                .context("ListQos RPC failed")?
                .into_inner()
                .qos_list;

            let has_name_filter = p.contains_key("name");
            if let Some(name_filter) = p.get("name") {
                filter_qos_by_name(&mut qos_list, name_filter);
            }

            format_engine::print_header(&fields);

            if qos_list.is_empty() && !has_name_filter {
                let default_qos = QosInfo {
                    name: "normal".into(),
                    preempt_mode: "off".into(),
                    usage_factor: 1.0,
                    ..Default::default()
                };
                println!(
                    "{}",
                    format_engine::format_row(&fields, &|spec| resolve_qos_field(
                        &default_qos,
                        spec
                    ))
                );
            } else {
                for q in &qos_list {
                    println!(
                        "{}",
                        format_engine::format_row(&fields, &|spec| resolve_qos_field(q, spec))
                    );
                }
            }
            Ok(())
        }
        "association" | "associations" => {
            println!(
                "{:<15} {:<20} {:<15} {:<10} {:<10}",
                "User", "Account", "Partition", "Share", "Default"
            );
            println!("{}", "-".repeat(70));
            Ok(())
        }
        "tres" => {
            println!("{:<5} {:<15} {:<10}", "ID", "Type", "Name");
            println!("{}", "-".repeat(30));
            println!("{:<5} {:<15} {:<10}", "1", "cpu", "");
            println!("{:<5} {:<15} {:<10}", "2", "mem", "");
            println!("{:<5} {:<15} {:<10}", "3", "energy", "");
            println!("{:<5} {:<15} {:<10}", "4", "node", "");
            println!("{:<5} {:<15} {:<10}", "1001", "gres/gpu", "");
            println!("{:<5} {:<15} {:<10}", "1002", "billing", "");
            Ok(())
        }
        other => bail!(
            "sacctmgr: unknown entity '{}'. Use: account, user, qos, association, tres",
            other
        ),
    }
}

fn filter_qos_by_name(qos_list: &mut Vec<QosInfo>, filter: &str) {
    let names: Vec<&str> = filter.split(',').map(str::trim).collect();
    qos_list.retain(|q| names.iter().any(|n| n.eq_ignore_ascii_case(&q.name)));
}

const QOS_DEFAULT_FORMAT: &str = "%-15N %-8p %-10P %-12U %-10J %-10S %-10W %-10w %-20T %-20V %-20G";

const QOS_ALL_FORMAT: &str =
    "%-15N %-30D %-8p %-10P %-12U %-10J %-10S %-10W %-10w %-20T %-20V %-20G";

fn qos_header(spec: char) -> &'static str {
    match spec {
        'N' => "Name",
        'D' => "Descr",
        'p' => "Priority",
        'P' => "Preempt",
        'U' => "UsageFactor",
        'G' => "GrpTRES",
        'T' => "MaxTRES",
        'V' => "MaxTRESPU",
        'J' => "MaxJobsPU",
        'S' => "MaxSubmitPU",
        'W' => "MaxWall",
        'w' => "GrpWall",
        _ => "?",
    }
}

fn qos_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "name" => Some('N'),
        "description" | "descr" => Some('D'),
        "priority" | "prio" => Some('p'),
        "preempt" | "preemptmode" => Some('P'),
        "usagefactor" => Some('U'),
        "grptres" => Some('G'),
        "maxtres" | "maxtrespj" | "maxtresperjob" => Some('T'),
        "maxtrespu" | "maxtresperuser" => Some('V'),
        "maxjobspu" | "maxjobsperuser" => Some('J'),
        "maxsubmitpu" | "maxsubmitjobspu" | "maxsubmitjobsperuser" => Some('S'),
        "maxwall" | "maxwalldurationperjob" => Some('W'),
        "grpwall" => Some('w'),
        _ => None,
    }
}

fn resolve_qos_field(q: &QosInfo, spec: char) -> String {
    match spec {
        'N' => q.name.clone(),
        'D' => q.description.clone(),
        'p' => q.priority.to_string(),
        'P' => q.preempt_mode.clone(),
        'U' => format!("{}", q.usage_factor),
        'G' => q.grp_tres.clone(),
        'T' => q.max_tres_per_job.clone(),
        'V' => q.max_tres_per_user.clone(),
        'J' => {
            if q.max_jobs_per_user == 0 {
                String::new()
            } else {
                q.max_jobs_per_user.to_string()
            }
        }
        'S' => {
            if q.max_submit_jobs_per_user == 0 {
                String::new()
            } else {
                q.max_submit_jobs_per_user.to_string()
            }
        }
        'W' => {
            if q.max_wall_minutes == 0 {
                String::new()
            } else {
                q.max_wall_minutes.to_string()
            }
        }
        'w' => {
            if q.grp_wall_minutes == 0 {
                String::new()
            } else {
                q.grp_wall_minutes.to_string()
            }
        }
        _ => "?".into(),
    }
}

/// Parse wall time strings like "60" (minutes), "1:00:00" (h:m:s), "1-00:00:00" (d-h:m:s)
/// Returns total minutes.
fn parse_wall_time(s: &str) -> Option<u32> {
    // Try plain integer (minutes)
    if let Ok(mins) = s.parse::<u32>() {
        return Some(mins);
    }

    // Try d-hh:mm:ss
    if let Some((days_str, rest)) = s.split_once('-') {
        let days: u32 = days_str.parse().ok()?;
        let parts: Vec<&str> = rest.split(':').collect();
        let (h, m) = match parts.len() {
            2 => (parts[0].parse::<u32>().ok()?, parts[1].parse::<u32>().ok()?),
            3 => (parts[0].parse::<u32>().ok()?, parts[1].parse::<u32>().ok()?),
            _ => return None,
        };
        return Some(days * 24 * 60 + h * 60 + m);
    }

    // Try hh:mm:ss or hh:mm
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            let h: u32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            Some(h * 60 + m)
        }
        3 => {
            let h: u32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            Some(h * 60 + m)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_add_user_request_parses_defaultqos() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "defaultqos=highprio".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.name, "testuser");
        assert_eq!(fields.account, "testacct");
        assert_eq!(fields.admin, "none");
        assert_eq!(fields.default_qos, "highprio");
    }

    #[test]
    fn reject_unknown_keys_flags_dropped_field() {
        // A field the command doesn't read must error, not be silently dropped
        // (a dropped limit reads as "set" but never enforces).
        let p = parse_params(&["name=normal".into(), "bogusfield=1".into()]);
        let err = reject_unknown_keys(&p, QOS_KEYS).unwrap_err();
        assert!(err.to_string().contains("unknown field 'bogusfield'"));
    }

    #[test]
    fn reject_unknown_keys_accepts_every_parsed_qos_field() {
        // Every input key the add/modify qos handlers read must be in the
        // allowlist, otherwise reject_unknown_keys bounces it before parsing
        // (grpwall regressed this way: parsed and shown, but not allowlisted).
        let parsed_fields = [
            "name",
            "description",
            "priority",
            "preemptmode",
            "usagefactor",
            "maxjobsperuser",
            "maxwall",
            "maxtresperjob",
            "maxsubmitjobsperuser",
            "maxtresperuser",
            "grptres",
            "grpwall",
        ];
        for field in parsed_fields {
            let p = parse_params(&["name=normal".into(), format!("{field}=1")]);
            assert!(
                reject_unknown_keys(&p, QOS_KEYS).is_ok(),
                "QOS field '{field}' is read by the handler but missing from QOS_KEYS"
            );
        }
    }

    #[test]
    fn reject_unknown_keys_accepts_known_and_alias() {
        // maxjobspu is the label `sacctmgr show qos` prints, so it must be a
        // valid input alias for maxjobsperuser.
        let p = parse_params(&["name=normal".into(), "maxjobspu=5".into()]);
        assert!(reject_unknown_keys(&p, QOS_KEYS).is_ok());
        assert_eq!(
            p.get("maxjobsperuser").or_else(|| p.get("maxjobspu")),
            Some(&"5".to_string())
        );
    }

    #[test]
    fn build_add_user_request_defaultqos_absent_is_empty() {
        let p = parse_params(&["name=testuser".into(), "account=testacct".into()]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.default_qos, "");
    }

    #[test]
    fn build_add_user_request_parses_account_limits() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxjobs=2".into(),
            "maxsubmitjobs=4".into(),
            "maxtresperjob=cpu=8".into(),
            "grptres=cpu=32".into(),
            "maxwall=60".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_running_jobs, 2);
        assert_eq!(fields.max_submit_jobs, 4);
        assert_eq!(fields.max_tres_per_job, "cpu=8");
        assert_eq!(fields.grp_tres, "cpu=32");
        assert_eq!(fields.max_wall_minutes, 60);
    }

    #[test]
    fn build_add_user_request_maxrunningjobs_alias() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxrunningjobs=3".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_running_jobs, 3);
    }

    #[test]
    fn build_add_user_request_maxwallduration_alias() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxwallduration=1:30".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_wall_minutes, 90);
    }

    #[test]
    fn build_add_user_request_account_limits_absent_are_zero() {
        let p = parse_params(&["name=testuser".into(), "account=testacct".into()]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_running_jobs, 0);
        assert_eq!(fields.max_submit_jobs, 0);
        assert_eq!(fields.max_tres_per_job, "");
        assert_eq!(fields.grp_tres, "");
        assert_eq!(fields.max_wall_minutes, 0);
    }

    #[test]
    fn build_add_user_request_missing_name_errors() {
        let p = parse_params(&["account=testacct".into()]);
        assert!(build_add_user_request(&p).is_err());
    }

    #[test]
    fn build_add_user_request_missing_account_errors() {
        let p = parse_params(&["name=testuser".into()]);
        assert!(build_add_user_request(&p).is_err());
    }

    #[test]
    fn parse_limit_rejects_non_numeric_value() {
        let err = parse_limit("maxjobs", "abc").unwrap_err();
        assert!(err.to_string().contains("maxjobs"));
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn parse_limit_rejects_negative_value() {
        assert!(parse_limit("maxjobs", "-5").is_err());
    }

    #[test]
    fn parse_limit_rejects_overflowing_value() {
        assert!(parse_limit("maxjobs", "99999999999999999999").is_err());
    }

    #[test]
    fn parse_limit_accepts_valid_value() {
        assert_eq!(parse_limit("maxjobs", "5").unwrap(), 5);
    }

    #[test]
    fn parse_wall_limit_rejects_non_numeric_value() {
        assert!(parse_wall_limit("maxwall", "abc").is_err());
    }

    #[test]
    fn parse_wall_limit_accepts_duration_syntax() {
        assert_eq!(parse_wall_limit("maxwall", "1:30").unwrap(), 90);
    }

    #[test]
    fn build_add_user_request_rejects_invalid_maxjobs() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxjobs=abc".into(),
        ]);
        let err = build_add_user_request(&p).unwrap_err();
        assert!(err.to_string().contains("maxjobs"));
    }

    #[test]
    fn build_add_user_request_rejects_negative_maxsubmitjobs() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxsubmitjobs=-1".into(),
        ]);
        assert!(build_add_user_request(&p).is_err());
    }

    #[test]
    fn build_add_user_request_rejects_invalid_maxwall() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxwall=notatime".into(),
        ]);
        assert!(build_add_user_request(&p).is_err());
    }

    #[test]
    fn build_add_user_request_add_and_modify_produce_the_same_shape() {
        // add and modify both funnel through the same helper, so `sacctmgr
        // add user name=X account=Y defaultqos=Z` and `sacctmgr modify user
        // name=X account=Y set defaultqos=Z` build identical requests.
        let add_params = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "defaultqos=highprio".into(),
        ]);
        let modify_params = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "set".into(),
            "defaultqos=highprio".into(),
        ]);
        assert_eq!(
            build_add_user_request(&add_params).unwrap(),
            build_add_user_request(&modify_params).unwrap()
        );
    }

    fn stub_qos() -> QosInfo {
        QosInfo {
            name: "gpuqos".into(),
            description: "GPU workers".into(),
            priority: 100,
            preempt_mode: "cancel".into(),
            usage_factor: 1.5,
            max_jobs_per_user: 8,
            max_submit_jobs_per_user: 20,
            max_wall_minutes: 120,
            max_tres_per_job: "node=2,cpu=64".into(),
            max_tres_per_user: "cpu=128".into(),
            grp_tres: "node=4,cpu=256".into(),
            ..Default::default()
        }
    }

    #[test]
    fn qos_named_format_renders_tres_fields() {
        let fields = format_engine::parse_named_format(
            "Name,GrpTRES,MaxTRES,MaxTRESPU",
            &qos_field_spec,
            &qos_header,
        );
        let q = stub_qos();
        let row = format_engine::format_row(&fields, &|spec| resolve_qos_field(&q, spec));
        assert!(row.contains("node=4,cpu=256"), "GrpTRES missing: {row}");
        assert!(row.contains("node=2,cpu=64"), "MaxTRES missing: {row}");
        assert!(row.contains("cpu=128"), "MaxTRESPU missing: {row}");
    }

    #[test]
    fn qos_field_spec_aliases_are_case_insensitive() {
        assert_eq!(qos_field_spec("grptres"), qos_field_spec("GrpTRES"));
        assert_eq!(qos_field_spec("maxtres"), qos_field_spec("MaxTRESPJ"));
        assert_eq!(qos_field_spec("maxtresperjob"), qos_field_spec("MaxTRES"));
        assert_eq!(
            qos_field_spec("maxwall"),
            qos_field_spec("MaxWallDurationPerJob")
        );
    }

    #[test]
    fn qos_default_format_includes_tres_columns() {
        let fields = format_engine::parse_format(QOS_DEFAULT_FORMAT, &qos_header);
        let header = format_engine::format_header(&fields);
        assert!(header.contains("GrpTRES"), "default header: {header}");
        assert!(header.contains("MaxTRES"), "default header: {header}");
    }

    #[test]
    fn qos_all_format_includes_description_and_submit() {
        let fields = format_engine::parse_format(QOS_ALL_FORMAT, &qos_header);
        let header = format_engine::format_header(&fields);
        assert!(header.contains("Descr"), "all header: {header}");
        assert!(header.contains("MaxSubmitPU"), "all header: {header}");
    }

    #[test]
    fn qos_resolve_zero_fields_are_blank() {
        let q = QosInfo {
            name: "normal".into(),
            preempt_mode: "off".into(),
            usage_factor: 1.0,
            ..Default::default()
        };
        assert_eq!(resolve_qos_field(&q, 'J'), "");
        assert_eq!(resolve_qos_field(&q, 'S'), "");
        assert_eq!(resolve_qos_field(&q, 'W'), "");
        assert_eq!(resolve_qos_field(&q, 'w'), "");
    }

    #[test]
    fn qos_name_filter_retains_matching_and_trims_whitespace() {
        let mut list = vec![
            QosInfo {
                name: "alpha".into(),
                ..Default::default()
            },
            QosInfo {
                name: "beta".into(),
                ..Default::default()
            },
            QosInfo {
                name: "gamma".into(),
                ..Default::default()
            },
        ];
        filter_qos_by_name(&mut list, "alpha, gamma");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "gamma");
    }

    #[test]
    fn qos_field_spec_rejects_unknown_names() {
        assert_eq!(qos_field_spec("bogus"), None);
        assert_eq!(qos_field_spec("nodelist"), None);
        assert_eq!(qos_field_spec(""), None);
    }

    #[test]
    fn qos_empty_format_string_produces_no_fields() {
        let fields = format_engine::parse_named_format("", &qos_field_spec, &qos_header);
        assert!(fields.is_empty());
    }

    #[test]
    fn list_user_filters_include_name() {
        let p = parse_params(&["name=testuser".into(), "account=testacct".into()]);

        let (account, user) = list_user_filters(&p);

        assert_eq!(account, "testacct");
        assert_eq!(user, "testuser");
    }
}
