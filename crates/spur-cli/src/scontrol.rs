// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;

use crate::exit_fmt::{format_exit, render_reason};

/// Administrative control commands.
#[derive(Parser, Debug)]
#[command(name = "scontrol", about = "Administrative control for Spur")]
pub struct ScontrolArgs {
    #[command(subcommand)]
    pub command: ScontrolCommand,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    pub controller: String,
}

#[derive(Subcommand, Debug)]
pub enum ScontrolCommand {
    /// Show detailed information
    Show {
        /// Entity type: job, node, partition, config
        entity: String,
        /// Entity name or ID
        name: Option<String>,
    },
    /// Update job/node/partition properties
    Update {
        /// key=value pairs
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Hold a job
    Hold {
        /// Job ID
        job_id: u32,
    },
    /// Release a held job
    Release {
        /// Job ID
        job_id: u32,
    },
    /// Requeue a job
    Requeue {
        /// Job ID
        job_id: u32,
    },
    /// Suspend a running job (SIGSTOP, retains allocation)
    Suspend {
        /// Job ID
        job_id: u32,
    },
    /// Resume a suspended job (SIGCONT)
    Resume {
        /// Job ID
        job_id: u32,
    },
    /// Create a reservation
    #[command(name = "create-reservation")]
    CreateReservation {
        /// Reservation name
        #[arg(long)]
        name: String,
        /// Start time (ISO 8601 or "now")
        #[arg(long, default_value = "now")]
        start_time: String,
        /// Duration in minutes
        #[arg(long)]
        duration: u32,
        /// Comma-separated node names
        #[arg(long)]
        nodes: String,
        /// Comma-separated accounts (optional)
        #[arg(long, default_value = "")]
        accounts: String,
        /// Comma-separated users (optional)
        #[arg(long, default_value = "")]
        users: String,
        /// Comma-separated flags (maint, ignore_jobs, no_hold_jobs)
        #[arg(long, default_value = "")]
        flags: String,
    },
    /// Update a reservation
    #[command(name = "update-reservation")]
    UpdateReservation {
        /// Reservation name
        #[arg(long)]
        name: String,
        /// New duration in minutes (0 = no change)
        #[arg(long, default_value = "0")]
        duration: u32,
        /// Comma-separated nodes to add
        #[arg(long, default_value = "")]
        add_nodes: String,
        /// Comma-separated nodes to remove
        #[arg(long, default_value = "")]
        remove_nodes: String,
        /// Comma-separated users to add
        #[arg(long, default_value = "")]
        add_users: String,
        /// Comma-separated users to remove
        #[arg(long, default_value = "")]
        remove_users: String,
        /// Comma-separated accounts to add
        #[arg(long, default_value = "")]
        add_accounts: String,
        /// Comma-separated accounts to remove
        #[arg(long, default_value = "")]
        remove_accounts: String,
    },
    /// Delete a reservation
    #[command(name = "delete-reservation")]
    DeleteReservation {
        /// Reservation name
        name: String,
    },
    /// Ping the controller
    Ping,
    /// Show version
    Version,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = ScontrolArgs::try_parse_from(&args)?;

    match args.command {
        ScontrolCommand::Show { entity, name } => {
            show(&args.controller, &entity, name.as_deref()).await
        }
        ScontrolCommand::Ping => ping(&args.controller).await,
        ScontrolCommand::Version => {
            println!("spur {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ScontrolCommand::Hold { job_id } => {
            send_job_update(
                &args.controller,
                spur_proto::proto::UpdateJobRequest {
                    job_id,
                    hold: Some(true),
                    ..Default::default()
                },
            )
            .await
        }
        ScontrolCommand::Release { job_id } => {
            send_job_update(
                &args.controller,
                spur_proto::proto::UpdateJobRequest {
                    job_id,
                    hold: Some(false),
                    ..Default::default()
                },
            )
            .await
        }
        ScontrolCommand::Requeue { job_id } => {
            // Requeue = cancel + resubmit, simplified for now
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .cancel_job(spur_proto::proto::CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: String::new(),
                })
                .await
                .context("requeue failed")?;
            println!("job {} requeued (cancelled for resubmission)", job_id);
            Ok(())
        }
        ScontrolCommand::Suspend { job_id } => {
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .suspend_job(spur_proto::proto::SuspendJobRequest {
                    job_id,
                    user: String::new(),
                })
                .await
                .context("suspend failed")?;
            println!("job {} suspended", job_id);
            Ok(())
        }
        ScontrolCommand::Resume { job_id } => {
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .resume_job(spur_proto::proto::ResumeJobRequest {
                    job_id,
                    user: String::new(),
                })
                .await
                .context("resume failed")?;
            println!("job {} resumed", job_id);
            Ok(())
        }
        ScontrolCommand::Update { params } => parse_and_update(&args.controller, &params).await,
        ScontrolCommand::CreateReservation {
            name,
            start_time,
            duration,
            nodes,
            accounts,
            users,
            flags,
        } => {
            create_reservation(
                &args.controller,
                &name,
                &start_time,
                duration,
                &nodes,
                &accounts,
                &users,
                &flags,
            )
            .await
        }
        ScontrolCommand::UpdateReservation {
            name,
            duration,
            add_nodes,
            remove_nodes,
            add_users,
            remove_users,
            add_accounts,
            remove_accounts,
        } => {
            let split_csv = |s: &str| -> Vec<String> {
                s.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            let mut client = SlurmControllerClient::connect(args.controller.to_string())
                .await
                .context("failed to connect to spurctld")?;
            client
                .update_reservation(spur_proto::proto::UpdateReservationRequest {
                    name: name.clone(),
                    duration_minutes: duration,
                    add_nodes: split_csv(&add_nodes),
                    remove_nodes: split_csv(&remove_nodes),
                    add_users: split_csv(&add_users),
                    remove_users: split_csv(&remove_users),
                    add_accounts: split_csv(&add_accounts),
                    remove_accounts: split_csv(&remove_accounts),
                })
                .await
                .context("failed to update reservation")?;
            println!("Reservation {} updated", name);
            Ok(())
        }
        ScontrolCommand::DeleteReservation { name } => {
            delete_reservation(&args.controller, &name).await
        }
    }
}

async fn show(controller: &str, entity: &str, name: Option<&str>) -> Result<()> {
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    match entity.to_lowercase().as_str() {
        "job" | "jobs" => {
            let job_ids = name
                .map(|n| vec![n.parse::<u32>().unwrap_or(0)])
                .unwrap_or_default();

            let resp = client
                .get_jobs(spur_proto::proto::GetJobsRequest {
                    job_ids,
                    ..Default::default()
                })
                .await
                .context("failed to get jobs")?;

            for job in resp.into_inner().jobs {
                println!("JobId={} JobName={}", job.job_id, job.name);
                println!("   UserId={} Account={}", job.user, job.account);
                println!("   Partition={} QOS={}", job.partition, job.qos);
                println!(
                    "   JobState={} Reason={}",
                    state_name(job.state),
                    render_reason(&job.state_reason, job.exit_signal),
                );
                println!(
                    "   NumNodes={} NumTasks={} CPUs/Task={}",
                    job.num_nodes, job.num_tasks, job.cpus_per_task
                );
                if !job.nodelist.is_empty() {
                    println!("   NodeList={}", job.nodelist);
                }
                println!(
                    "   SubmitTime={} StartTime={} EndTime={}",
                    format_ts(job.submit_time.as_ref()),
                    format_ts(job.start_time.as_ref()),
                    format_ts(job.end_time.as_ref()),
                );
                println!("   WorkDir={}", job.work_dir);
                println!("   StdOut={} StdErr={}", job.stdout_path, job.stderr_path);
                println!(
                    "   ExitCode={} DerivedExitCode={} Priority={}",
                    format_exit(job.exit_code, job.exit_signal),
                    format_exit(job.derived_exit_code, 0),
                    job.priority
                );
                println!();
            }
        }
        "node" | "nodes" => {
            let resp = client
                .get_nodes(spur_proto::proto::GetNodesRequest {
                    nodelist: name.unwrap_or("").into(),
                    ..Default::default()
                })
                .await
                .context("failed to get nodes")?;

            for node in resp.into_inner().nodes {
                let total = node.total_resources.as_ref();
                let alloc = node.alloc_resources.as_ref();
                println!("NodeName={}", node.name);
                println!(
                    "   State={} Reason={}",
                    node_state_name(node.state),
                    node.state_reason
                );
                if !node.partitions.is_empty() {
                    println!("   Partitions={}", node.partitions.join(","));
                }
                println!(
                    "   CPUTot={} CPUAlloc={} RealMemory={} FreeMem={}",
                    total.map(|r| r.cpus).unwrap_or(0),
                    alloc.map(|r| r.cpus).unwrap_or(0),
                    total.map(|r| r.memory_mb).unwrap_or(0),
                    node.free_memory_mb,
                );
                let gpus = total.map(|r| r.gpus.len()).unwrap_or(0);
                if gpus > 0 {
                    let gpu_types: Vec<String> = total
                        .unwrap()
                        .gpus
                        .iter()
                        .map(|g| format!("gpu:{}:1", g.gpu_type))
                        .collect();
                    println!("   Gres={}", gpu_types.join(","));
                }
                println!("   Arch={} OS={}", node.arch, node.os);
                if !node.labels.is_empty() {
                    let mut label_str: Vec<String> = node
                        .labels
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    label_str.sort();
                    println!("   Labels={}", label_str.join(","));
                }
                if !node.active_reservation.is_empty() {
                    println!("   ActiveReservation={}", node.active_reservation);
                }
                println!("   CpuLoad={}", node.cpu_load as f64 / 100.0);
                println!();
            }
        }
        "partition" | "partitions" => {
            let resp = client
                .get_partitions(spur_proto::proto::GetPartitionsRequest {
                    name: name.unwrap_or("").into(),
                })
                .await
                .context("failed to get partitions")?;

            for part in resp.into_inner().partitions {
                println!(
                    "PartitionName={}{}",
                    part.name,
                    if part.is_default { " Default=YES" } else { "" }
                );
                println!("   State={}", part.state);
                println!("   Nodes={}", part.nodes);
                println!(
                    "   TotalNodes={} TotalCPUs={}",
                    part.total_nodes, part.total_cpus
                );
                println!(
                    "   MaxTime={} DefaultTime={}",
                    part.max_time
                        .as_ref()
                        .map(|t| spur_core::config::format_time(Some((t.seconds / 60) as u32)))
                        .unwrap_or_else(|| "UNLIMITED".into()),
                    part.default_time
                        .as_ref()
                        .map(|t| spur_core::config::format_time(Some((t.seconds / 60) as u32)))
                        .unwrap_or_else(|| "UNLIMITED".into()),
                );
                println!("   PriorityTier={}", part.priority_tier);
                println!();
            }
        }
        "reservation" | "reservations" => {
            let resp = client
                .list_reservations(spur_proto::proto::ListReservationsRequest {})
                .await
                .context("failed to list reservations")?;

            for res in resp.into_inner().reservations {
                println!("ReservationName={}", res.name);
                println!("   StartTime={}", res.start_time);
                println!("   EndTime={}", res.end_time);
                println!("   Nodes={}", res.nodes);
                if !res.state.is_empty() {
                    println!("   State={}", res.state);
                }
                if !res.flags.is_empty() {
                    println!("   Flags={}", res.flags);
                }
                if !res.accounts.is_empty() {
                    println!("   Accounts={}", res.accounts);
                }
                if !res.users.is_empty() {
                    println!("   Users={}", res.users);
                }
                println!();
            }
        }
        "step" | "steps" => {
            let job_id: u32 = name
                .ok_or_else(|| anyhow::anyhow!("scontrol show steps: job_id required"))?
                .parse()
                .context("invalid job_id")?;

            let resp = client
                .get_job_steps(spur_proto::proto::GetJobStepsRequest { job_id })
                .await
                .context("failed to get job steps")?;

            let steps = resp.into_inner().steps;
            if steps.is_empty() {
                println!("No steps found for job {}", job_id);
            } else {
                for step in steps {
                    let step_name = if step.step_id == 0xFFFF_FFFE {
                        "batch".to_string()
                    } else if step.step_id == 0xFFFF_FFFD {
                        "extern".to_string()
                    } else {
                        step.step_id.to_string()
                    };
                    println!(
                        "StepId={}.{} StepName={} State={} NumTasks={}",
                        step.job_id, step_name, step.name, step.state, step.num_tasks
                    );
                }
            }
        }
        "config" => {
            println!("ClusterName=spur");
            println!("SlurmctldAddr={}", controller);
            println!("Version={}", env!("CARGO_PKG_VERSION"));
        }
        "federation" => {
            let resp = client.ping(()).await.context("failed to ping controller")?;

            let inner = resp.into_inner();
            if inner.federation_peers.is_empty() {
                println!("No federation peers configured.");
            } else {
                println!("FEDERATION PEERS");
                println!("{:<20} ADDRESS", "CLUSTER");
                for peer in &inner.federation_peers {
                    // Format is "name@address"
                    if let Some((name, addr)) = peer.split_once('@') {
                        println!("{:<20} {}", name, addr);
                    } else {
                        println!("{:<20} (unknown)", peer);
                    }
                }
            }
        }
        other => {
            bail!(
                "scontrol: unknown entity type '{}'. Use: job, node, partition, reservation, federation, config",
                other
            );
        }
    }

    Ok(())
}

async fn ping(controller: &str) -> Result<()> {
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    let resp = client.ping(()).await.context("ping failed")?;

    let inner = resp.into_inner();
    println!(
        "Slurmctld(primary) at {} is UP. Version={}",
        inner.hostname, inner.version
    );

    Ok(())
}

fn state_name(state: i32) -> &'static str {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display())
        .unwrap_or("UNKNOWN")
}

fn node_state_name(state: i32) -> &'static str {
    spur_core::node::NodeState::from_proto_i32(state)
        .map(|s| s.display_upper())
        .unwrap_or("UNKNOWN")
}

