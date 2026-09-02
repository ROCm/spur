// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::format_engine;
use crate::timearg::{datetime_to_proto, parse_time_arg};
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

    /// Omit the header line
    #[arg(short = 'n', long, global = true)]
    pub noheader: bool,

    /// Output '|' delimited with a trailing '|'
    #[arg(short = 'p', long, global = true)]
    pub parsable: bool,

    /// Output '|' delimited without a trailing '|'
    #[arg(short = 'P', long, global = true)]
    pub parsable2: bool,
}

impl SacctmgrArgs {
    /// Slurm takes whichever delimiter flag came last; clap's derive cannot see flag order,
    /// so `-P` wins when both are given rather than rejecting input Slurm accepts.
    fn output_style(&self) -> format_engine::OutputStyle {
        let layout = if self.parsable2 {
            format_engine::RowLayout::Delimited
        } else if self.parsable {
            format_engine::RowLayout::DelimitedTrailing
        } else {
            format_engine::RowLayout::Aligned
        };

        format_engine::OutputStyle::new(self.noheader, layout)
    }
}

#[derive(Subcommand, Debug)]
pub enum SacctmgrCommand {
    /// Add entities
    Add {
        /// Entity type: account, user, qos
        entity: String,
        /// Optional `key=value` pairs; a global flag may sit anywhere among them.
        params: Vec<String>,
    },
    /// Delete entities
    Delete {
        /// Entity type: account, user, qos
        entity: String,
        /// Optional `key=value` pairs (name= or where clause); a global flag may sit anywhere among them.
        params: Vec<String>,
    },
    /// Modify entities
    Modify {
        /// Entity type: account, user, qos
        entity: String,
        /// Optional `key=value` pairs; a global flag may sit anywhere among them.
        params: Vec<String>,
    },
    /// List/show entities
    Show {
        /// Entity type: account, user, qos, association
        entity: String,
        /// Optional `key=value` filters; a global flag may sit anywhere among them.
        params: Vec<String>,
    },
    /// List entities (alias for show)
    List { entity: String, params: Vec<String> },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SacctmgrArgs::try_parse_from(&args)?;
    let addr = args.controller.clone();
    let style = args.output_style();

