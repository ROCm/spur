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
    /// Create a partition
    #[command(name = "create-partition")]
    CreatePartition {
        /// Partition name
        #[arg(long)]
        name: String,
        /// Hostlist of nodes (mutually exclusive with --selector)
        #[arg(long, default_value = "")]
        nodes: String,
        /// Label selector as KEY=VALUE pairs, comma-separated (mutually exclusive with --nodes)
        #[arg(long, default_value = "")]
        selector: String,
        /// Partition state: UP (default), DOWN, DRAIN, INACTIVE
        #[arg(long, default_value = "UP")]
        state: String,
        /// Mark as the cluster default partition
        #[arg(long)]
        default: bool,
        /// Maximum job wall-clock time (e.g. "24:00:00" or "INFINITE")
        #[arg(long, default_value = "")]
        max_time: String,
        /// Default job wall-clock time (e.g. "01:00:00")
        #[arg(long, default_value = "")]
        default_time: String,
        /// Maximum number of nodes per job
        #[arg(long)]
        max_nodes: Option<u32>,
        /// Minimum number of nodes per job
        #[arg(long, default_value = "1")]
        min_nodes: u32,
        /// Comma-separated accounts allowed (empty = all)
        #[arg(long, default_value = "")]
        allow_accounts: String,
        /// Comma-separated groups allowed (empty = all)
        #[arg(long, default_value = "")]
        allow_groups: String,
        /// Comma-separated accounts denied
        #[arg(long, default_value = "")]
        deny_accounts: String,
        /// Comma-separated QoS names denied
        #[arg(long, default_value = "")]
        deny_qos: String,
        /// Scheduling priority tier
        #[arg(long, default_value = "1")]
        priority_tier: u32,
        /// Preemption mode: OFF (default), CANCEL, REQUEUE, SUSPEND
        #[arg(long, default_value = "OFF")]
        preempt_mode: String,
    },
    /// Update a partition
    #[command(name = "update-partition")]
    UpdatePartition {
        /// Partition name to update
        #[arg(long)]
        name: String,
        /// New hostlist of nodes
        #[arg(long)]
        nodes: Option<String>,
        /// New label selector as KEY=VALUE pairs, comma-separated
        #[arg(long)]
        selector: Option<String>,
        /// New partition state: UP, DOWN, DRAIN, INACTIVE
        #[arg(long)]
        state: Option<String>,
        /// Set as the cluster default partition
        #[arg(long)]
        default: Option<bool>,
        /// New maximum job wall-clock time ("INFINITE" to clear)
        #[arg(long)]
        max_time: Option<String>,
        /// New default job wall-clock time
        #[arg(long)]
        default_time: Option<String>,
        /// New maximum nodes per job (0 = clear limit)
        #[arg(long)]
        max_nodes: Option<u32>,
        /// Clear the maximum nodes limit
        #[arg(long)]
        clear_max_nodes: bool,
        /// New minimum nodes per job
        #[arg(long)]
        min_nodes: Option<u32>,
        /// Replace allowed-accounts list (comma-separated; requires --set-allow-accounts)
        #[arg(long, default_value = "")]
        allow_accounts: String,
        /// Apply the --allow-accounts value (even if empty, to clear the list)
        #[arg(long)]
        set_allow_accounts: bool,
        /// Replace allowed-groups list (comma-separated; requires --set-allow-groups)
        #[arg(long, default_value = "")]
        allow_groups: String,
        /// Apply the --allow-groups value (even if empty, to clear the list)
        #[arg(long)]
        set_allow_groups: bool,
        /// Replace denied-accounts list (comma-separated; requires --set-deny-accounts)
        #[arg(long, default_value = "")]
        deny_accounts: String,
        /// Apply the --deny-accounts value
        #[arg(long)]
        set_deny_accounts: bool,
        /// Replace denied-QoS list (comma-separated; requires --set-deny-qos)
        #[arg(long, default_value = "")]
        deny_qos: String,
        /// Apply the --deny-qos value
        #[arg(long)]
        set_deny_qos: bool,
        /// New priority tier
        #[arg(long)]
        priority_tier: Option<u32>,
        /// New preemption mode: OFF, CANCEL, REQUEUE, SUSPEND
        #[arg(long)]
        preempt_mode: Option<String>,
    },
    /// Delete a partition
    #[command(name = "delete-partition")]
    DeletePartition {
        /// Partition name
        #[arg(long)]
        name: String,
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
        /// Comma-separated flags (maint, ignore_jobs, no_hold_jobs, overlap)
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
            let channel = spur_client::connect_channel(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = SlurmControllerClient::new(channel);
            client
                .cancel_job(spur_proto::proto::CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: whoami::username().unwrap_or_else(|_| "unknown".into()),
                })
                .await
                .context("requeue failed")?;
            println!("job {} requeued (cancelled for resubmission)", job_id);
            Ok(())
        }
        ScontrolCommand::Suspend { job_id } => {
            let channel = spur_client::connect_channel(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = SlurmControllerClient::new(channel);
            client
                .suspend_job(spur_proto::proto::SuspendJobRequest {
                    job_id,
                    user: whoami::username().unwrap_or_else(|_| "unknown".into()),
                })
                .await
                .context("suspend failed")?;
            println!("job {} suspended", job_id);
            Ok(())
        }
        ScontrolCommand::Resume { job_id } => {
            let channel = spur_client::connect_channel(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = SlurmControllerClient::new(channel);
            client
                .resume_job(spur_proto::proto::ResumeJobRequest {
                    job_id,
                    user: whoami::username().unwrap_or_else(|_| "unknown".into()),
                })
                .await
                .context("resume failed")?;
            println!("job {} resumed", job_id);
            Ok(())
        }
        ScontrolCommand::Update { params } => parse_and_update(&args.controller, &params).await,
        ScontrolCommand::CreatePartition {
            name,
            nodes,
            selector,
            state,
            default,
            max_time,
            default_time,
            max_nodes,
            min_nodes,
            allow_accounts,
            allow_groups,
            deny_accounts,
            deny_qos,
            priority_tier,
            preempt_mode,
        } => {
            create_partition(
                &args.controller,
                &name,
                &nodes,
                &selector,
                &state,
                default,
                &max_time,
                &default_time,
                max_nodes,
                min_nodes,
                &allow_accounts,
                &allow_groups,
                &deny_accounts,
                &deny_qos,
                priority_tier,
                &preempt_mode,
            )
            .await
        }
        ScontrolCommand::UpdatePartition {
            name,
            nodes,
            selector,
            state,
            default,
            max_time,
            default_time,
            max_nodes,
            clear_max_nodes,
            min_nodes,
            allow_accounts,
            allow_groups,
            set_allow_accounts,
            set_allow_groups,
            deny_accounts,
            deny_qos,
            set_deny_accounts,
            set_deny_qos,
            priority_tier,
            preempt_mode,
        } => {
            update_partition(
                &args.controller,
                &name,
                nodes,
                selector,
                state,
                default,
                max_time,
                default_time,
                max_nodes,
                clear_max_nodes,
                min_nodes,
                if set_allow_accounts { Some(&allow_accounts) } else { None },
                if set_allow_groups { Some(&allow_groups) } else { None },
                set_allow_accounts,
                set_allow_groups,
                if set_deny_accounts { Some(&deny_accounts) } else { None },
                if set_deny_qos { Some(&deny_qos) } else { None },
                set_deny_accounts,
                set_deny_qos,
                priority_tier,
                preempt_mode,
            )
            .await
        }
        ScontrolCommand::DeletePartition { name } => {
            delete_partition(&args.controller, &name).await
        }
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
            let channel = spur_client::connect_channel(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = SlurmControllerClient::new(channel);
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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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
                print!("   StdOut={} StdErr={}", job.stdout_path, job.stderr_path);
                if !job.stdin_path.is_empty() {
                    print!(" StdIn={}", job.stdin_path);
                }
                println!();
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
                println!(
                    "   AllowGroups={} AllowAccounts={} AllowQos={}",
                    if part.allow_groups.is_empty() { "ALL".into() } else { part.allow_groups.clone() },
                    if part.allow_accounts.is_empty() { "ALL".into() } else { part.allow_accounts.clone() },
                    if part.allow_qos.is_empty() { "ALL".into() } else { part.allow_qos.clone() },
                );
                if !part.deny_accounts.is_empty() {
                    println!("   DenyAccounts={}", part.deny_accounts);
                }
                if !part.deny_qos.is_empty() {
                    println!("   DenyQos={}", part.deny_qos);
                }
                println!("   State={}", part.state.to_uppercase());
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
                println!(
                    "   MinNodes={} MaxNodes={}",
                    part.min_nodes,
                    if part.max_nodes == 0 { "UNLIMITED".into() } else { part.max_nodes.to_string() },
                );
                println!(
                    "   PreemptMode={} PriorityTier={}",
                    part.preempt_mode.to_uppercase(),
                    part.priority_tier
                );
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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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

/// Parse "KEY=VALUE,KEY2=VALUE2" into a HashMap.
fn parse_selector(s: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').context(format!(
            "selector entry '{}' is not in KEY=VALUE format",
            pair
        ))?;
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// Create a partition via the controller.
#[allow(clippy::too_many_arguments)]
async fn create_partition(
    controller: &str,
    name: &str,
    nodes: &str,
    selector: &str,
    state: &str,
    is_default: bool,
    max_time: &str,
    default_time: &str,
    max_nodes: Option<u32>,
    min_nodes: u32,
    allow_accounts: &str,
    allow_groups: &str,
    deny_accounts: &str,
    deny_qos: &str,
    priority_tier: u32,
    preempt_mode: &str,
) -> Result<()> {
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

    let split_csv = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    client
        .create_partition(spur_proto::proto::CreatePartitionRequest {
            name: name.to_string(),
            nodes: nodes.to_string(),
            selector: parse_selector(selector)?,
            state: state.to_string(),
            is_default,
            max_time: max_time.to_string(),
            default_time: default_time.to_string(),
            max_nodes,
            min_nodes,
            allow_accounts: split_csv(allow_accounts),
            allow_groups: split_csv(allow_groups),
            deny_accounts: split_csv(deny_accounts),
            deny_qos: split_csv(deny_qos),
            priority_tier,
            preempt_mode: preempt_mode.to_string(),
        })
        .await
        .context("failed to create partition")?;

    println!("Partition {} created", name);
    Ok(())
}

/// Update a partition via the controller.
#[allow(clippy::too_many_arguments)]
async fn update_partition(
    controller: &str,
    name: &str,
    nodes: Option<String>,
    selector: Option<String>,
    state: Option<String>,
    is_default: Option<bool>,
    max_time: Option<String>,
    default_time: Option<String>,
    max_nodes: Option<u32>,
    clear_max_nodes: bool,
    min_nodes: Option<u32>,
    allow_accounts: Option<&str>,
    allow_groups: Option<&str>,
    set_allow_accounts: bool,
    set_allow_groups: bool,
    deny_accounts: Option<&str>,
    deny_qos: Option<&str>,
    set_deny_accounts: bool,
    set_deny_qos: bool,
    priority_tier: Option<u32>,
    preempt_mode: Option<String>,
) -> Result<()> {
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

    let split_csv = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let selector_map = if let Some(ref s) = selector {
        parse_selector(s)?
    } else {
        HashMap::new()
    };

    client
        .update_partition(spur_proto::proto::UpdatePartitionRequest {
            name: name.to_string(),
            nodes,
            selector: selector_map,
            state,
            is_default,
            max_time,
            default_time,
            max_nodes_value: max_nodes,
            clear_max_nodes,
            min_nodes,
            allow_accounts: allow_accounts.map(split_csv).unwrap_or_default(),
            set_allow_accounts,
            allow_groups: allow_groups.map(split_csv).unwrap_or_default(),
            set_allow_groups,
            deny_accounts: deny_accounts.map(split_csv).unwrap_or_default(),
            set_deny_accounts,
            deny_qos: deny_qos.map(split_csv).unwrap_or_default(),
            set_deny_qos,
            priority_tier,
            preempt_mode,
        })
        .await
        .context("failed to update partition")?;

    println!("Partition {} updated", name);
    Ok(())
}

/// Delete a partition via the controller.
async fn delete_partition(controller: &str, name: &str) -> Result<()> {
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

    client
        .delete_partition(spur_proto::proto::DeletePartitionRequest {
            name: name.to_string(),
        })
        .await
        .context("failed to delete partition")?;

    println!("Partition {} deleted", name);
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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

    client
        .delete_reservation(spur_proto::proto::DeleteReservationRequest {
            name: name.to_string(),
        })
        .await
        .context("failed to delete reservation")?;

    println!("Reservation {} deleted", name);
    Ok(())
}