fn format_ts(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "N/A".into(),
    }
}

async fn send_job_update(controller: &str, req: spur_proto::proto::UpdateJobRequest) -> Result<()> {
    let hold = req.hold;
    let job_id = req.job_id;
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    client.update_job(req).await.context("update failed")?;

    if hold == Some(true) {
        println!("job {} held", job_id);
    } else if hold == Some(false) {
        println!("job {} released", job_id);
    } else {
        println!("job {} updated", job_id);
    }
    Ok(())
}

/// Parse "key=value" params from `scontrol update` command.
async fn parse_and_update(controller: &str, params: &[String]) -> Result<()> {
    let mut job_id: Option<u32> = None;
    let mut priority: Option<u32> = None;
    let mut time_limit: Option<String> = None;
    let mut partition: Option<String> = None;
    let mut account: Option<String> = None;
    let mut comment: Option<String> = None;
    let mut qos: Option<String> = None;

    // Node update fields
    let mut node_name: Option<String> = None;
    let mut node_state: Option<String> = None;
    let mut node_reason: Option<String> = None;

    for param in params {
        if let Some((key, value)) = param.split_once('=') {
            match key.to_lowercase().as_str() {
                "jobid" | "job" => job_id = value.parse().ok(),
                "priority" => priority = value.parse().ok(),
                "timelimit" | "time_limit" => time_limit = Some(value.into()),
                "partition" => partition = Some(value.into()),
                "account" => account = Some(value.into()),
                "comment" => comment = Some(value.into()),
                "qos" => qos = Some(value.into()),
                "nodename" | "node" => node_name = Some(value.into()),
                "state" => node_state = Some(value.into()),
                "reason" => node_reason = Some(value.into()),
                other => eprintln!("scontrol: unknown update key '{}'", other),
            }
        }
    }

    // Node update takes priority if NodeName is specified
    if let Some(name) = node_name {
        return update_node(controller, &name, node_state.as_deref(), node_reason).await;
    }

    let jid =
        job_id.ok_or_else(|| anyhow::anyhow!("scontrol update: JobId= or NodeName= required"))?;

    let tl = time_limit.as_ref().and_then(|t| {
        spur_core::config::parse_time_minutes(t).map(|m| prost_types::Duration {
            seconds: m as i64 * 60,
            nanos: 0,
        })
    });

    send_job_update(
        controller,
        spur_proto::proto::UpdateJobRequest {
            job_id: jid,
            priority,
            time_limit: tl,
            partition,
            account,
            comment,
            qos,
            ..Default::default()
        },
    )
    .await
}