    match args.command {
        SacctmgrCommand::Add { entity, params } => add(&entity, &params, &addr).await,
        SacctmgrCommand::Delete { entity, params } => delete(&entity, &params, &addr).await,
        SacctmgrCommand::Modify { entity, params } => modify(&entity, &params, &addr).await,
        SacctmgrCommand::Show { entity, params } | SacctmgrCommand::List { entity, params } => {
            show(&entity, &params, &addr, style).await
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
    "preempt",
    "preemptexempttime",
    "clearpreemptexempttime",
    "usagefactor",
    "maxjobsperuser",
    "maxjobspu",
    "maxwall",
    "maxtresperjob",
    "maxsubmitjobsperuser",
    "maxsubmitjobsperaccount",
    "maxsubmitpa",
    "maxsubmitjobspa",
    "grpsubmit",
    "grpsubmitjobs",
    "maxtresperuser",
    "grptres",
    "grpwall",
    "flags",
];

/// Input keys the `add`/`modify user` handlers read (names + aliases). Gates
/// mistyped fields the same way `QOS_KEYS`/`ACCOUNT_KEYS` do.
const USER_KEYS: &[&str] = &[
    "name",
    "user",
    "account",
    "defaultaccount",
    "adminlevel",
    "defaultqos",
    "qos",
    "maxrunningjobs",
    "maxjobs",
    "maxsubmitjobs",
    "grpsubmit",
    "grpsubmitjobs",
    "maxtresperjob",
    "grptres",
    "maxwall",
    "maxwallduration",
];

/// Input keys the `add`/`modify account` handlers read (names + aliases). Gates
/// mistyped fields the same way `QOS_KEYS` does, so an unsupported field errors
/// loudly instead of being silently dropped.
const ACCOUNT_KEYS: &[&str] = &[
    "name",
    "account",
    "description",
    "organization",
    "parent",
    "fairshare",
    "maxrunningjobs",
    "maxjobs",
    "grptres",
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

async fn connect(addr: &str) -> Result<SlurmAccountingClient<crate::authclient::AuthChannel>> {
    let channel = crate::authclient::connect(addr)
        .await
        .context("failed to connect to controller")?;
    Ok(spur_proto::accounting_client(channel))
}

/// Parse a numeric limit value. `-1` is Slurm's keyword for "no limit" and maps
/// to the INFINITE sentinel (clears the stored limit); `0` is a literal value
/// meaning "block all". Any other negative, or a value at/above INFINITE, is
/// rejected. Fails loudly instead of silently defaulting so a typo never
/// accidentally lifts or sets a limit.
fn parse_limit(key: &str, val: &str) -> Result<u32> {
    let n: i64 = val
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid value for {key}=: '{val}'"))?;
    if n == -1 {
        return Ok(spur_core::accounting::INFINITE);
    }
    if n < 0 || n >= spur_core::accounting::INFINITE as i64 {
        bail!("invalid value for {key}=: '{val}'");
    }
    Ok(n as u32)
}

/// Same as `parse_limit`, but for wall-time fields that also accept Slurm's
/// `d-hh:mm:ss`/`hh:mm:ss` duration syntax (see `parse_wall_time`). `-1` clears
/// the limit (INFINITE sentinel).
fn parse_wall_limit(key: &str, val: &str) -> Result<u32> {
    if val == "-1" {
        return Ok(spur_core::accounting::INFINITE);
    }
    let minutes =
        parse_wall_time(val).ok_or_else(|| anyhow::anyhow!("invalid value for {key}=: '{val}'"))?;
    if minutes == spur_core::accounting::INFINITE {
        bail!("invalid value for {key}=: '{val}'");
    }
    Ok(minutes)
}

/// Parse a floating-point field (e.g. `fairshare=`/`usagefactor=`), failing
/// loudly instead of silently defaulting so a typo never quietly changes a
/// weight on a partial `modify`.
fn parse_f64(key: &str, val: &str) -> Result<f64> {
    val.parse()
        .map_err(|_| anyhow::anyhow!("invalid value for {key}=: '{val}'"))
}

/// Parse a signed integer field (e.g. `priority=`), failing loudly rather than
/// silently defaulting, for the same reason as `parse_f64`.
fn parse_i32(key: &str, val: &str) -> Result<i32> {
    val.parse()
        .map_err(|_| anyhow::anyhow!("invalid value for {key}=: '{val}'"))
}

/// Parse an unsigned integer field, failing loudly rather than silently
/// defaulting so a typo never quietly changes a limit on a partial `modify`.
fn parse_u32(key: &str, val: &str) -> Result<u32> {
    val.parse()
        .map_err(|_| anyhow::anyhow!("invalid value for {key}=: '{val}'"))
}

/// Return true when a key=value pair explicitly opts in to a boolean action
/// (value is "1", "yes", or "true", case-insensitive). Bare keys (no `=value`)
/// never match because `parse_params` maps them to an empty string.
fn is_truthy(val: &str) -> bool {
    matches!(val.to_lowercase().as_str(), "1" | "yes" | "true")
}

/// Look up a field by its accepted aliases in priority order, returning the key
/// the caller actually wrote alongside its value. Parsers use the returned key
/// so an invalid-value error names the exact parameter the user passed (e.g.
/// `maxrunningjobs`) rather than a canonical alias (`maxjobs`).
fn find_alias<'a>(
    p: &'a std::collections::HashMap<String, String>,
    keys: &[&'a str],
) -> Option<(&'a str, &'a str)> {
    keys.iter()
        .copied()
        .find_map(|k| p.get(k).map(|v| (k, v.as_str())))
}

/// Fields shared by `add user` and `modify user` (both upsert via the same
/// `AddUserRequest`).
#[derive(Debug, PartialEq)]
struct UserUpsertFields {
    name: String,
    account: String,
    admin: String,
    default_qos: String,
    allowed_qos: String,
    max_running_jobs: u32,
    max_submit_jobs: u32,
    grp_submit_jobs: u32,
    max_tres_per_job: String,
    grp_tres: String,
    max_wall_minutes: u32,
}

/// Resolve the association account for `add`/`modify user`. `account=` picks the
/// association to write and `defaultaccount=` marks the user's default; both map
/// to one `(account, is_default)` pair, so two different accounts are rejected —
/// honoring it would modify one association while silently clearing the default.
fn resolve_user_account(p: &std::collections::HashMap<String, String>) -> Result<String> {
    let account = match (p.get("account"), p.get("defaultaccount")) {
        (Some(a), Some(d)) if a != d => bail!(
            "account={a} and defaultaccount={d} name different accounts; \
             set the default with defaultaccount= alone or a matching account="
        ),
        (Some(a), _) => a,
        (None, Some(d)) => d,
        (None, None) => bail!("account= required"),
    };
    if account.is_empty() {
        bail!("account= must not be empty");
    }
    Ok(account.clone())
}

/// Parse the key=value params for `add user`/`modify user` into the shared
/// upsert shape. Numeric/TRES aliases mirror `add account`'s
/// `maxrunningjobs`/`maxjobs` and `add qos`'s `maxwall` for consistency
/// across entities.
fn build_add_user_request(
    p: &std::collections::HashMap<String, String>,
) -> Result<UserUpsertFields> {
    reject_unknown_keys(p, USER_KEYS)?;
    let name = p
        .get("name")
        .or_else(|| p.get("user"))
        .ok_or_else(|| anyhow::anyhow!("name= required"))?
        .clone();
    let account = resolve_user_account(p)?;
    let admin = p
        .get("adminlevel")
        .cloned()
        .unwrap_or_else(|| "none".into());
    let default_qos = p.get("defaultqos").cloned().unwrap_or_default();
    let allowed_qos = p.get("qos").cloned().unwrap_or_default();
    if !default_qos.is_empty()
        && !allowed_qos.is_empty()
        && !allowed_qos
            .split(',')
            .map(str::trim)
            .any(|q| q == default_qos)
    {
        bail!("defaultqos={default_qos} must be included in qos={allowed_qos}");
    }
    // An unset numeric limit sends INFINITE (clear/no-limit) on `add`; a literal
    // 0 would mean "block all" under the sentinel semantics.
    let no_limit = spur_core::accounting::INFINITE;
    let max_running_jobs: u32 = find_alias(p, &["maxrunningjobs", "maxjobs"])
        .map(|(k, v)| parse_limit(k, v))
        .transpose()?
        .unwrap_or(no_limit);
    let max_submit_jobs: u32 = p
        .get("maxsubmitjobs")
        .map(|v| parse_limit("maxsubmitjobs", v))
        .transpose()?
        .unwrap_or(no_limit);
    let grp_submit_jobs: u32 = find_alias(p, &["grpsubmit", "grpsubmitjobs"])
        .map(|(k, v)| parse_limit(k, v))
        .transpose()?
        .unwrap_or(no_limit);
    let max_tres_per_job = p.get("maxtresperjob").cloned().unwrap_or_default();
    let grp_tres = p.get("grptres").cloned().unwrap_or_default();
    let max_wall_minutes: u32 = find_alias(p, &["maxwall", "maxwallduration"])
        .map(|(k, v)| parse_wall_limit(k, v))
        .transpose()?
        .unwrap_or(no_limit);

    Ok(UserUpsertFields {
        name,
        account,
        admin,
        default_qos,
        allowed_qos,
        max_running_jobs,
        max_submit_jobs,
        grp_submit_jobs,
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
            reject_unknown_keys(&p, ACCOUNT_KEYS)?;
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
            let max_jobs: u32 = find_alias(&p, &["maxrunningjobs", "maxjobs"])
                .map(|(k, v)| parse_limit(k, v))
                .transpose()?
                .unwrap_or(spur_core::accounting::INFINITE);
            let grp_tres = p.get("grptres").cloned().unwrap_or_default();

            let mut client = connect(addr).await?;
            client
                .create_account(CreateAccountRequest {
                    name: name.clone(),
                    description: Some(desc.clone()),
                    organization: Some(org.clone()),
                    parent_account: Some(parent.clone()),
                    fairshare_weight: Some(fairshare),
                    max_running_jobs: Some(max_jobs),
                    grp_tres: Some(grp_tres),
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
                    admin_level: Some(fields.admin.clone()),
                    // Like `modify`, send None when defaultaccount= is absent so a
                    // plain add can't silently demote a default the user already has.
                    is_default: p.get("defaultaccount").map(|da| da == &fields.account),
                    default_qos: Some(fields.default_qos.clone()),
                    allowed_qos: Some(fields.allowed_qos.clone()),
                    max_running_jobs: Some(fields.max_running_jobs),
                    max_submit_jobs: Some(fields.max_submit_jobs),
                    grp_submit_jobs: Some(fields.grp_submit_jobs),
                    max_tres_per_job: Some(fields.max_tres_per_job.clone()),
                    grp_tres: Some(fields.grp_tres.clone()),
                    max_wall_minutes: Some(fields.max_wall_minutes),
                })
                .await
                .context("AddUser RPC failed")?;

            println!(
                " Adding User(s)\n  Name       = {}\n  Account    = {}\n  Admin      = {}",
                fields.name, fields.account, fields.admin
            );
            if !fields.allowed_qos.is_empty() {
                println!("  QOS        = {}", fields.allowed_qos);
            }
            if !fields.default_qos.is_empty() {
                println!("  DefQOS     = {}", fields.default_qos);
            }
            let unset = spur_core::accounting::INFINITE;
            if fields.max_running_jobs != unset {
                println!("  MaxJobs    = {}", fields.max_running_jobs);
            }
            if fields.max_submit_jobs != unset {
                println!("  MaxSubmit  = {}", fields.max_submit_jobs);
            }
            if fields.grp_submit_jobs != unset {
                println!("  GrpSubmit  = {}", fields.grp_submit_jobs);
            }
            if fields.max_wall_minutes != unset {
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
            let no_limit = spur_core::accounting::INFINITE;
            let max_jobs: u32 = find_alias(&p, &["maxjobsperuser", "maxjobspu"])
                .map(|(k, v)| parse_limit(k, v))
                .transpose()?
                .unwrap_or(no_limit);
            let max_wall: u32 = p
                .get("maxwall")
                .map(|v| parse_wall_limit("maxwall", v))
                .transpose()?
                .unwrap_or(no_limit);
            let max_tres = p.get("maxtresperjob").cloned().unwrap_or_default();
            let grp_wall: u32 = p
                .get("grpwall")
                .map(|v| parse_wall_limit("grpwall", v))
                .transpose()?
                .unwrap_or(no_limit);

            let mut client = connect(addr).await?;
            client
                .create_qos(CreateQosRequest {
                    name: name.clone(),
                    description: Some(desc),
                    priority: Some(priority),
                    preempt_mode: Some(preempt.clone()),
                    preempt: p.get("preempt").cloned(),
                    usage_factor: Some(usage_factor),
                    max_jobs_per_user: Some(max_jobs),
                    max_wall_minutes: Some(max_wall),
                    max_tres_per_job: Some(max_tres),
                    max_submit_jobs_per_user: Some(
                        p.get("maxsubmitjobsperuser")
                            .map(|v| parse_limit("maxsubmitjobsperuser", v))
                            .transpose()?
                            .unwrap_or(no_limit),
                    ),
                    max_tres_per_user: Some(p.get("maxtresperuser").cloned().unwrap_or_default()),
                    grp_tres: Some(p.get("grptres").cloned().unwrap_or_default()),
                    grp_wall_minutes: Some(grp_wall),
                    preempt_exempt_time: p
                        .get("preemptexempttime")
                        .map(|v| parse_u32("preemptexempttime", v))
                        .transpose()?,
                    clear_preempt_exempt_time: false,
                    max_submit_jobs_per_account: Some(
                        find_alias(
                            &p,
                            &["maxsubmitjobsperaccount", "maxsubmitpa", "maxsubmitjobspa"],
                        )
                        .map(|(k, v)| parse_limit(k, v))
                        .transpose()?
                        .unwrap_or(no_limit),
                    ),
                    grp_submit_jobs: Some(
                        find_alias(&p, &["grpsubmit", "grpsubmitjobs"])
                            .map(|(k, v)| parse_limit(k, v))
                            .transpose()?
                            .unwrap_or(no_limit),
                    ),
                    flags: Some(p.get("flags").cloned().unwrap_or_default()),
                })
                .await
                .context("CreateQos RPC failed")?;

            println!(
                " Adding QOS(s)\n  Name       = {}\n  Priority   = {}\n  Preempt    = {}",
                name, priority, preempt
            );
            if max_wall != no_limit {
                println!("  MaxWall    = {} min", max_wall);
            }
            if max_jobs != no_limit {
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
            let acct_display = if account.is_empty() { "all" } else { &account };
            match client
                .remove_user(RemoveUserRequest {
                    user: name.clone(),
                    account: account.clone(),
                })
                .await
            {
                Ok(_) => {
                    println!(" Deleting user {} from account {}", name, acct_display);
                    println!(" Done.");
                    Ok(())
                }
                // Slurm prints "Nothing deleted" and exits 0 when the delete is a no-op.
                Err(status) if status.code() == tonic::Code::NotFound => {
                    println!(" Nothing deleted.");
                    Ok(())
                }
                Err(status) => Err(anyhow::Error::new(status).context("RemoveUser RPC failed")),
            }
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

/// Build a partial-patch `CreateAccountRequest` for `modify account`: a field
/// is `Some` only when the command restated it (so the server leaves every
/// other column untouched), and an explicitly empty value clears it.
fn build_modify_account_request(
    p: &std::collections::HashMap<String, String>,
) -> Result<CreateAccountRequest> {
    reject_unknown_keys(p, ACCOUNT_KEYS)?;
    let name = p
        .get("name")
        .or_else(|| p.get("account"))
        .ok_or_else(|| anyhow::anyhow!("name= required"))?
        .clone();
    Ok(CreateAccountRequest {
        name,
        description: p.get("description").cloned(),
        organization: p.get("organization").cloned(),
        parent_account: p.get("parent").cloned(),
        fairshare_weight: p
            .get("fairshare")
            .map(|v| parse_f64("fairshare", v))
            .transpose()?,
        max_running_jobs: find_alias(p, &["maxrunningjobs", "maxjobs"])
            .map(|(k, v)| parse_limit(k, v))
            .transpose()?,
        grp_tres: p.get("grptres").cloned(),
    })
}

/// Build a partial-patch `CreateQosRequest` for `modify qos` (see
/// `build_modify_account_request` for the presence contract).
fn build_modify_qos_request(
    p: &std::collections::HashMap<String, String>,
) -> Result<CreateQosRequest> {
    reject_unknown_keys(p, QOS_KEYS)?;
    let name = p
        .get("name")
        .or_else(|| p.get("qos"))
        .ok_or_else(|| anyhow::anyhow!("name= required"))?
        .clone();
    Ok(CreateQosRequest {
        name,
        description: p.get("description").cloned(),
        priority: p
            .get("priority")
            .map(|v| parse_i32("priority", v))
            .transpose()?,
        preempt_mode: p.get("preemptmode").cloned(),
        preempt: p.get("preempt").cloned(),
        usage_factor: p
            .get("usagefactor")
            .map(|v| parse_f64("usagefactor", v))
            .transpose()?,
        max_jobs_per_user: find_alias(p, &["maxjobsperuser", "maxjobspu"])
            .map(|(k, v)| parse_limit(k, v))
            .transpose()?,
        max_wall_minutes: p
            .get("maxwall")
            .map(|v| parse_wall_limit("maxwall", v))
            .transpose()?,
        max_tres_per_job: p.get("maxtresperjob").cloned(),
        max_submit_jobs_per_user: p
            .get("maxsubmitjobsperuser")
            .map(|v| parse_limit("maxsubmitjobsperuser", v))
            .transpose()?,
        max_tres_per_user: p.get("maxtresperuser").cloned(),
        grp_tres: p.get("grptres").cloned(),
        grp_wall_minutes: p
            .get("grpwall")
            .map(|v| parse_wall_limit("grpwall", v))
            .transpose()?,
        preempt_exempt_time: p
            .get("preemptexempttime")
            .map(|v| parse_u32("preemptexempttime", v))
            .transpose()?,
        clear_preempt_exempt_time: p
            .get("clearpreemptexempttime")
            .map(|v| is_truthy(v))
            .unwrap_or(false),
        max_submit_jobs_per_account: find_alias(
            p,
            &["maxsubmitjobsperaccount", "maxsubmitpa", "maxsubmitjobspa"],
        )
        .map(|(k, v)| parse_limit(k, v))
        .transpose()?,
        grp_submit_jobs: find_alias(p, &["grpsubmit", "grpsubmitjobs"])
            .map(|(k, v)| parse_limit(k, v))
            .transpose()?,
        flags: p.get("flags").cloned(),
    })
}

/// Build a partial-patch `AddUserRequest` for `modify user` (see
/// `build_modify_account_request` for the presence contract). Unlike `add user`,
/// an omitted `qos`/`defaultqos` stays `None` so the server keeps the stored
/// value; the default-in-allow-list check runs only when both are restated.
fn build_modify_user_request(
    p: &std::collections::HashMap<String, String>,
) -> Result<AddUserRequest> {
    reject_unknown_keys(p, USER_KEYS)?;
    let user = p
        .get("name")
        .or_else(|| p.get("user"))
        .ok_or_else(|| anyhow::anyhow!("name= required"))?
        .clone();
    let account = resolve_user_account(p)?;
    let default_qos = p.get("defaultqos").cloned();
    let allowed_qos = p.get("qos").cloned();
    if let (Some(dq), Some(list)) = (default_qos.as_deref(), allowed_qos.as_deref()) {
        if !dq.is_empty() && !list.is_empty() && !list.split(',').map(str::trim).any(|q| q == dq) {
            bail!("defaultqos={dq} must be included in qos={list}");
        }
    }
    Ok(AddUserRequest {
        user,
        is_default: p.get("defaultaccount").map(|da| da == &account),
        account,
        admin_level: p.get("adminlevel").cloned(),
        default_qos,
        allowed_qos,
        max_running_jobs: find_alias(p, &["maxrunningjobs", "maxjobs"])
            .map(|(k, v)| parse_limit(k, v))
            .transpose()?,
        max_submit_jobs: p
            .get("maxsubmitjobs")
            .map(|v| parse_limit("maxsubmitjobs", v))
            .transpose()?,
        grp_submit_jobs: find_alias(p, &["grpsubmit", "grpsubmitjobs"])
            .map(|(k, v)| parse_limit(k, v))
            .transpose()?,
        max_tres_per_job: p.get("maxtresperjob").cloned(),
        grp_tres: p.get("grptres").cloned(),
        max_wall_minutes: find_alias(p, &["maxwall", "maxwallduration"])
            .map(|(k, v)| parse_wall_limit(k, v))
            .transpose()?,
    })
}

/// Modify shares `add`'s upsert RPCs, but sends only the restated fields
/// (proto3 presence) so the server preserves every unstated column.
async fn modify(entity: &str, params: &[String], addr: &str) -> Result<()> {
    let p = parse_params(params);

    match entity.to_lowercase().as_str() {
        "account" => {
            let req = build_modify_account_request(&p)?;
            let name = req.name.clone();
            let mut client = connect(addr).await?;
            client
                .create_account(req)
                .await
                .context("CreateAccount (modify) RPC failed")?;
            println!(" Modified account '{}'.", name);
            Ok(())
        }
        "qos" => {
            let req = build_modify_qos_request(&p)?;
            let name = req.name.clone();
            let mut client = connect(addr).await?;
            client
                .create_qos(req)
                .await
                .context("CreateQos (modify) RPC failed")?;
            println!(" Modified QOS '{}'.", name);
            Ok(())
        }
        "user" => {
            let req = build_modify_user_request(&p)?;
            let name = req.user.clone();
            let mut client = connect(addr).await?;
            client
                .add_user(req)
                .await
                .context("AddUser (modify) RPC failed")?;
            println!(" Modified user '{}'.", name);
            Ok(())
        }
        other => bail!("sacctmgr: unknown entity type '{}'", other),
    }
}

/// Entities `show` prints as hardcoded fixed-width tables, with no field-spec table behind
/// them. A new entity rendered that way belongs here.
const FIXED_WIDTH_ENTITIES: [&str; 5] = ["user", "users", "association", "associations", "tres"];

/// Delimiting a fixed-width table hands a script padded text it cannot parse, so refuse it.
/// Unknown entities fall through to `show`, which reports them itself.
fn reject_unsupported_delimiter(entity: &str, style: format_engine::OutputStyle) -> Result<()> {
    if !style.is_delimited() || !FIXED_WIDTH_ENTITIES.contains(&entity) {
        return Ok(());
    }

    bail!(
        "sacctmgr: delimited output (-p/-P) is not supported for '{entity}'. \
         Supported entities: account, qos, txn"
    )
}

async fn show(
    entity: &str,
    params: &[String],
    addr: &str,
    style: format_engine::OutputStyle,
) -> Result<()> {
    let p = parse_params(params);
    let entity = entity.to_lowercase();
    reject_unsupported_delimiter(&entity, style)?;

    match entity.as_str() {
        "account" | "accounts" => {
            let fields = account_format_fields(p.get("format").map(String::as_str))?;

            let mut client = connect(addr).await?;
            let resp = client
                .list_accounts(ListAccountsRequest {})
                .await
                .context("ListAccounts RPC failed")?;

            let accounts = resp.into_inner().accounts;

            style.print_header(&fields);

            for a in &accounts {
                println!(
                    "{}",
                    style.row(&fields, &|spec| resolve_account_field(a, spec))
                );
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

            if style.shows_header() {
                println!("{}", user_header_row());
                println!("{}", "-".repeat(180));
            }

            for u in &users {
                println!("{}", format_user_row(u));
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

            style.print_header(&fields);

            if qos_list.is_empty() && !has_name_filter {
                let default_qos = QosInfo {
                    name: "normal".into(),
                    preempt_mode: "off".into(),
                    usage_factor: 1.0,
                    ..Default::default()
                };
                println!(
                    "{}",
                    style.row(&fields, &|spec| resolve_qos_field(&default_qos, spec))
                );
            } else {
                for q in &qos_list {
                    println!("{}", style.row(&fields, &|spec| resolve_qos_field(q, spec)));
                }
            }
            Ok(())
        }
        "association" | "associations" => {
            if style.shows_header() {
                println!(
                    "{:<15} {:<20} {:<15} {:<10} {:<10}",
                    "User", "Account", "Partition", "Share", "Default"
                );
                println!("{}", "-".repeat(70));
            }
            Ok(())
        }
        "tres" => {
            if style.shows_header() {
                println!("{:<5} {:<15} {:<10}", "ID", "Type", "Name");
                println!("{}", "-".repeat(30));
            }
            println!("{:<5} {:<15} {:<10}", "1", "cpu", "");
            println!("{:<5} {:<15} {:<10}", "2", "mem", "");
            println!("{:<5} {:<15} {:<10}", "3", "energy", "");
            println!("{:<5} {:<15} {:<10}", "4", "node", "");
            println!("{:<5} {:<15} {:<10}", "1001", "gres/gpu", "");
            println!("{:<5} {:<15} {:<10}", "1002", "billing", "");
            Ok(())
        }
        "txn" | "transaction" | "transactions" => {
            let fields = txn_format_fields(p.get("format").map(String::as_str))?;
            let request = build_txn_request(&p);

            let mut client = connect(addr).await?;
            let txns = client
                .get_transactions(request)
                .await
                .context("GetTransactions RPC failed")?
                .into_inner()
                .transactions;

            style.print_header(&fields);
            for t in &txns {
                println!("{}", style.row(&fields, &|spec| resolve_txn_field(t, spec)));
            }
            Ok(())
        }
        other => bail!(
            "sacctmgr: unknown entity '{}'. Use: account, user, qos, association, tres, txn",
            other
        ),
    }
}

fn filter_qos_by_name(qos_list: &mut Vec<QosInfo>, filter: &str) {
    let names: Vec<&str> = filter.split(',').map(str::trim).collect();
    qos_list.retain(|q| names.iter().any(|n| n.eq_ignore_ascii_case(&q.name)));
}

// Slurm's default `sacctmgr show transaction` columns: Time, Action, Actor,
// Where, Info. Where renders entity_type:entity_name; Info renders details JSON.
const TXN_DEFAULT_FORMAT: &str = "%-20t %-8a %-14A %-24w %-40i";
const TXN_ALL_FORMAT: &str = "%-8d %-20t %-8a %-14A %-6v %-8s %-24w %-10o %-8u %-40i";

fn txn_header(spec: char) -> &'static str {
    match spec {
        't' => "Time",
        'a' => "Action",
        'A' => "Actor",
        'w' => "Where",
        'i' => "Info",
        'o' => "Outcome",
        'v' => "Verified",
        's' => "Source",
        'd' => "ID",
        'u' => "ActorUID",
        _ => "?",
    }
}

fn txn_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "time" | "timestamp" | "ts" => Some('t'),
        "action" => Some('a'),
        "actor" => Some('A'),
        "where" | "entity" => Some('w'),
        "info" | "details" => Some('i'),
        "outcome" => Some('o'),
        "verified" => Some('v'),
        "source" => Some('s'),
        "id" => Some('d'),
        "actoruid" | "uid" => Some('u'),
        _ => None,
    }
}

fn txn_format_fields(
    format_param: Option<&str>,
) -> anyhow::Result<Vec<format_engine::FormatToken>> {
    format_engine::resolve_format(
        format_param,
        TXN_DEFAULT_FORMAT,
        TXN_ALL_FORMAT,
        &txn_field_spec,
        &txn_header,
        "Time, Action, Actor, Where, Info, Outcome, Verified, Source, ID, ActorUID",
    )
}

/// Build a `GetTransactions` request from Slurm-style `key=value` filters
/// (`Actor=`, `Action=`, `Entity=`, `Name=`, `Outcome=`, `Start=`, `End=`,
/// `limit=`). `action`/`outcome` are lowercased to match the stored values.
fn build_txn_request(p: &std::collections::HashMap<String, String>) -> GetTransactionsRequest {
    GetTransactionsRequest {
        actor: p.get("actor").cloned().unwrap_or_default(),
        entity_type: p
            .get("entity")
            .or_else(|| p.get("entitytype"))
            .map(|s| s.to_lowercase())
            .unwrap_or_default(),
        entity_name: p
            .get("name")
            .or_else(|| p.get("entityname"))
            .cloned()
            .unwrap_or_default(),
        action: p
            .get("action")
            .map(|s| s.to_lowercase())
            .unwrap_or_default(),
        outcome: p
            .get("outcome")
            .map(|s| s.to_lowercase())
            .unwrap_or_default(),
        start_after: p
            .get("start")
            .and_then(|s| parse_time_arg(s))
            .map(datetime_to_proto),
        start_before: p
            .get("end")
            .and_then(|s| parse_time_arg(s))
            .map(datetime_to_proto),
        limit: p.get("limit").and_then(|s| s.parse().ok()).unwrap_or(0),
    }
}

fn resolve_txn_field(t: &TransactionRecord, spec: char) -> String {
    match spec {
        't' => t.timestamp.as_ref().map(fmt_txn_ts).unwrap_or_default(),
        'a' => t.action.clone(),
        'A' => t.actor.clone(),
        'w' => format!("{}:{}", t.entity_type, t.entity_name),
        'i' => t.details.clone(),
        'o' => t.outcome.clone(),
        'v' => if t.verified { "yes" } else { "no" }.to_string(),
        's' => t.source.clone(),
        'd' => t.id.to_string(),
        // uid is recorded only for a verified identity; render blank otherwise so
        // the unknown case (stored NULL, flattened to 0 on the wire) can't read as root.
        'u' => {
            if t.verified {
                t.actor_uid.to_string()
            } else {
                String::new()
            }
        }
        _ => "?".to_string(),
    }
}

fn fmt_txn_ts(ts: &prost_types::Timestamp) -> String {
    // Sanitize nanos (never displayed) so a negative/out-of-range value can't wrap
    // via `as u32` and blank an otherwise-valid timestamp.
    let nanos = u32::try_from(ts.nanos)
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(ts.seconds, nanos)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

const ACCOUNT_DEFAULT_FORMAT: &str = "%-20N %-30D %-15O %-10P %-10S %-10G";

const ACCOUNT_ALL_FORMAT: &str = "%-20N %-30D %-15O %-10P %-10S %-10G %-10J";

fn account_header(spec: char) -> &'static str {
    match spec {
        'N' => "Account",
        'D' => "Descr",
        'O' => "Org",
        'P' => "Parent",
        'S' => "Share",
        'G' => "GrpTRES",
        'J' => "MaxJobs",
        _ => "?",
    }
}

fn account_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "account" | "name" => Some('N'),
        "description" | "descr" => Some('D'),
        "organization" | "org" => Some('O'),
        "parent" | "parentaccount" => Some('P'),
        "share" | "fairshare" | "fairshareweight" => Some('S'),
        "grptres" => Some('G'),
        "maxjobs" | "maxrunningjobs" => Some('J'),
        _ => None,
    }
}

/// Resolve `show account` output columns from an optional `format=` param. Shared by the handler
/// and its tests so the default/all formats and the valid-field hint stay in one place.
fn account_format_fields(
    format_param: Option<&str>,
) -> anyhow::Result<Vec<format_engine::FormatToken>> {
    format_engine::resolve_format(
        format_param,
        ACCOUNT_DEFAULT_FORMAT,
        ACCOUNT_ALL_FORMAT,
        &account_field_spec,
        &account_header,
        "Account, Descr, Org, Parent, Share, GrpTRES, MaxJobs",
    )
}

/// Render a numeric limit, blank when unset/unlimited (the INFINITE sentinel);
/// Slurm shows "no limit" as an empty cell. A literal 0 renders as "0".
pub(crate) fn blank_if_unset(v: u32) -> String {
    if v == spur_core::accounting::INFINITE {
        String::new()
    } else {
        v.to_string()
    }
}

/// Render a numeric value, blank when zero. Used for `preempt_exempt_time`,
/// whose absence is carried as `None` (not the `INFINITE` sentinel).
fn blank_if_zero(v: u32) -> String {
    if v == 0 {
        String::new()
    } else {
        v.to_string()
    }
}

fn user_header_row() -> String {
    format!(
        "{:<15} {:<20} {:<10} {:<20} {:<20} {:<15} {:<10} {:<10} {:<10} {:<10} {:<16} {:<16}",
        "User",
        "Account",
        "Admin",
        "Default Acct",
        "QOS",
        "Def QOS",
        "MaxJobs",
        "MaxSubmit",
        "GrpSubmit",
        "MaxWall",
        "MaxTRES",
        "GrpTRES",
    )
}

fn format_user_row(u: &UserInfo) -> String {
    format!(
        "{:<15} {:<20} {:<10} {:<20} {:<20} {:<15} {:<10} {:<10} {:<10} {:<10} {:<16} {:<16}",
        u.name,
        u.account,
        u.admin_level,
        u.default_account,
        u.allowed_qos,
        u.default_qos,
        blank_if_unset(u.max_running_jobs),
        blank_if_unset(u.max_submit_jobs),
        blank_if_unset(u.grp_submit_jobs),
        blank_if_unset(u.max_wall_minutes),
        u.max_tres_per_job,
        u.grp_tres,
    )
}

fn resolve_account_field(a: &AccountInfo, spec: char) -> String {
    match spec {
        'N' => a.name.clone(),
        'D' => a.description.clone(),
        'O' => a.organization.clone(),
        'P' => a.parent_account.clone(),
        'S' => (a.fairshare_weight as u32).to_string(),
        'G' => a.grp_tres.clone(),
        'J' => blank_if_unset(a.max_running_jobs),
        _ => "?".into(),
    }
}

const QOS_DEFAULT_FORMAT: &str =
    "%-15N %-8p %-10P %-12U %-10J %-10S %-10W %-10w %-14F %-20T %-20V %-20G";

const QOS_ALL_FORMAT: &str =
    "%-15N %-30D %-8p %-10P %-12U %-10J %-10S %-12A %-12B %-10W %-10w %-14F %-20T %-20V %-20G";

fn qos_header(spec: char) -> &'static str {
    match spec {
        'N' => "Name",
        'D' => "Descr",
        'p' => "Priority",
        'P' => "PreemptMode",
        'Q' => "Preempt",
        'E' => "PreemptExemptTime",
        'U' => "UsageFactor",
        'G' => "GrpTRES",
        'T' => "MaxTRES",
        'V' => "MaxTRESPU",
        'J' => "MaxJobsPU",
        'S' => "MaxSubmitPU",
        'A' => "MaxSubmitPA",
        'B' => "GrpSubmit",
        'W' => "MaxWall",
        'w' => "GrpWall",
        'F' => "Flags",
        _ => "?",
    }
}

fn qos_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "name" => Some('N'),
        "description" | "descr" => Some('D'),
        "priority" | "prio" => Some('p'),
        "preemptmode" => Some('P'),
        "preempt" => Some('Q'),
        "preemptexempttime" => Some('E'),
        "usagefactor" => Some('U'),
        "grptres" => Some('G'),
        "maxtres" | "maxtrespj" | "maxtresperjob" => Some('T'),
        "maxtrespu" | "maxtresperuser" => Some('V'),
        "maxjobspu" | "maxjobsperuser" => Some('J'),
        "maxsubmitpu" | "maxsubmitjobspu" | "maxsubmitjobsperuser" => Some('S'),
        "maxsubmitpa" | "maxsubmitjobspa" | "maxsubmitjobsperaccount" => Some('A'),
        "grpsubmit" | "grpsubmitjobs" => Some('B'),
        "maxwall" | "maxwalldurationperjob" => Some('W'),
        "grpwall" => Some('w'),
        "flags" => Some('F'),
        _ => None,
    }
}

fn resolve_qos_field(q: &QosInfo, spec: char) -> String {
    match spec {
        'N' => q.name.clone(),
        'D' => q.description.clone(),
        'p' => q.priority.to_string(),
        'P' => q.preempt_mode.clone(),
        'Q' => q.preempt.clone(),
        'E' => q.preempt_exempt_time.map(blank_if_zero).unwrap_or_default(),
        'U' => format!("{}", q.usage_factor),
        'G' => q.grp_tres.clone(),
        'T' => q.max_tres_per_job.clone(),
        'V' => q.max_tres_per_user.clone(),
        'J' => blank_if_unset(q.max_jobs_per_user),
        'S' => blank_if_unset(q.max_submit_jobs_per_user),
        'A' => blank_if_unset(q.max_submit_jobs_per_account),
        'B' => blank_if_unset(q.grp_submit_jobs),
        'W' => blank_if_unset(q.max_wall_minutes),
        'w' => blank_if_unset(q.grp_wall_minutes),
        'F' => q.flags.clone(),
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
    fn build_add_user_request_parses_qos_allow_list() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "qos=highprio,lowprio".into(),
            "defaultqos=highprio".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.allowed_qos, "highprio,lowprio");
        assert_eq!(fields.default_qos, "highprio");
    }

    #[test]
    fn build_add_user_request_allowed_qos_absent_is_empty() {
        let p = parse_params(&["name=testuser".into(), "account=testacct".into()]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.allowed_qos, "");
    }

    #[test]
    fn build_add_user_request_rejects_default_qos_outside_allow_list() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "qos=highprio,lowprio".into(),
            "defaultqos=other-teams-qos".into(),
        ]);
        let err = build_add_user_request(&p).unwrap_err();
        assert!(err.to_string().contains("must be included in qos="));
    }

    #[test]
    fn build_add_user_request_allows_defaultqos_alone_without_a_list() {
        // Pinning only a default (no qos= list) is still valid — it's
        // PR #490's single-QOS scoping, not a validation error.
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "defaultqos=highprio".into(),
        ]);
        assert!(build_add_user_request(&p).is_ok());
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
            "maxsubmitjobsperaccount",
            "grpsubmit",
            "maxtresperuser",
            "grptres",
            "grpwall",
            "flags",
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
    fn reject_unknown_keys_accepts_every_parsed_user_field() {
        let parsed_fields = [
            "name",
            "user",
            "account",
            "defaultaccount",
            "adminlevel",
            "defaultqos",
            "qos",
            "maxrunningjobs",
            "maxjobs",
            "maxsubmitjobs",
            "grpsubmit",
            "maxtresperjob",
            "grptres",
            "maxwall",
            "maxwallduration",
        ];
        for field in parsed_fields {
            let p = parse_params(&["name=testuser".into(), format!("{field}=1")]);
            assert!(
                reject_unknown_keys(&p, USER_KEYS).is_ok(),
                "user field '{field}' is read by the handler but missing from USER_KEYS"
            );
        }
    }

    #[test]
    fn build_add_user_request_rejects_unknown_field() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "qoz=highprio".into(),
        ]);
        let err = build_add_user_request(&p).unwrap_err();
        assert!(err.to_string().contains("unknown field 'qoz'"));
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
    fn reject_unknown_keys_accepts_every_parsed_account_field() {
        // Every key the add/modify account handlers read must be allowlisted,
        // otherwise reject_unknown_keys would bounce a valid field.
        let parsed_fields = [
            "name",
            "account",
            "description",
            "organization",
            "parent",
            "fairshare",
            "maxrunningjobs",
            "maxjobs",
            "grptres",
        ];
        for field in parsed_fields {
            let p = parse_params(&["name=acct".into(), format!("{field}=1")]);
            assert!(
                reject_unknown_keys(&p, ACCOUNT_KEYS).is_ok(),
                "account field '{field}' is read by the handler but missing from ACCOUNT_KEYS"
            );
        }
    }

    #[test]
    fn reject_unknown_keys_flags_mistyped_account_grptres() {
        // A typo'd limit key must error, not be silently dropped.
        let p = parse_params(&["name=acct".into(), "grptre=cpu=8".into()]);
        let err = reject_unknown_keys(&p, ACCOUNT_KEYS).unwrap_err();
        assert!(err.to_string().contains("unknown field 'grptre'"));
    }

    #[test]
    fn parse_params_keeps_account_grptres_value_intact() {
        // The comma-separated TRES value must survive as a single value (the `add
        // account` handler reads p["grptres"] and forwards it to the RPC verbatim).
        let p = parse_params(&[
            "name=physics".into(),
            "grptres=cpu=16,mem=32768,gres/gpu=8".into(),
        ]);
        assert_eq!(
            p.get("grptres").map(String::as_str),
            Some("cpu=16,mem=32768,gres/gpu=8")
        );
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
    fn build_add_user_request_account_limits_absent_are_unset() {
        // Absent numeric limits default to the INFINITE sentinel (leave
        // unchanged / no limit), not a literal 0 which would block all.
        let unset = spur_core::accounting::INFINITE;
        let p = parse_params(&["name=testuser".into(), "account=testacct".into()]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_running_jobs, unset);
        assert_eq!(fields.max_submit_jobs, unset);
        assert_eq!(fields.max_tres_per_job, "");
        assert_eq!(fields.grp_tres, "");
        assert_eq!(fields.max_wall_minutes, unset);
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
    fn build_add_user_request_maxsubmitjobs_minus_one_clears() {
        // -1 is the clear sentinel (INFINITE), not an error.
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxsubmitjobs=-1".into(),
        ]);
        let fields = build_add_user_request(&p).unwrap();
        assert_eq!(fields.max_submit_jobs, spur_core::accounting::INFINITE);
    }

    #[test]
    fn build_add_user_request_rejects_other_negative_maxsubmitjobs() {
        let p = parse_params(&[
            "name=testuser".into(),
            "account=testacct".into(),
            "maxsubmitjobs=-5".into(),
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
    fn build_modify_account_request_omits_unrestated_fields() {
        // `modify account name=X set fairshare=2` must send only fairshare;
        // every other field is None so the server leaves it untouched.
        let p = parse_params(&["name=physics".into(), "set".into(), "fairshare=2".into()]);
        let req = build_modify_account_request(&p).unwrap();
        assert_eq!(req.name, "physics");
        assert_eq!(req.fairshare_weight, Some(2.0));
        assert_eq!(req.description, None);
        assert_eq!(req.organization, None);
        assert_eq!(req.parent_account, None);
        assert_eq!(req.max_running_jobs, None);
        assert_eq!(req.grp_tres, None);
    }

    #[test]
    fn build_modify_account_request_explicit_empty_clears() {
        // An explicit `grptres=` is a request to clear, distinct from omitting
        // it (which preserves).
        let p = parse_params(&["name=physics".into(), "grptres=".into()]);
        let req = build_modify_account_request(&p).unwrap();
        assert_eq!(req.grp_tres, Some(String::new()));
    }

    #[test]
    fn build_modify_account_request_rejects_invalid_fairshare() {
        let p = parse_params(&["name=physics".into(), "fairshare=abc".into()]);
        assert!(build_modify_account_request(&p).is_err());
    }

    #[test]
    fn find_alias_returns_the_key_the_caller_wrote() {
        // The priority alias wins and is reported as written.
        let p = parse_params(&["maxrunningjobs=3".into()]);
        assert_eq!(
            find_alias(&p, &["maxrunningjobs", "maxjobs"]),
            Some(("maxrunningjobs", "3"))
        );
        // The fallback alias is reported under its own name, not the canonical.
        let p = parse_params(&["maxjobs=4".into()]);
        assert_eq!(
            find_alias(&p, &["maxrunningjobs", "maxjobs"]),
            Some(("maxjobs", "4"))
        );
        // No alias present.
        let p = parse_params(&["other=1".into()]);
        assert_eq!(find_alias(&p, &["maxrunningjobs", "maxjobs"]), None);
    }

    #[test]
    fn build_modify_account_request_error_names_the_typed_alias() {
        // An invalid maxrunningjobs must report that key, not the canonical
        // `maxjobs` the user never typed.
        let p = parse_params(&["name=physics".into(), "maxrunningjobs=abc".into()]);
        let msg = build_modify_account_request(&p).unwrap_err().to_string();
        assert!(msg.contains("maxrunningjobs"), "got: {msg}");
        assert!(
            !msg.contains("maxjobs"),
            "reported canonical alias instead: {msg}"
        );

        // The fallback alias is reported under its own name too.
        let p = parse_params(&["name=physics".into(), "maxjobs=abc".into()]);
        let msg = build_modify_account_request(&p).unwrap_err().to_string();
        assert!(msg.contains("maxjobs"), "got: {msg}");
    }

    #[test]
    fn build_modify_qos_request_error_names_the_typed_alias() {
        let p = parse_params(&["name=normal".into(), "maxjobspu=abc".into()]);
        let msg = build_modify_qos_request(&p).unwrap_err().to_string();
        assert!(msg.contains("maxjobspu"), "got: {msg}");
        assert!(
            !msg.contains("maxjobsperuser"),
            "reported canonical alias instead: {msg}"
        );
    }

    #[test]
    fn build_modify_user_request_error_names_the_typed_alias() {
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "maxwallduration=nope".into(),
        ]);
        let msg = build_modify_user_request(&p).unwrap_err().to_string();
        assert!(msg.contains("maxwallduration"), "got: {msg}");
    }

    #[test]
    fn build_modify_qos_request_omits_unrestated_fields() {
        let p = parse_params(&["name=normal".into(), "set".into(), "priority=10".into()]);
        let req = build_modify_qos_request(&p).unwrap();
        assert_eq!(req.name, "normal");
        assert_eq!(req.priority, Some(10));
        assert_eq!(req.description, None);
        assert_eq!(req.grp_tres, None);
        assert_eq!(req.max_tres_per_job, None);
        assert_eq!(req.preempt_mode, None);
        assert_eq!(req.usage_factor, None);
    }

    #[test]
    fn build_modify_user_request_omits_unrestated_limits_and_qos() {
        // `modify user ... set maxjobs=5` must not touch the QOS allow-list or
        // the other limits.
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "set".into(),
            "maxjobs=5".into(),
        ]);
        let req = build_modify_user_request(&p).unwrap();
        assert_eq!(req.user, "alice");
        assert_eq!(req.account, "physics");
        assert_eq!(req.max_running_jobs, Some(5));
        assert_eq!(req.default_qos, None);
        assert_eq!(req.allowed_qos, None);
        assert_eq!(req.grp_tres, None);
        assert_eq!(req.max_submit_jobs, None);
        assert_eq!(req.admin_level, None);
        assert_eq!(req.is_default, None);
    }

    #[test]
    fn build_modify_user_request_explicit_empty_qos_clears() {
        let p = parse_params(&["name=alice".into(), "account=physics".into(), "qos=".into()]);
        let req = build_modify_user_request(&p).unwrap();
        assert_eq!(req.allowed_qos, Some(String::new()));
    }

    #[test]
    fn build_modify_user_request_rejects_default_outside_restated_list() {
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "qos=a,b".into(),
            "defaultqos=c".into(),
        ]);
        assert!(build_modify_user_request(&p).is_err());
    }

    #[test]
    fn build_modify_user_request_defaultaccount_alone_marks_default() {
        let p = parse_params(&["name=alice".into(), "defaultaccount=physics".into()]);
        let req = build_modify_user_request(&p).unwrap();
        assert_eq!(req.account, "physics");
        assert_eq!(req.is_default, Some(true));
    }

    #[test]
    fn build_modify_user_request_matching_account_and_defaultaccount_marks_default() {
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "defaultaccount=physics".into(),
        ]);
        let req = build_modify_user_request(&p).unwrap();
        assert_eq!(req.account, "physics");
        assert_eq!(req.is_default, Some(true));
    }

    #[test]
    fn build_modify_user_request_rejects_conflicting_account_and_defaultaccount() {
        // Two different accounts can't collapse into one association without
        // silently clearing the default, so the command must fail loudly.
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "defaultaccount=chemistry".into(),
        ]);
        assert!(build_modify_user_request(&p).is_err());
    }

    #[test]
    fn build_modify_user_request_rejects_empty_account() {
        let p = parse_params(&["name=alice".into(), "account=".into()]);
        assert!(build_modify_user_request(&p).is_err());
    }

    #[test]
    fn build_add_user_request_rejects_conflicting_account_and_defaultaccount() {
        let p = parse_params(&[
            "name=alice".into(),
            "account=physics".into(),
            "defaultaccount=chemistry".into(),
        ]);
        assert!(build_add_user_request(&p).is_err());
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
    fn qos_resolve_unset_fields_are_blank_and_zero_is_shown() {
        // Post sentinel-flip: the INFINITE sentinel renders blank (no limit);
        // a literal 0 renders as "0" (block all).
        let unset = spur_core::accounting::INFINITE;
        let q = QosInfo {
            name: "normal".into(),
            preempt_mode: "off".into(),
            usage_factor: 1.0,
            max_jobs_per_user: unset,
            max_submit_jobs_per_user: unset,
            max_wall_minutes: unset,
            grp_wall_minutes: unset,
            max_submit_jobs_per_account: unset,
            grp_submit_jobs: unset,
            ..Default::default()
        };
        assert_eq!(resolve_qos_field(&q, 'J'), "");
        assert_eq!(resolve_qos_field(&q, 'S'), "");
        assert_eq!(resolve_qos_field(&q, 'W'), "");
        assert_eq!(resolve_qos_field(&q, 'w'), "");
        assert_eq!(resolve_qos_field(&q, 'A'), "");
        assert_eq!(resolve_qos_field(&q, 'B'), "");

        let blocking = QosInfo {
            max_jobs_per_user: 0,
            ..q
        };
        assert_eq!(resolve_qos_field(&blocking, 'J'), "0");
    }

    #[test]
    fn format_user_row_renders_limits_and_blanks_unset() {
        let unset = spur_core::accounting::INFINITE;
        let header = user_header_row();
        for column in [
            "User",
            "Account",
            "MaxJobs",
            "MaxSubmit",
            "GrpSubmit",
            "MaxWall",
            "MaxTRES",
            "GrpTRES",
        ] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }

        let u = UserInfo {
            name: "carol".into(),
            account: "ml".into(),
            admin_level: "None".into(),
            max_running_jobs: 4,
            max_submit_jobs: 8,
            grp_submit_jobs: unset,
            max_wall_minutes: 1440,
            max_tres_per_job: "cpu=64".into(),
            grp_tres: String::new(),
            ..Default::default()
        };
        let row = format_user_row(&u);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert!(cols.contains(&"carol"));
        assert!(cols.contains(&"4"));
        assert!(cols.contains(&"8"));
        assert!(cols.contains(&"1440"));
        assert!(cols.contains(&"cpu=64"));
        // grp_submit_jobs is the INFINITE sentinel: it must render as a blank
        // cell. The header and row share the same fixed-width layout, so the
        // GrpSubmit column occupies the same byte range in both; assert that
        // slice of the row is all whitespace.
        let start = header.find("GrpSubmit").expect("header has GrpSubmit");
        let cell = &row[start..start + "GrpSubmit".len()];
        assert!(
            cell.trim().is_empty(),
            "GrpSubmit cell should be blank, got {cell:?}"
        );
    }

    #[test]
    fn parse_limit_minus_one_clears_to_infinite() {
        assert_eq!(
            parse_limit("maxjobs", "-1").unwrap(),
            spur_core::accounting::INFINITE
        );
    }

    #[test]
    fn parse_limit_zero_is_literal_block_all() {
        assert_eq!(parse_limit("maxjobs", "0").unwrap(), 0);
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

    #[test]
    fn list_user_filters_include_user_alias() {
        let p = parse_params(&["user=testuser".into()]);

        let (account, user) = list_user_filters(&p);

        assert!(account.is_empty());
        assert_eq!(user, "testuser");
    }

    #[test]
    fn build_txn_request_maps_filters_and_lowercases_action() {
        let p = parse_params(&[
            "actor=alice".into(),
            "action=Delete".into(),
            "entity=reservation".into(),
            "name=maint".into(),
            "outcome=Denied".into(),
            "start=2024-01-01".into(),
            "limit=50".into(),
        ]);

        let req = build_txn_request(&p);

        assert_eq!(req.actor, "alice");
        assert_eq!(req.action, "delete");
        assert_eq!(req.entity_type, "reservation");
        assert_eq!(req.entity_name, "maint");
        assert_eq!(req.outcome, "denied");
        assert_eq!(req.limit, 50);
        assert!(req.start_after.is_some());
        assert!(req.start_before.is_none());
    }

    #[test]
    fn resolve_txn_field_renders_where_and_verified() {
        let t = TransactionRecord {
            id: 7,
            timestamp: None,
            actor: "bob".into(),
            actor_uid: 1000,
            verified: true,
            source: "api".into(),
            action: "create".into(),
            entity_type: "reservation".into(),
            entity_name: "daily".into(),
            outcome: "success".into(),
            details: "{}".into(),
        };
        assert_eq!(resolve_txn_field(&t, 'w'), "reservation:daily");
        assert_eq!(resolve_txn_field(&t, 'v'), "yes");
        assert_eq!(resolve_txn_field(&t, 'A'), "bob");
        assert_eq!(resolve_txn_field(&t, 'd'), "7");
        assert_eq!(resolve_txn_field(&t, 'u'), "1000");
    }

    #[test]
    fn resolve_txn_field_blanks_uid_when_unverified() {
        // Unverified rows carry an unknown uid (stored NULL, 0 on the wire); it
        // must render blank so it can't be mistaken for root (uid 0).
        let t = TransactionRecord {
            actor: "vm".into(),
            actor_uid: 0,
            verified: false,
            ..Default::default()
        };
        assert_eq!(resolve_txn_field(&t, 'u'), "");
        assert_eq!(resolve_txn_field(&t, 'v'), "no");
    }

    #[test]
    fn fmt_txn_ts_sanitizes_bad_nanos() {
        let bad = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: -1,
        };
        let good = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        };
        // A negative nanos must not wrap via `as u32` and blank the timestamp.
        assert!(!fmt_txn_ts(&bad).is_empty());
        assert_eq!(fmt_txn_ts(&bad), fmt_txn_ts(&good));
    }

    fn stub_account() -> AccountInfo {
        AccountInfo {
            name: "physics".into(),
            description: "Physics dept".into(),
            organization: "sciences".into(),
            parent_account: "root".into(),
            fairshare_weight: 5.0,
            max_running_jobs: 10,
            grp_tres: "node=4,cpu=256".into(),
        }
    }

    #[test]
    fn account_named_format_selects_and_orders_fields() {
        let fields = format_engine::parse_named_format(
            "Account,GrpTRES",
            &account_field_spec,
            &account_header,
        );
        let specs: Vec<char> = fields
            .iter()
            .filter_map(|t| match t {
                format_engine::FormatToken::Field(f) => Some(f.spec),
                _ => None,
            })
            .collect();
        assert_eq!(specs, vec!['N', 'G']);

        let a = stub_account();
        let row = format_engine::format_row(&fields, &|spec| resolve_account_field(&a, spec));
        assert!(row.contains("physics"), "Account missing: {row}");
        assert!(row.contains("node=4,cpu=256"), "GrpTRES missing: {row}");
        assert!(
            !row.contains("Physics dept"),
            "Descr should be absent: {row}"
        );
    }

    #[test]
    fn account_format_reorders_columns() {
        let fields = format_engine::parse_named_format(
            "GrpTRES,Account",
            &account_field_spec,
            &account_header,
        );
        let header = format_engine::format_header(&fields);
        let grp = header.find("GrpTRES").expect("GrpTRES header");
        let acct = header.find("Account").expect("Account header");
        assert!(grp < acct, "GrpTRES should precede Account: {header}");
    }

    #[test]
    fn account_default_format_matches_legacy_columns() {
        let fields = format_engine::parse_format(ACCOUNT_DEFAULT_FORMAT, &account_header);
        let header = format_engine::format_header(&fields);
        for col in ["Account", "Descr", "Org", "Parent", "Share", "GrpTRES"] {
            assert!(
                header.contains(col),
                "default header missing {col}: {header}"
            );
        }
    }

    #[test]
    fn account_all_format_includes_maxjobs() {
        let fields = format_engine::parse_format(ACCOUNT_ALL_FORMAT, &account_header);
        let header = format_engine::format_header(&fields);
        assert!(header.contains("MaxJobs"), "all header: {header}");
    }

    #[test]
    fn account_field_spec_aliases_are_case_insensitive() {
        assert_eq!(account_field_spec("account"), account_field_spec("Name"));
        assert_eq!(
            account_field_spec("org"),
            account_field_spec("Organization")
        );
        assert_eq!(
            account_field_spec("parent"),
            account_field_spec("ParentAccount")
        );
        assert_eq!(account_field_spec("share"), account_field_spec("FairShare"));
    }

    #[test]
    fn account_field_spec_rejects_unknown_names() {
        assert_eq!(account_field_spec("bogus"), None);
        assert_eq!(account_field_spec(""), None);
    }

    #[test]
    fn account_resolve_unset_maxjobs_is_blank_and_zero_is_shown() {
        // Post sentinel-flip: INFINITE renders blank (no limit); a literal 0
        // renders "0" (block all).
        let unset = AccountInfo {
            max_running_jobs: spur_core::accounting::INFINITE,
            ..stub_account()
        };
        assert_eq!(resolve_account_field(&unset, 'J'), "");

        let blocking = AccountInfo {
            max_running_jobs: 0,
            ..stub_account()
        };
        assert_eq!(resolve_account_field(&blocking, 'J'), "0");
    }

    #[test]
    fn account_resolve_nonzero_maxjobs_renders_value() {
        let a = AccountInfo {
            max_running_jobs: 10,
            ..stub_account()
        };
        assert_eq!(resolve_account_field(&a, 'J'), "10");
    }

    #[test]
    fn account_resolve_unknown_spec_is_visible_marker() {
        // A header spec missing a resolver renders "?" (not a silently blank column).
        assert_eq!(resolve_account_field(&stub_account(), 'Z'), "?");
    }

    #[test]
    fn account_resolve_format_all_expands_to_all_columns() {
        let fields = account_format_fields(Some("all")).unwrap();
        let header = format_engine::format_header(&fields);
        assert!(header.contains("MaxJobs"), "all header: {header}");
        assert!(header.contains("GrpTRES"), "all header: {header}");
    }

    #[test]
    fn account_resolve_format_unknown_field_errors() {
        assert!(account_format_fields(Some("bogus")).is_err());
    }

    #[test]
    fn account_resolve_format_none_uses_default() {
        let fields = account_format_fields(None).unwrap();
        let header = format_engine::format_header(&fields);
        assert!(header.contains("Account"), "default header: {header}");
        assert!(
            !header.contains("MaxJobs"),
            "default should omit MaxJobs: {header}"
        );
    }

    // --- parse_u32 / is_truthy ---

    #[test]
    fn parse_u32_accepts_valid_integer() {
        assert_eq!(parse_u32("preemptexempttime", "120").unwrap(), 120);
        assert_eq!(parse_u32("preemptexempttime", "0").unwrap(), 0);
    }

    #[test]
    fn parse_u32_rejects_non_integer() {
        let err = parse_u32("preemptexempttime", "abc").unwrap_err();
        assert!(
            err.to_string().contains("preemptexempttime"),
            "error names the field: {err}"
        );
        assert!(
            err.to_string().contains("abc"),
            "error names the bad value: {err}"
        );
    }

    #[test]
    fn parse_u32_rejects_negative() {
        assert!(parse_u32("preemptexempttime", "-1").is_err());
    }

    #[test]
    fn is_truthy_accepts_canonical_values() {
        for v in &["1", "yes", "YES", "true", "True", "TRUE"] {
            assert!(is_truthy(v), "{v} should be truthy");
        }
    }

    #[test]
    fn is_truthy_rejects_other_values() {
        for v in &["0", "no", "false", "", "maybe"] {
            assert!(!is_truthy(v), "{v} should not be truthy");
        }
    }

    // --- clearpreemptexempttime must require explicit value ---

    #[test]
    fn clear_preempt_exempt_time_requires_truthy_value() {
        // A bare key (parse_params maps it to "") must NOT trigger the clear.
        let p: std::collections::HashMap<String, String> = [
            ("name".into(), "q".into()),
            ("clearpreemptexempttime".into(), "".into()),
        ]
        .into_iter()
        .collect();
        let req = build_modify_qos_request(&p).unwrap();
        assert!(!req.clear_preempt_exempt_time, "bare key must not clear");
    }

    #[test]
    fn clear_preempt_exempt_time_fires_on_explicit_one() {
        let p: std::collections::HashMap<String, String> = [
            ("name".into(), "q".into()),
            ("clearpreemptexempttime".into(), "1".into()),
        ]
        .into_iter()
        .collect();
        let req = build_modify_qos_request(&p).unwrap();
        assert!(req.clear_preempt_exempt_time);
    }

    #[test]
    fn preempt_exempt_time_bad_value_errors() {
        let p: std::collections::HashMap<String, String> = [
            ("name".into(), "q".into()),
            ("preemptexempttime".into(), "not-a-number".into()),
        ]
        .into_iter()
        .collect();
        assert!(
            build_modify_qos_request(&p).is_err(),
            "malformed preemptexempttime should error"
        );
    }

    #[test]
    fn scripted_query_with_header_and_delimiter_flags_parses() {
        let args = SacctmgrArgs::try_parse_from([
            "sacctmgr",
            "-n",
            "-P",
            "show",
            "qos",
            "format=Name,Priority,MaxWall",
        ])
        .expect("-n -P must parse");

        assert!(args.noheader);
        assert!(args.parsable2);
        assert!(!args.parsable);
    }

    #[test]
    fn delimiter_flag_long_names_match_slurm() {
        let trailing =
            SacctmgrArgs::try_parse_from(["sacctmgr", "--noheader", "--parsable", "show", "qos"])
                .expect("--parsable must parse");
        assert!(trailing.noheader);
        assert!(trailing.parsable);
        assert!(!trailing.parsable2);

        let no_trailing = SacctmgrArgs::try_parse_from(["sacctmgr", "--parsable2", "show", "qos"])
            .expect("--parsable2 must parse");
        assert!(no_trailing.parsable2);
        assert!(!no_trailing.parsable);
    }

    #[test]
    fn header_and_delimiter_flags_leave_other_arguments_alone() {
        let args = SacctmgrArgs::try_parse_from([
            "sacctmgr",
            "-i",
            "-n",
            "-P",
            "show",
            "qos",
            "format=Name,Priority",
        ])
        .expect("flags must compose with existing globals");

        assert!(args.immediate);
        assert_eq!(args.controller, "http://localhost:6817");

        let SacctmgrCommand::Show { entity, params } = args.command else {
            panic!("expected a show command");
        };
        assert_eq!(entity, "qos");
        assert_eq!(params, vec!["format=Name,Priority".to_string()]);
    }

    #[test]
    fn a_delimiter_flag_after_a_param_is_still_parsed() {
        let args =
            SacctmgrArgs::try_parse_from(["sacctmgr", "show", "qos", "format=Name,Priority", "-P"])
                .expect("a flag after a param must parse");

        assert!(args.parsable2);
        let SacctmgrCommand::Show { params, .. } = args.command else {
            panic!("expected a show command");
        };
        assert_eq!(params, vec!["format=Name,Priority".to_string()]);
    }

    #[test]
    fn a_delimiter_flag_between_params_does_not_swallow_the_next() {
        let args = SacctmgrArgs::try_parse_from([
            "sacctmgr",
            "show",
            "qos",
            "name=gpu",
            "-P",
            "format=Name",
        ])
        .expect("a flag between params must parse");

        assert!(args.parsable2);
        let SacctmgrCommand::Show { params, .. } = args.command else {
            panic!("expected a show command");
        };
        assert_eq!(
            params,
            vec!["name=gpu".to_string(), "format=Name".to_string()],
            "the flag must not consume the following filter as its value"
        );
    }

    #[test]
    fn a_flag_among_modify_params_does_not_swallow_an_update() {
        let args = SacctmgrArgs::try_parse_from([
            "sacctmgr",
            "modify",
            "qos",
            "gpu",
            "set",
            "-i",
            "priority=10",
        ])
        .expect("a flag among modify params must parse");

        assert!(args.immediate);
        let SacctmgrCommand::Modify { params, .. } = args.command else {
            panic!("expected a modify command");
        };
        assert_eq!(
            parse_params(&params).get("priority").map(String::as_str),
            Some("10"),
            "a swallowed flag would consume the update and silently apply nothing"
        );
    }

    fn style_from(flags: &[&str]) -> format_engine::OutputStyle {
        let mut argv = vec!["sacctmgr"];
        argv.extend_from_slice(flags);
        argv.extend_from_slice(&["show", "qos"]);

        SacctmgrArgs::try_parse_from(argv)
            .expect("flags must parse")
            .output_style()
    }

    /// A `show qos` row for the given flags, rendered through the real QOS resolver.
    fn qos_row(flags: &[&str], format: &str) -> String {
        let fields = format_engine::parse_named_format(format, &qos_field_spec, &qos_header);
        style_from(flags).row(&fields, &|spec| resolve_qos_field(&stub_qos(), spec))
    }

    #[test]
    fn parsable2_row_has_no_trailing_delimiter() {
        assert_eq!(qos_row(&["-P"], "Name,Priority"), "gpuqos|100");
    }

    #[test]
    fn parsable_row_has_a_trailing_delimiter() {
        assert_eq!(qos_row(&["-p"], "Name,Priority"), "gpuqos|100|");
    }

    #[test]
    fn delimited_values_containing_commas_stay_unambiguous() {
        // TRES values are comma-separated internally, which is why Slurm's parsable output
        // uses '|' rather than ','.
        assert_eq!(qos_row(&["-P"], "Name,GrpTRES"), "gpuqos|node=4,cpu=256");
    }

    #[test]
    fn format_ordering_is_preserved_in_delimited_output() {
        assert_eq!(qos_row(&["-P"], "Priority,Name"), "100|gpuqos");
    }

    #[test]
    fn both_delimiter_flags_resolve_to_parsable2() {
        assert_eq!(qos_row(&["-p", "-P"], "Name,Priority"), "gpuqos|100");
    }

    #[test]
    fn without_a_delimiter_flag_rows_are_byte_identical_to_today() {
        let fields =
            format_engine::parse_named_format(QOS_DEFAULT_FORMAT, &qos_field_spec, &qos_header);
        let q = stub_qos();
        let expected = format_engine::format_row(&fields, &|spec| resolve_qos_field(&q, spec));

        assert_eq!(
            style_from(&[]).row(&fields, &|spec| resolve_qos_field(&q, spec)),
            expected
        );
        assert_eq!(
            style_from(&["-n"]).row(&fields, &|spec| resolve_qos_field(&q, spec)),
            expected
        );
    }

    #[test]
    fn noheader_flag_resolves_into_the_style() {
        assert!(style_from(&[]).shows_header());
        assert!(!style_from(&["-n"]).shows_header());
    }

    fn stub_txn() -> TransactionRecord {
        TransactionRecord {
            id: 7,
            timestamp: None,
            actor: "bob".into(),
            actor_uid: 1000,
            verified: true,
            source: "api".into(),
            action: "create".into(),
            entity_type: "qos".into(),
            entity_name: "gpu".into(),
            outcome: "success".into(),
            details: "{}".into(),
        }
    }

    /// A `show txn` header block and row for the given flags, mirroring the arm's rendering.
    fn txn_render(flags: &[&str], format: &str) -> (Vec<String>, String) {
        let fields = txn_format_fields(Some(format)).expect("txn format must parse");
        let style = SacctmgrArgs::try_parse_from(
            ["sacctmgr"]
                .iter()
                .chain(flags)
                .chain(&["show", "txn"])
                .copied()
                .collect::<Vec<_>>(),
        )
        .expect("flags must parse")
        .output_style();
        let row = style.row(&fields, &|spec| resolve_txn_field(&stub_txn(), spec));
        (style.header_lines(&fields), row)
    }

    #[test]
    fn txn_honours_noheader_and_delimited_output() {
        let (header, row) = txn_render(&["-n", "-P"], "Action,Actor");
        assert!(header.is_empty(), "-n must suppress the txn header");
        assert_eq!(row, "create|bob");

        let (header, _) = txn_render(&[], "Action,Actor");
        assert!(!header.is_empty(), "the txn header must print by default");
    }

    #[test]
    fn delimited_output_is_refused_only_where_columns_are_not_modelled() {
        let delimited = style_from(&["-P"]);
        for entity in [
            "account",
            "accounts",
            "qos",
            "txn",
            "transaction",
            "transactions",
        ] {
            assert!(
                reject_unsupported_delimiter(entity, delimited).is_ok(),
                "{entity} should support delimited output"
            );
        }
        for entity in FIXED_WIDTH_ENTITIES {
            assert!(
                reject_unsupported_delimiter(entity, delimited).is_err(),
                "{entity} should refuse delimited output"
            );
        }

        // Padded output stays available for every entity.
        let padded = style_from(&[]);
        for entity in FIXED_WIDTH_ENTITIES {
            assert!(reject_unsupported_delimiter(entity, padded).is_ok());
        }
    }

    #[tokio::test]
    async fn unsupported_delimiter_fails_before_contacting_the_controller() {
        // Port 1 is unroutable: reaching the network at all would surface a connect error
        // instead, so this also pins that the check runs first.
        let err = show("user", &[], "http://127.0.0.1:1", style_from(&["-P"]))
            .await
            .expect_err("delimited show user must fail");

        assert!(
            err.to_string().contains("not supported for 'user'"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn an_unknown_entity_reports_itself_rather_than_the_delimiter() {
        let err = show("wombat", &[], "http://127.0.0.1:1", style_from(&["-P"]))
            .await
            .expect_err("unknown entity must fail");

        assert!(
            err.to_string().contains("unknown entity 'wombat'"),
            "unexpected error: {err}"
        );
    }
}
