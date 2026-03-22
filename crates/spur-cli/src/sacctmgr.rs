use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::*;

/// Accounting management commands.
#[derive(Parser, Debug)]
#[command(name = "sacctmgr", about = "Spur accounting manager")]
pub struct SacctmgrArgs {
    #[command(subcommand)]
    pub command: SacctmgrCommand,

    /// Accounting daemon address
    #[arg(
        long,
        env = "SPUR_ACCOUNTING_ADDR",
        default_value = "http://localhost:6819",
        global = true
    )]
    pub accounting: String,

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
    let addr = args.accounting.clone();

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

async fn connect(addr: &str) -> Result<SlurmAccountingClient<tonic::transport::Channel>> {
    SlurmAccountingClient::connect(addr.to_string())
        .await
        .context("failed to connect to spurdbd")
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
                .and_then(|v| v.parse().ok())
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
            let name = p
                .get("name")
                .or_else(|| p.get("user"))
                .ok_or_else(|| anyhow::anyhow!("name= required"))?;
            let account = p
                .get("account")
                .or_else(|| p.get("defaultaccount"))
                .ok_or_else(|| anyhow::anyhow!("account= required"))?;
            let admin = p
                .get("adminlevel")
                .cloned()
                .unwrap_or_else(|| "none".into());
            let is_default = p
                .get("defaultaccount")
                .map(|da| da == account)
                .unwrap_or(true);

            let mut client = connect(addr).await?;
            client
                .add_user(AddUserRequest {
                    user: name.clone(),
                    account: account.clone(),
                    admin_level: admin.clone(),
                    is_default,
                })
                .await
                .context("AddUser RPC failed")?;

            println!(
                " Adding User(s)\n  Name       = {}\n  Account    = {}\n  Admin      = {}",
                name, account, admin
            );
            println!(" User added.");
            Ok(())
        }
        "qos" => {
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
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let max_wall: u32 = p
                .get("maxwall")
                .and_then(|v| parse_wall_time(v))
                .unwrap_or(0);
            let max_tres = p.get("maxtresperjob").cloned().unwrap_or_default();

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
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                })
                .await
                .context("CreateAccount (modify) RPC failed")?;

            println!(" Modified account '{}'.", name);
            Ok(())
        }
        "qos" => {
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
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    max_wall_minutes: p
                        .get("maxwall")
                        .and_then(|v| parse_wall_time(v))
                        .unwrap_or(0),
                    max_tres_per_job: p.get("maxtresperjob").cloned().unwrap_or_default(),
                })
                .await
                .context("CreateQos (modify) RPC failed")?;

            println!(" Modified QOS '{}'.", name);
            Ok(())
        }
        "user" => {
            println!(" Modifying user with: {:?}", p);
            println!(
                " (User modify updates admin level via add — use 'sacctmgr add user' to update)"
            );
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
            let account_filter = p.get("account").cloned().unwrap_or_default();

            let mut client = connect(addr).await?;
            let resp = client
                .list_users(ListUsersRequest {
                    account: account_filter,
                })
                .await
                .context("ListUsers RPC failed")?;

            let users = resp.into_inner().users;

            println!(
                "{:<15} {:<20} {:<10} {:<20}",
                "User", "Account", "Admin", "Default Acct"
            );
            println!("{}", "-".repeat(65));

            for u in &users {
                println!(
                    "{:<15} {:<20} {:<10} {:<20}",
                    u.name, u.account, u.admin_level, u.default_account,
                );
            }
            Ok(())
        }
        "qos" => {
            let mut client = connect(addr).await?;
            let resp = client
                .list_qos(ListQosRequest {})
                .await
                .context("ListQos RPC failed")?;

            let qos_list = resp.into_inner().qos_list;

            println!(
                "{:<15} {:<8} {:<10} {:<12} {:<10} {:<10}",
                "Name", "Prio", "Preempt", "UsageFactor", "MaxJobsPU", "MaxWall"
            );
            println!("{}", "-".repeat(65));

            if qos_list.is_empty() {
                // Show default
                println!(
                    "{:<15} {:<8} {:<10} {:<12} {:<10} {:<10}",
                    "normal", "0", "off", "1.0", "", ""
                );
            } else {
                for q in &qos_list {
                    let max_jobs_str = if q.max_jobs_per_user == 0 {
                        String::new()
                    } else {
                        q.max_jobs_per_user.to_string()
                    };
                    let max_wall_str = if q.max_wall_minutes == 0 {
                        String::new()
                    } else {
                        q.max_wall_minutes.to_string()
                    };
                    println!(
                        "{:<15} {:<8} {:<10} {:<12} {:<10} {:<10}",
                        q.name,
                        q.priority,
                        q.preempt_mode,
                        q.usage_factor,
                        max_jobs_str,
                        max_wall_str,
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