/// Update a node's state via the controller.
async fn update_node(
    controller: &str,
    name: &str,
    state: Option<&str>,
    reason: Option<String>,
) -> Result<()> {
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    let proto_state = state.map(|s| match s.to_lowercase().as_str() {
        "idle" | "resume" => spur_proto::proto::NodeState::NodeIdle as i32,
        "drain" => spur_proto::proto::NodeState::NodeDrain as i32,
        "down" => spur_proto::proto::NodeState::NodeDown as i32,
        other => {
            eprintln!(
                "scontrol: unknown node state '{}', defaulting to idle",
                other
            );
            spur_proto::proto::NodeState::NodeIdle as i32
        }
    });

    client
        .update_node(spur_proto::proto::UpdateNodeRequest {
            name: name.to_string(),
            state: proto_state,
            reason,
            labels: HashMap::new(),
            remove_labels: Vec::new(),
        })
        .await
        .context("node update failed")?;

    println!("node {} updated", name);
    Ok(())
}

/// Create a reservation via the controller.
#[allow(clippy::too_many_arguments)]
async fn create_reservation(
    controller: &str,
    name: &str,
    start_time: &str,
    duration: u32,
    nodes: &str,
    accounts: &str,
    users: &str,
    flags: &str,
) -> Result<()> {
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    let node_list: Vec<String> = nodes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let account_list: Vec<String> = accounts
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let user_list: Vec<String> = users
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let flag_list: Vec<String> = flags
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    client
        .create_reservation(spur_proto::proto::CreateReservationRequest {
            name: name.to_string(),
            start_time: start_time.to_string(),
            duration_minutes: duration,
            nodes: node_list,
            accounts: account_list,
            users: user_list,
            flags: flag_list,
        })
        .await
        .context("failed to create reservation")?;

    println!("Reservation {} created", name);
    Ok(())
}

/// Delete a reservation via the controller.
async fn delete_reservation(controller: &str, name: &str) -> Result<()> {
    let mut client = SlurmControllerClient::connect(controller.to_string())
        .await
        .context("failed to connect to spurctld")?;

    client
        .delete_reservation(spur_proto::proto::DeleteReservationRequest {
            name: name.to_string(),
        })
        .await
        .context("failed to delete reservation")?;

    println!("Reservation {} deleted", name);
    Ok(())
}
