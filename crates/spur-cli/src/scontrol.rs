// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use crate::exit_fmt::{format_exit, render_reason};
use crate::timefmt::format_timestamp as format_ts;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;

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
    /// Create a partition or reservation (Slurm-compatible inline syntax)
    ///
    /// Examples:
    ///   scontrol create PartitionName=gpu Nodes=n[1-4] MaxTime=24:00:00 State=UP
    ///   scontrol create ReservationName=maint StartTime=now Duration=60 Nodes=n1
    Create {
        /// key=value pairs (e.g. PartitionName=gpu Nodes=n1 MaxTime=4:00:00)
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Update job/node/partition properties (Slurm-compatible inline syntax)
    ///
    /// Examples:
    ///   scontrol update PartitionName=gpu MaxTime=48:00:00 State=DOWN
    ///   scontrol update JobId=42 Priority=100
    ///   scontrol update NodeName=n1 State=drain Reason=maintenance
    Update {
        /// key=value pairs
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Delete a partition or reservation (Slurm-compatible inline syntax)
    ///
    /// Examples:
    ///   scontrol delete PartitionName=gpu
    ///   scontrol delete ReservationName=maint
    Delete {
        /// key=value pairs (e.g. PartitionName=gpu)
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
    /// Requeue a job (return it to PENDING with the same spec)
    Requeue {
        /// Job ID
        job_id: u32,
    },
    /// Requeue a job and leave it in a held state
    #[command(name = "requeuehold", alias = "requeue-hold")]
    RequeueHold {
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
        /// Hostlist of nodes; a node matches if it satisfies this OR --selector
        #[arg(long, default_value = "")]
        nodes: String,
        /// Label selector as KEY=VALUE pairs, comma-separated; a node matches if it satisfies this OR --nodes
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
        /// Comma-separated QoS names allowed (empty = all)
        #[arg(long, default_value = "")]
        allow_qos: String,
        /// Scheduling priority tier
        #[arg(long, default_value = "1")]
        priority_tier: u32,
        /// Preemption mode: OFF (default), CANCEL, REQUEUE, SUSPEND
        #[arg(long, default_value = "OFF")]
        preempt_mode: String,
        /// Minimum seconds a job must run before it is eligible for preemption
        /// (0 = immediately preemptable; omit to use the global default)
        #[arg(long)]
        preempt_exempt_time: Option<u32>,
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
        /// Clear the label selector
        #[arg(long)]
        clear_selector: bool,
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
        /// Replace allowed-QoS list (comma-separated; requires --set-allow-qos)
        #[arg(long, default_value = "")]
        allow_qos: String,
        /// Apply the --allow-qos value (even if empty, to clear the list)
        #[arg(long)]
        set_allow_qos: bool,
        /// New priority tier
        #[arg(long)]
        priority_tier: Option<u32>,
        /// New preemption mode: OFF, CANCEL, REQUEUE, SUSPEND
        #[arg(long)]
        preempt_mode: Option<String>,
        /// Minimum seconds a job must have been running before it is eligible for
        /// preemption. 0 is a valid value (immediately preemptable).
        #[arg(long)]
        preempt_exempt_time: Option<u32>,
        /// Clear the partition's preempt_exempt_time override, reverting to the
        /// global scheduler.preempt_exempt_time default.
        #[arg(long)]
        clear_preempt_exempt_time: bool,
    },
    /// Delete a partition
    #[command(name = "delete-partition")]
    DeletePartition {
        /// Partition name
        #[arg(long)]
        name: String,
    },
    /// Re-read spur.conf and apply it live on the leader; followers converge on
    /// restart. Not every field is reloadable — see docs/admin-guide/configuration.rst,
    /// "Applying configuration changes", for the per-field reload scope.
    Reconfigure,
    /// Create a reservation
    #[command(name = "create-reservation")]
    CreateReservation {
        /// Reservation name
        #[arg(long)]
        name: String,
        /// Start time (ISO 8601 or "now")
        #[arg(long, default_value = "now")]
        start_time: String,
        /// Duration: whole minutes, Slurm time (H:MM, H:MM:SS, D-HH:MM:SS), or suffixed (90m, 1h30m, 30s); UNLIMITED/INFINITE not supported
        #[arg(long)]
        duration: String,
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
        /// New duration: whole minutes, Slurm time (01:00:00, 30-00:00:00), or suffixed (90m, 1h30m); any zero-length value (e.g. 0, 00:00:00) leaves it unchanged; UNLIMITED/INFINITE not supported
        #[arg(long, default_value = "0")]
        duration: String,
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
                    user: crate::interactive::current_user()?,
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
                    user: crate::interactive::current_user()?,
                    ..Default::default()
                },
            )
            .await
        }
        ScontrolCommand::Requeue { job_id } => requeue(&args.controller, job_id, false).await,
        ScontrolCommand::RequeueHold { job_id } => requeue(&args.controller, job_id, true).await,
        ScontrolCommand::Suspend { job_id } => {
            let channel = crate::authclient::connect(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = spur_proto::controller_client(channel);
            client
                .suspend_job(spur_proto::proto::SuspendJobRequest {
                    job_id,
                    user: crate::interactive::current_user()?,
                })
                .await
                .context("suspend failed")?;
            println!("job {} suspended", job_id);
            Ok(())
        }
        ScontrolCommand::Resume { job_id } => {
            let channel = crate::authclient::connect(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = spur_proto::controller_client(channel);
            client
                .resume_job(spur_proto::proto::ResumeJobRequest {
                    job_id,
                    user: crate::interactive::current_user()?,
                })
                .await
                .context("resume failed")?;
            println!("job {} resumed", job_id);
            Ok(())
        }
        ScontrolCommand::Create { params } => parse_and_create(&args.controller, &params).await,
        ScontrolCommand::Update { params } => parse_and_update(&args.controller, &params).await,
        ScontrolCommand::Delete { params } => parse_and_delete(&args.controller, &params).await,
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
            allow_qos,
            priority_tier,
            preempt_mode,
            preempt_exempt_time,
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
                &allow_qos,
                priority_tier,
                &preempt_mode,
                preempt_exempt_time,
            )
            .await
        }
        ScontrolCommand::UpdatePartition {
            name,
            nodes,
            selector,
            clear_selector,
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
            allow_qos,
            set_allow_qos,
            priority_tier,
            preempt_mode,
            preempt_exempt_time,
            clear_preempt_exempt_time,
        } => {
            let selector_map = match selector {
                Some(ref s) => parse_selector(s)?,
                None => HashMap::new(),
            };
            let req = spur_proto::proto::UpdatePartitionRequest {
                name,
                nodes,
                selector: selector_map,
                set_selector: clear_selector || selector.is_some(),
                state,
                is_default: default,
                max_time,
                default_time,
                max_nodes_value: max_nodes,
                clear_max_nodes,
                min_nodes,
                allow_accounts: split_csv(&allow_accounts),
                set_allow_accounts,
                allow_groups: split_csv(&allow_groups),
                set_allow_groups,
                deny_accounts: split_csv(&deny_accounts),
                set_deny_accounts,
                deny_qos: split_csv(&deny_qos),
                set_deny_qos,
                allow_qos: split_csv(&allow_qos),
                set_allow_qos,
                priority_tier,
                preempt_mode,
                preempt_exempt_time,
                clear_preempt_exempt_time,
            };
            update_partition(&args.controller, req).await
        }
        ScontrolCommand::DeletePartition { name } => {
            delete_partition(&args.controller, &name).await
        }
        ScontrolCommand::Reconfigure => reconfigure(&args.controller).await,
        ScontrolCommand::CreateReservation {
            name,
            start_time,
            duration,
            nodes,
            accounts,
            users,
            flags,
        } => {
            crate::privilege::require_privileged("manage reservations")?;
            let duration = parse_reservation_duration(&duration)?;
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
            crate::privilege::require_privileged("manage reservations")?;

            // Any zero-length value (default "0", or an explicit "00:00:00" etc.)
            // resolves to 0, which the controller treats as "leave duration
            // unchanged"; there is no zero-length reservation to set.
            let duration = parse_reservation_duration(&duration)?;
            let channel = crate::authclient::connect(&args.controller)
                .await
                .context("failed to connect to spurctld")?;
            let mut client = spur_proto::controller_client(channel);
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
                    user: crate::interactive::current_user()?,
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
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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
                print!("{}", format_job_detail(&job));
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
                    node_state_display(&node),
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
                if let Some(line) = planned_reservation_line(&node) {
                    println!("   {line}");
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
                    if part.allow_groups.is_empty() {
                        "ALL".into()
                    } else {
                        part.allow_groups.clone()
                    },
                    if part.allow_accounts.is_empty() {
                        "ALL".into()
                    } else {
                        part.allow_accounts.clone()
                    },
                    if part.allow_qos.is_empty() {
                        "ALL".into()
                    } else {
                        part.allow_qos.clone()
                    },
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
                    if part.max_nodes == 0 {
                        "UNLIMITED".into()
                    } else {
                        part.max_nodes.to_string()
                    },
                );
                print!(
                    "   PreemptMode={} PriorityTier={}",
                    part.preempt_mode.to_uppercase(),
                    part.priority_tier
                );
                if let Some(t) = part.preempt_exempt_time {
                    if t > 0 {
                        print!(" PreemptExemptTime={t}");
                    }
                }
                println!();
                println!();
            }
        }
        "reservation" | "reservations" => {
            let name = normalize_show_name(name);
            let resp = client
                .list_reservations(spur_proto::proto::ListReservationsRequest {
                    name: name.unwrap_or("").into(),
                })
                .await
                .context("failed to list reservations")?;

            let reservations = resp.into_inner().reservations;
            if reservations.is_empty() {
                if let Some(name) = name {
                    bail!("Reservation {name} not found");
                }
                return Ok(());
            }

            for res in reservations {
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
                if !res.owner.is_empty() {
                    println!("   Owner={}", res.owner);
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
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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

/// `State=` value for `scontrol show node`, composited the way real Slurm
/// does (`IDLE+RESERVED`, `IDLE+MAINTENANCE+RESERVED`, `IDLE+PLANNED`).
fn node_state_display(node: &spur_proto::proto::NodeInfo) -> String {
    let base = node_state_name(node.state);
    match spur_core::node::node_overlay(node) {
        Some(spur_core::node::NodeOverlay::Reserved { maint: true }) => {
            format!("{base}+MAINTENANCE+RESERVED")
        }
        Some(spur_core::node::NodeOverlay::Reserved { maint: false }) => {
            format!("{base}+RESERVED")
        }
        Some(spur_core::node::NodeOverlay::Planned) => format!("{base}+PLANNED"),
        None => base.to_string(),
    }
}

/// `PlannedJobId=/PlannedStartTime=` line for `scontrol show node` (`None` if
/// unset, `N/A` start if unknown); server only sets it while Idle, like sinfo's `plnd`.
fn planned_reservation_line(node: &spur_proto::proto::NodeInfo) -> Option<String> {
    if node.planned_job_id == 0 {
        return None;
    }
    Some(format!(
        "PlannedJobId={} PlannedStartTime={}",
        node.planned_job_id,
        format_ts(node.planned_start.as_ref()),
    ))
}

async fn requeue(controller: &str, job_id: u32, hold: bool) -> Result<()> {
    let channel = spur_client::connect_channel(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);
    let resp = client
        .requeue_job(spur_proto::proto::RequeueJobRequest {
            job_id,
            user: crate::interactive::current_user()?,
            hold,
        })
        .await
        .context("requeue failed")?
        .into_inner();
    let held = if hold { " and held" } else { "" };
    // A single job (or non-array) requeues exactly one record; only an array
    // fan-out reports a count and any skipped tasks.
    if resp.requeued <= 1 && resp.skipped.is_empty() {
        println!("job {} requeued{}", job_id, held);
    } else {
        println!("requeued {} task(s){}", resp.requeued, held);
        for skipped in &resp.skipped {
            eprintln!("scontrol: skipped {}", skipped);
        }
    }
    Ok(())
}

async fn send_job_update(controller: &str, req: spur_proto::proto::UpdateJobRequest) -> Result<()> {
    let hold = req.hold;
    let job_id = req.job_id;
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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

/// The entity a Slurm-style `scontrol` key=value command targets.
#[derive(Debug, PartialEq, Eq)]
enum ScontrolEntity {
    Partition,
    Reservation,
}

/// Detect the target entity by scanning all params for `PartitionName=` /
/// `ReservationName=`. Slurm key=value syntax is order-independent, so the
/// marker may appear anywhere. Errors if both markers are present; returns
/// `None` when neither is (the caller picks the default, e.g. job/node update).
fn detect_entity(params: &[String]) -> Result<Option<ScontrolEntity>> {
    let present = |marker: &str| {
        params.iter().any(|p| {
            p.split_once('=')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case(marker))
        })
    };
    match (present("PartitionName"), present("ReservationName")) {
        (true, true) => {
            anyhow::bail!("scontrol: specify only one of PartitionName= or ReservationName=")
        }
        (true, false) => Ok(Some(ScontrolEntity::Partition)),
        (false, true) => Ok(Some(ScontrolEntity::Reservation)),
        (false, false) => Ok(None),
    }
}

/// Parse key=value pairs from a Slurm-style `scontrol create` command and
/// dispatch to the appropriate create handler (partition or reservation).
async fn parse_and_create(controller: &str, params: &[String]) -> Result<()> {
    match detect_entity(params)? {
        Some(ScontrolEntity::Partition) => parse_and_create_partition(controller, params).await,
        Some(ScontrolEntity::Reservation) => parse_and_create_reservation(controller, params).await,
        None => anyhow::bail!(
            "scontrol create: expected PartitionName=<name> or ReservationName=<name>"
        ),
    }
}

/// Parse Slurm key=value pairs and call create_partition.
///
/// Slurm keys (case-insensitive): PartitionName, Nodes, State, Default,
/// MaxTime, DefaultTime, MaxNodes, MinNodes, AllowAccounts, AllowGroups,
/// AllowQos, DenyAccounts, DenyQos, PriorityTier, PreemptMode.
async fn parse_and_create_partition(controller: &str, params: &[String]) -> Result<()> {
    let mut name = String::new();
    let mut nodes = String::new();
    let mut state = "UP".to_string();
    let mut is_default = false;
    let mut max_time = String::new();
    let mut default_time = String::new();
    let mut max_nodes: Option<u32> = None;
    let mut min_nodes: u32 = 1;
    let mut allow_accounts = String::new();
    let mut allow_groups = String::new();
    let mut allow_qos = String::new();
    let mut deny_accounts = String::new();
    let mut deny_qos = String::new();
    let mut priority_tier: u32 = 1;
    let mut preempt_mode = "OFF".to_string();
    let mut preempt_exempt_time: Option<u32> = None;

    for param in params {
        if let Some((key, value)) = param.split_once('=') {
            match key.to_lowercase().as_str() {
                "partitionname" => name = value.into(),
                "nodes" => nodes = value.into(),
                "state" => state = value.to_uppercase(),
                "default" => is_default = value.eq_ignore_ascii_case("yes"),
                "maxtime" => max_time = value.into(),
                "defaulttime" => default_time = value.into(),
                "maxnodes" => max_nodes = value.parse().ok(),
                "minnodes" => min_nodes = value.parse().unwrap_or(1),
                "allowaccounts" => allow_accounts = value.into(),
                "allowgroups" => allow_groups = value.into(),
                "allowqos" => allow_qos = value.into(),
                "denyaccounts" => deny_accounts = value.into(),
                "denyqos" => deny_qos = value.into(),
                "prioritytier" | "priorityjobfactor" => priority_tier = value.parse().unwrap_or(1),
                "preemptmode" => preempt_mode = value.to_uppercase(),
                "preemptexempttime" => {
                    preempt_exempt_time = Some(value.parse::<u32>().map_err(|_| {
                        anyhow::anyhow!("invalid value for PreemptExemptTime=: '{value}'")
                    })?);
                }
                // silently ignore Slurm-only keys that don't map to spur fields
                "allocnodes" | "hidden" | "rootonly" | "reqresv" | "oversubscribe"
                | "overtimelimit" | "gracetime" | "disablerootjobs" | "exclusiveuser"
                | "exclusivetopo" | "lln" | "maxcpuspernode" | "maxcpuspersocket"
                | "jobdefaults" | "defmempernode" | "maxmempernode" | "qos" | "tres" => {}
                other => eprintln!("scontrol create partition: unknown key '{}'", other),
            }
        }
    }

    if name.is_empty() {
        anyhow::bail!("scontrol create: PartitionName= is required");
    }

    create_partition(
        controller,
        &name,
        &nodes,
        "",
        &state,
        is_default,
        &max_time,
        &default_time,
        max_nodes,
        min_nodes,
        &allow_accounts,
        &allow_groups,
        &deny_accounts,
        &deny_qos,
        &allow_qos,
        priority_tier,
        &preempt_mode,
        preempt_exempt_time,
    )
    .await?;

    Ok(())
}

/// Parsed inputs for a Slurm-inline `scontrol create ReservationName=...`.
#[derive(Debug)]
struct ReservationCreateParams {
    name: String,
    start_time: String,
    duration_minutes: u32,
    nodes: String,
    accounts: String,
    users: String,
    flags: String,
}

/// Pure parse+validate of Slurm key=value pairs, split from the privileged
/// network call so the required-field and duration rules are unit-testable.
fn parse_reservation_create_params(params: &[String]) -> Result<ReservationCreateParams> {
    let mut name = String::new();
    let mut start_time = "now".to_string();
    let mut duration: Option<String> = None;
    let mut nodes = String::new();
    let mut accounts = String::new();
    let mut users = String::new();
    let mut flags = String::new();

    for param in params {
        if let Some((key, value)) = param.split_once('=') {
            match key.to_lowercase().as_str() {
                "reservationname" => name = value.into(),
                "starttime" => start_time = value.into(),
                "duration" => duration = Some(value.into()),
                "nodes" => nodes = value.into(),
                "accounts" => accounts = value.into(),
                "users" => users = value.into(),
                "flags" => flags = value.into(),
                other => eprintln!("scontrol create reservation: unknown key '{}'", other),
            }
        }
    }

    if name.is_empty() {
        anyhow::bail!("scontrol create: ReservationName= is required");
    }

    let duration_minutes = match duration {
        Some(d) => parse_reservation_duration(&d)?,
        None => anyhow::bail!("scontrol create: Duration= is required"),
    };

    Ok(ReservationCreateParams {
        name,
        start_time,
        duration_minutes,
        nodes,
        accounts,
        users,
        flags,
    })
}

/// Parse Slurm key=value pairs and call create_reservation.
async fn parse_and_create_reservation(controller: &str, params: &[String]) -> Result<()> {
    crate::privilege::require_privileged("manage reservations")?;

    let p = parse_reservation_create_params(params)?;
    create_reservation(
        controller,
        &p.name,
        &p.start_time,
        p.duration_minutes,
        &p.nodes,
        &p.accounts,
        &p.users,
        &p.flags,
    )
    .await
}

/// Parse key=value pairs from a Slurm-style `scontrol delete` command and
/// dispatch to the appropriate delete handler.
async fn parse_and_delete(controller: &str, params: &[String]) -> Result<()> {
    let value_for = |marker: &str| {
        params.iter().find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.eq_ignore_ascii_case(marker).then(|| v.to_string())
        })
    };

    match detect_entity(params)? {
        Some(ScontrolEntity::Partition) => {
            let name = value_for("PartitionName")
                .ok_or_else(|| anyhow::anyhow!("scontrol delete: PartitionName= value missing"))?;
            delete_partition(controller, &name).await
        }
        Some(ScontrolEntity::Reservation) => {
            let name = value_for("ReservationName").ok_or_else(|| {
                anyhow::anyhow!("scontrol delete: ReservationName= value missing")
            })?;
            delete_reservation(controller, &name).await
        }
        None => anyhow::bail!(
            "scontrol delete: expected PartitionName=<name> or ReservationName=<name>"
        ),
    }
}

/// Parse "key=value" params from `scontrol update` command.
async fn parse_and_update(controller: &str, params: &[String]) -> Result<()> {
    if let Some(ScontrolEntity::Partition) = detect_entity(params)? {
        return parse_and_update_partition(controller, params).await;
    }

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
    if let Some(node_pattern) = node_name {
        let proto_state = node_state.as_deref().map(parse_node_state).transpose()?;

        let channel = crate::authclient::connect(controller)
            .await
            .context("failed to connect to spurctld")?;
        let mut client = spur_proto::controller_client(channel);

        let names = resolve_node_names(&mut client, &node_pattern).await?;
        let mut failed: Vec<String> = Vec::new();
        for name in &names {
            if let Err(e) = update_node(&mut client, name, proto_state, node_reason.clone()).await {
                eprintln!("error: {name}: {e}");
                failed.push(name.clone());
            }
        }
        if !failed.is_empty() {
            bail!(
                "failed on {} of {} node(s): {}",
                failed.len(),
                names.len(),
                failed.join(", ")
            );
        }
        return Ok(());
    }

    let jid = job_id.ok_or_else(|| {
        anyhow::anyhow!("scontrol update: JobId=, NodeName=, or PartitionName= required")
    })?;

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
            user: crate::interactive::current_user()?,
            ..Default::default()
        },
    )
    .await
}

pub(crate) fn is_all_node_pattern(pattern: &str) -> bool {
    pattern.eq_ignore_ascii_case("ALL")
}

/// Resolve a node name pattern to a list of individual node names.
///
/// Supports Slurm-compatible hostlist expressions (`node[1-3]`),
/// comma-separated lists (`node1,node2`), and the `ALL` keyword.
pub(crate) async fn resolve_node_names(
    client: &mut SlurmControllerClient<crate::authclient::AuthChannel>,
    pattern: &str,
) -> Result<Vec<String>> {
    if is_all_node_pattern(pattern) {
        let resp = client
            .get_nodes(spur_proto::proto::GetNodesRequest {
                nodelist: String::new(),
                ..Default::default()
            })
            .await
            .context("failed to get nodes")?;
        let names: Vec<String> = resp
            .into_inner()
            .nodes
            .into_iter()
            .map(|n| n.name)
            .collect();
        if names.is_empty() {
            bail!("no nodes registered in the cluster");
        }
        return Ok(names);
    }
    spur_core::hostlist::expand(pattern).context("invalid node name pattern")
}

/// Parse a Slurm node state name into its proto representation.
fn parse_node_state(state: &str) -> Result<i32> {
    match state.to_lowercase().as_str() {
        "idle" | "resume" => Ok(spur_proto::proto::NodeState::NodeIdle as i32),
        "drain" => Ok(spur_proto::proto::NodeState::NodeDrain as i32),
        "down" => Ok(spur_proto::proto::NodeState::NodeDown as i32),
        _ => bail!(
            "scontrol: unknown node state '{}'. Valid states: IDLE, RESUME, DRAIN, DOWN",
            state
        ),
    }
}

/// Parse Slurm key=value pairs for `scontrol update PartitionName=...`.
///
/// Slurm keys accepted (all case-insensitive): PartitionName (required),
/// Nodes, State, Default, MaxTime, DefaultTime, MaxNodes, MinNodes,
/// AllowAccounts, AllowGroups, DenyAccounts, DenyQos, AllowQos,
/// PriorityTier, PreemptMode.
async fn parse_and_update_partition(controller: &str, params: &[String]) -> Result<()> {
    let mut name = String::new();
    let mut nodes: Option<String> = None;
    let mut state: Option<String> = None;
    let mut is_default: Option<bool> = None;
    let mut max_time: Option<String> = None;
    let mut default_time: Option<String> = None;
    let mut max_nodes: Option<u32> = None;
    let mut clear_max_nodes = false;
    let mut min_nodes: Option<u32> = None;
    let mut allow_accounts: Option<String> = None;
    let mut allow_groups: Option<String> = None;
    let mut deny_accounts: Option<String> = None;
    let mut deny_qos: Option<String> = None;
    let mut allow_qos: Option<String> = None;
    let mut priority_tier: Option<u32> = None;
    let mut preempt_mode: Option<String> = None;
    let mut preempt_exempt_time: Option<u32> = None;
    let mut clear_preempt_exempt_time = false;

    for param in params {
        if let Some((key, value)) = param.split_once('=') {
            match key.to_lowercase().as_str() {
                "partitionname" => name = value.into(),
                "nodes" => nodes = Some(value.into()),
                "state" => state = Some(value.to_uppercase()),
                "default" => is_default = Some(value.eq_ignore_ascii_case("yes")),
                "maxtime" => max_time = Some(value.into()),
                "defaulttime" => default_time = Some(value.into()),
                "maxnodes" => {
                    // Last key wins: a later numeric value must undo an earlier clear.
                    if value.eq_ignore_ascii_case("UNLIMITED") || value == "0" {
                        clear_max_nodes = true;
                        max_nodes = None;
                    } else {
                        max_nodes = value.parse().ok();
                        clear_max_nodes = false;
                    }
                }
                "minnodes" => min_nodes = value.parse().ok(),
                "allowaccounts" => allow_accounts = Some(value.into()),
                "allowgroups" => allow_groups = Some(value.into()),
                "denyaccounts" => deny_accounts = Some(value.into()),
                "denyqos" => deny_qos = Some(value.into()),
                "allowqos" => allow_qos = Some(value.into()),
                "prioritytier" | "priorityjobfactor" => priority_tier = value.parse().ok(),
                "preemptmode" => preempt_mode = Some(value.to_uppercase()),
                "preemptexempttime" => {
                    preempt_exempt_time = Some(value.parse::<u32>().map_err(|_| {
                        anyhow::anyhow!("invalid value for PreemptExemptTime=: '{value}'")
                    })?);
                }
                "clearpreemptexempttime" => {
                    clear_preempt_exempt_time = value.eq_ignore_ascii_case("yes")
                        || value == "1"
                        || value.eq_ignore_ascii_case("true");
                }
                // silently ignore Slurm-only keys
                "allocnodes" | "hidden" | "rootonly" | "reqresv" | "oversubscribe"
                | "overtimelimit" | "gracetime" | "disablerootjobs" | "exclusiveuser"
                | "exclusivetopo" | "lln" | "maxcpuspernode" | "maxcpuspersocket"
                | "jobdefaults" | "defmempernode" | "maxmempernode" | "qos" | "tres" => {}
                other => eprintln!("scontrol update partition: unknown key '{}'", other),
            }
        }
    }

    if name.is_empty() {
        anyhow::bail!("scontrol update: PartitionName= is required");
    }

    // An ACL is applied only when its key appeared; an empty value clears it.
    let req = spur_proto::proto::UpdatePartitionRequest {
        name,
        nodes,
        selector: HashMap::new(), // selector not supported in inline syntax
        set_selector: false,
        state,
        is_default,
        max_time,
        default_time,
        max_nodes_value: max_nodes,
        clear_max_nodes,
        min_nodes,
        set_allow_accounts: allow_accounts.is_some(),
        allow_accounts: allow_accounts.as_deref().map(split_csv).unwrap_or_default(),
        set_allow_groups: allow_groups.is_some(),
        allow_groups: allow_groups.as_deref().map(split_csv).unwrap_or_default(),
        set_deny_accounts: deny_accounts.is_some(),
        deny_accounts: deny_accounts.as_deref().map(split_csv).unwrap_or_default(),
        set_deny_qos: deny_qos.is_some(),
        deny_qos: deny_qos.as_deref().map(split_csv).unwrap_or_default(),
        set_allow_qos: allow_qos.is_some(),
        allow_qos: allow_qos.as_deref().map(split_csv).unwrap_or_default(),
        priority_tier,
        preempt_mode,
        preempt_exempt_time,
        clear_preempt_exempt_time,
    };

    update_partition(controller, req).await
}

/// Update a node's state via the controller.
async fn update_node(
    client: &mut SlurmControllerClient<crate::authclient::AuthChannel>,
    name: &str,
    state: Option<i32>,
    reason: Option<String>,
) -> Result<()> {
    client
        .update_node(spur_proto::proto::UpdateNodeRequest {
            name: name.to_string(),
            state,
            reason,
            labels: HashMap::new(),
            remove_labels: Vec::new(),
        })
        .await
        .context("node update failed")?;

    println!("node {} updated", name);
    Ok(())
}

/// Normalize a `scontrol show <entity> <name>` filter: trim surrounding
/// whitespace and treat a blank name as "no filter". Keeping this on the client
/// side means the request field and the not-found decision use the same value,
/// so the CLI and the server (which also trims) never disagree.
fn normalize_show_name(name: Option<&str>) -> Option<&str> {
    name.map(str::trim).filter(|s| !s.is_empty())
}

/// Split a comma-separated list into trimmed, non-empty entries.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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
    allow_qos: &str,
    priority_tier: u32,
    preempt_mode: &str,
    preempt_exempt_time: Option<u32>,
) -> Result<()> {
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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
            allow_qos: split_csv(allow_qos),
            priority_tier,
            preempt_mode: preempt_mode.to_string(),
            preempt_exempt_time,
        })
        .await
        .context("failed to create partition")?;

    println!("Partition {} created", name);
    Ok(())
}

/// Update a partition via the controller. The request is already the proto
/// struct, so callers assemble it directly rather than threading a long
/// positional field list through this sender.
async fn update_partition(
    controller: &str,
    req: spur_proto::proto::UpdatePartitionRequest,
) -> Result<()> {
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    let name = req.name.clone();
    client
        .update_partition(req)
        .await
        .context("failed to update partition")?;

    println!("Partition {} updated", name);
    Ok(())
}

/// Delete a partition via the controller.
async fn delete_partition(controller: &str, name: &str) -> Result<()> {
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    client
        .delete_partition(spur_proto::proto::DeletePartitionRequest {
            name: name.to_string(),
        })
        .await
        .context("failed to delete partition")?;

    println!("Partition {} deleted", name);
    Ok(())
}

/// Reload spur.conf and reconcile partition state to match it.
async fn reconfigure(controller: &str) -> Result<()> {
    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    client.reconfigure(()).await.context("reconfigure failed")?;

    println!(
        "Reconfiguration complete on the leader. Followers converge on restart. \
         Not every setting is reloadable — see the Reload column in \
         docs/admin-guide/configuration.rst for what needs a controller or spurd restart."
    );
    Ok(())
}

/// Parse a reservation duration into minutes via the shared `--time` grammar
/// (whole minutes, `HH:MM:SS`, `D-HH:MM:SS`, `90m`); rejects INFINITE. Quirk of
/// that grammar: a bare `MM:SS` is read as `HH:MM` and bare `days-hours` is
/// rejected, so prefer the unambiguous colon forms.
fn parse_reservation_duration(s: &str) -> Result<u32> {
    spur_core::config::parse_time_minutes(s).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid reservation duration '{s}'; use whole minutes, Slurm time (01:00:00, 30-00:00:00), or suffixed (90m, 1h30m); UNLIMITED/INFINITE not supported"
        )
    })
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
    // Privilege is gated by callers; this is the last input guard.
    if duration == 0 {
        bail!("reservation duration must be positive; e.g. --duration=01:00:00 or Duration=30-00:00:00");
    }

    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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
            user: crate::interactive::current_user()?,
        })
        .await
        .context("failed to create reservation")?;

    println!("Reservation {} created", name);
    Ok(())
}

/// Delete a reservation via the controller.
async fn delete_reservation(controller: &str, name: &str) -> Result<()> {
    crate::privilege::require_privileged("manage reservations")?;

    let channel = crate::authclient::connect(controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    client
        .delete_reservation(spur_proto::proto::DeleteReservationRequest {
            name: name.to_string(),
            user: crate::interactive::current_user()?,
        })
        .await
        .context("failed to delete reservation")?;

    println!("Reservation {} deleted", name);
    Ok(())
}

/// Slurm renders unset strings as `(null)` rather than omitting the field, so
/// an operator can tell "not requested" from "not reported".
fn or_null(value: &str) -> &str {
    if value.is_empty() {
        "(null)"
    } else {
        value
    }
}

fn format_limit(d: Option<&prost_types::Duration>) -> String {
    match d {
        Some(d) if d.seconds > 0 => crate::timefmt::format_duration_dhms(d.seconds),
        _ => "UNLIMITED".into(),
    }
}

fn format_job_detail(job: &spur_proto::proto::JobInfo) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(out, "JobId={} JobName={}", job.job_id, job.name);
    if !job.comment.is_empty() {
        let _ = writeln!(out, "   Comment={}", job.comment);
    }
    let _ = writeln!(out, "   UserId={} Account={}", job.user, job.account);
    let _ = writeln!(out, "   Partition={} QOS={}", job.partition, job.qos);
    let _ = writeln!(
        out,
        "   JobState={} Reason={} Dependency={}",
        state_name(job.state),
        render_reason(&job.state_reason, job.exit_signal),
        or_null(&job.dependency.join(",")),
    );
    let _ = writeln!(
        out,
        "   Requeue={} Restarts={} BatchFlag={} Exclusive={}",
        job.requeue as u8, job.restarts, job.batch_flag as u8, job.exclusive as u8,
    );
    let _ = writeln!(
        out,
        "   RunTime={} TimeLimit={} TimeMin={}",
        crate::timefmt::format_duration_dhms(job.run_time.as_ref().map_or(0, |d| d.seconds)),
        format_limit(job.time_limit.as_ref()),
        match job.time_min.as_ref() {
            Some(d) if d.seconds > 0 => crate::timefmt::format_duration_dhms(d.seconds),
            _ => "N/A".into(),
        },
    );
    let _ = writeln!(
        out,
        "   SubmitTime={} EligibleTime={} AccrueTime={}",
        format_ts(job.submit_time.as_ref()),
        format_ts(job.eligible_time.as_ref()),
        format_ts(job.accrue_time.as_ref()),
    );
    // Slurm reports a pending job's projected start under StartTime; Spur has
    // the same figure from the scheduler's held future slot.
    let start = match (job.start_time.as_ref(), job.planned_start_time.as_ref()) {
        (None, Some(planned)) => format_ts(Some(planned)),
        (actual, _) => format_ts(actual),
    };
    let _ = writeln!(
        out,
        "   StartTime={} EndTime={} Deadline={}",
        start,
        format_ts(job.end_time.as_ref()),
        format_ts(job.deadline.as_ref()),
    );
    let _ = writeln!(
        out,
        "   LastSchedEval={}",
        format_ts(job.last_sched_eval.as_ref())
    );
    let _ = writeln!(
        out,
        "   ReqNodeList={} ExcNodeList={}",
        or_null(&job.req_nodelist),
        or_null(&job.exc_nodelist),
    );
    let _ = write!(out, "   NodeList={}", or_null(&job.nodelist));
    if !job.sched_nodelist.is_empty() {
        let _ = write!(out, " SchedNodeList={}", job.sched_nodelist);
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "   NumNodes={} NumTasks={} CPUs/Task={}",
        job.num_nodes, job.num_tasks, job.cpus_per_task
    );
    let _ = writeln!(out, "   ReqTRES={}", or_null(&job.req_tres));
    if job.req_gpus > 0 || !job.req_gpus_detail.is_empty() {
        let detail = if job.req_gpus_detail.is_empty() {
            format!("gpu:{}", job.req_gpus)
        } else {
            job.req_gpus_detail.clone()
        };
        let _ = writeln!(
            out,
            "   {}={} ReqGPUs={}",
            gpu_tres_label(&detail),
            detail,
            job.req_gpus
        );
    }
    // Slurm suffixes the unit only on a real request; an unset floor is bare 0.
    let min_mem = match job.min_memory_node_mb {
        0 => "0".to_string(),
        mb => format!("{mb}M"),
    };
    let _ = writeln!(
        out,
        "   MinCPUsNode={} MinMemoryNode={}",
        job.min_cpus_node, min_mem
    );
    let _ = writeln!(out, "   Features={}", or_null(&job.features));
    if !job.reservation.is_empty() {
        let _ = writeln!(out, "   Reservation={}", job.reservation);
    }
    if job.array_job_id != 0 {
        let _ = writeln!(
            out,
            "   ArrayJobId={} ArrayTaskId={}",
            job.array_job_id, job.array_task_id
        );
    }
    let _ = writeln!(out, "   WorkDir={}", job.work_dir);
    let _ = write!(
        out,
        "   StdOut={} StdErr={}",
        job.stdout_path, job.stderr_path
    );
    if !job.stdin_path.is_empty() {
        let _ = write!(out, " StdIn={}", job.stdin_path);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "   Command={}", or_null(&job.command));
    let _ = writeln!(out, "   SubmitLine={}", or_null(&job.submit_line));
    let _ = writeln!(
        out,
        "   ExitCode={} DerivedExitCode={} Priority={}",
        format_exit(job.exit_code, job.exit_signal),
        format_exit(job.derived_exit_code, 0),
        job.priority
    );
    if job.preempted_by != 0 || !job.preempt_mode.is_empty() {
        let _ = writeln!(
            out,
            "   PreemptedBy={} PreemptMode={} PreemptQOS={}",
            if job.preempted_by == 0 {
                "N/A".to_string()
            } else {
                job.preempted_by.to_string()
            },
            if job.preempt_mode.is_empty() {
                "N/A"
            } else {
                &job.preempt_mode
            },
            if job.preempt_qos.is_empty() {
                "N/A"
            } else {
                &job.preempt_qos
            },
        );
    }
    let _ = writeln!(out);
    out
}

fn gpu_tres_label(detail: &str) -> &'static str {
    if detail.ends_with("/node") {
        "TresPerNode"
    } else if detail.ends_with("/task") {
        "TresPerTask"
    } else {
        "TresPerJob"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_pinned_job() -> spur_proto::proto::JobInfo {
        spur_proto::proto::JobInfo {
            job_id: 71860,
            name: "probe".into(),
            state: spur_proto::proto::JobState::JobPending as i32,
            state_reason: "Resources".into(),
            req_nodelist: "node[1-2]".into(),
            exc_nodelist: "node9".into(),
            features: "mi300x".into(),
            submit_line: "sbatch -w 'node[1-2]' job.sh".into(),
            req_tres: "cpu=16,node=2,gres/gpu=8".into(),
            min_cpus_node: 8,
            min_memory_node_mb: 16000,
            dependency: vec!["afterok:5".into()],
            ..Default::default()
        }
    }

    #[test]
    fn job_detail_shows_the_requested_and_excluded_node_lists() {
        let out = format_job_detail(&pending_pinned_job());
        assert!(
            out.contains("ReqNodeList=node[1-2] ExcNodeList=node9"),
            "{out}"
        );
        // Allocated list is empty while pending; it must not be confused with
        // the requested one.
        assert!(out.contains("NodeList=(null)"), "{out}");
    }

    #[test]
    fn job_detail_shows_submit_line_and_constraints() {
        let out = format_job_detail(&pending_pinned_job());
        assert!(
            out.contains("SubmitLine=sbatch -w 'node[1-2]' job.sh"),
            "{out}"
        );
        assert!(out.contains("Features=mi300x"), "{out}");
        assert!(out.contains("Dependency=afterok:5"), "{out}");
        assert!(out.contains("ReqTRES=cpu=16,node=2,gres/gpu=8"), "{out}");
        assert!(out.contains("MinCPUsNode=8 MinMemoryNode=16000M"), "{out}");
        // Slurm prints a bare 0 when no per-node floor was requested.
        let unset = format_job_detail(&spur_proto::proto::JobInfo::default());
        assert!(unset.contains("MinMemoryNode=0\n"), "{unset}");
    }

    #[test]
    fn job_detail_renders_null_for_unrequested_placement_fields() {
        let out = format_job_detail(&spur_proto::proto::JobInfo::default());
        // Printing the field as (null) is the point: absence of the line is
        // indistinguishable from an unsupported build.
        assert!(
            out.contains("ReqNodeList=(null) ExcNodeList=(null)"),
            "{out}"
        );
        assert!(out.contains("SubmitLine=(null)"), "{out}");
        assert!(out.contains("Features=(null)"), "{out}");
    }

    #[test]
    fn job_detail_reports_last_sched_eval_and_accrual_times() {
        let mut job = pending_pinned_job();
        job.submit_time = Some(prost_types::Timestamp {
            seconds: 1_756_281_787,
            nanos: 0,
        });
        job.eligible_time = job.submit_time;
        job.accrue_time = job.submit_time;
        assert!(
            format_job_detail(&job).contains("LastSchedEval=N/A"),
            "unevaluated job must say so, not omit the field"
        );

        job.last_sched_eval = Some(prost_types::Timestamp {
            seconds: 1_756_281_999,
            nanos: 0,
        });
        let out = format_job_detail(&job);
        assert!(out.contains("LastSchedEval=2025-08-27T08:06:39"), "{out}");
        assert!(
            out.contains("EligibleTime=2025-08-27T08:03:07 AccrueTime=2025-08-27T08:03:07"),
            "{out}"
        );
    }

    #[test]
    fn job_detail_reports_the_projected_start_of_a_held_future_slot() {
        let mut job = pending_pinned_job();
        job.planned_start_time = Some(prost_types::Timestamp {
            seconds: 1_756_281_787,
            nanos: 0,
        });
        job.sched_nodelist = "node[1-2]".into();

        let out = format_job_detail(&job);
        assert!(out.contains("StartTime=2025-08-27T08:03:07"), "{out}");
        assert!(
            out.contains("NodeList=(null) SchedNodeList=node[1-2]"),
            "the allocated list stays empty; the planned one answers 'which nodes': {out}"
        );
    }

    #[test]
    fn job_detail_prefers_the_actual_start_over_the_projection() {
        let mut job = pending_pinned_job();
        job.start_time = Some(prost_types::Timestamp {
            seconds: 1_756_281_787,
            nanos: 0,
        });
        job.planned_start_time = Some(prost_types::Timestamp {
            seconds: 1_900_000_000,
            nanos: 0,
        });
        assert!(
            format_job_detail(&job).contains("StartTime=2025-08-27T08:03:07"),
            "a started job must not display a stale projection"
        );
    }

    #[test]
    fn job_detail_omits_sched_nodelist_when_no_slot_is_held() {
        let out = format_job_detail(&pending_pinned_job());
        assert!(out.contains("NodeList=(null)\n"), "{out}");
        assert!(!out.contains("SchedNodeList"), "{out}");
    }

    #[test]
    fn job_detail_keeps_the_time_limit_and_exit_summary() {
        let mut job = pending_pinned_job();
        job.time_limit = Some(prost_types::Duration {
            seconds: 300,
            nanos: 0,
        });
        job.priority = 11000;
        let out = format_job_detail(&job);
        assert!(out.contains("TimeLimit=00:05:00"), "{out}");
        assert!(out.contains("RunTime=00:00:00"), "{out}");
        assert!(
            out.contains("ExitCode=0:0 DerivedExitCode=0:0 Priority=11000"),
            "{out}"
        );
    }

    #[test]
    fn job_detail_reports_unlimited_when_no_time_limit_is_set() {
        let out = format_job_detail(&pending_pinned_job());
        assert!(out.contains("TimeLimit=UNLIMITED"), "{out}");
    }

    #[test]
    fn gpu_tres_label_per_node() {
        assert_eq!(gpu_tres_label("gpu:4/node"), "TresPerNode");
        assert_eq!(gpu_tres_label("gpu:mi300x:2/node"), "TresPerNode");
    }

    #[test]
    fn gpu_tres_label_per_task() {
        assert_eq!(gpu_tres_label("gpu:2/task"), "TresPerTask");
        assert_eq!(gpu_tres_label("gpu:h100:1/task"), "TresPerTask");
    }

    #[test]
    fn gpu_tres_label_total() {
        assert_eq!(gpu_tres_label("gpu:8"), "TresPerJob");
        assert_eq!(gpu_tres_label("gpu:mi300x:4"), "TresPerJob");
    }

    #[test]
    fn planned_reservation_line_none_when_no_planned_job() {
        let node = spur_proto::proto::NodeInfo::default();
        assert_eq!(planned_reservation_line(&node), None);
    }

    #[test]
    fn planned_reservation_line_shows_job_and_start() {
        let node = spur_proto::proto::NodeInfo {
            planned_job_id: 42,
            planned_start: Some(prost_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
            ..Default::default()
        };
        assert_eq!(
            planned_reservation_line(&node).unwrap(),
            "PlannedJobId=42 PlannedStartTime=2023-11-14T22:13:20"
        );
    }

    #[test]
    fn planned_reservation_line_start_na_when_unset() {
        let node = spur_proto::proto::NodeInfo {
            planned_job_id: 42,
            planned_start: None,
            ..Default::default()
        };
        assert_eq!(
            planned_reservation_line(&node).unwrap(),
            "PlannedJobId=42 PlannedStartTime=N/A"
        );
    }

    fn idle_node() -> spur_proto::proto::NodeInfo {
        spur_proto::proto::NodeInfo {
            state: spur_proto::proto::NodeState::NodeIdle as i32,
            ..Default::default()
        }
    }

    #[test]
    fn node_state_display_plain_idle() {
        assert_eq!(node_state_display(&idle_node()), "IDLE");
    }

    #[test]
    fn node_state_display_reserved() {
        let mut node = idle_node();
        node.active_reservation = "r1".into();
        assert_eq!(node_state_display(&node), "IDLE+RESERVED");
    }

    #[test]
    fn node_state_display_maintenance_reserved() {
        let mut node = idle_node();
        node.active_reservation = "r1".into();
        node.reservation_maint = true;
        assert_eq!(node_state_display(&node), "IDLE+MAINTENANCE+RESERVED");
    }

    #[test]
    fn node_state_display_planned() {
        let mut node = idle_node();
        node.planned_job_id = 42;
        assert_eq!(node_state_display(&node), "IDLE+PLANNED");
    }

    #[test]
    fn node_state_display_non_idle_state_ignores_overlay_fields() {
        let mut node = idle_node();
        node.state = spur_proto::proto::NodeState::NodeAllocated as i32;
        node.planned_job_id = 42;
        assert_eq!(node_state_display(&node), "ALLOCATED");
    }

    #[test]
    fn parse_node_state_known_states() {
        let idle = spur_proto::proto::NodeState::NodeIdle as i32;
        assert_eq!(parse_node_state("idle").unwrap(), idle);
        assert_eq!(parse_node_state("resume").unwrap(), idle);
        assert_eq!(
            parse_node_state("drain").unwrap(),
            spur_proto::proto::NodeState::NodeDrain as i32
        );
        assert_eq!(
            parse_node_state("down").unwrap(),
            spur_proto::proto::NodeState::NodeDown as i32
        );
    }

    fn p(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_show_name_trims_and_blanks_to_none() {
        assert_eq!(normalize_show_name(None), None);
        assert_eq!(normalize_show_name(Some("   ")), None);
        assert_eq!(
            normalize_show_name(Some(" rocm_patch ")),
            Some("rocm_patch")
        );
        assert_eq!(normalize_show_name(Some("rocm_patch")), Some("rocm_patch"));
    }

    #[test]
    fn detect_entity_is_order_independent() {
        // The entity marker may appear after other keys — Slurm syntax is
        // order-independent, so detection must scan all params, not just the first.
        assert_eq!(
            detect_entity(&p(&["Nodes=n1", "PartitionName=gpu"])).unwrap(),
            Some(ScontrolEntity::Partition)
        );
        assert_eq!(
            detect_entity(&p(&["Flags=MAINT", "ReservationName=maint"])).unwrap(),
            Some(ScontrolEntity::Reservation)
        );
        assert_eq!(
            detect_entity(&p(&["State=DOWN", "PartitionName=gpu"])).unwrap(),
            Some(ScontrolEntity::Partition)
        );
    }

    #[test]
    fn parse_node_state_is_case_insensitive() {
        let drain = spur_proto::proto::NodeState::NodeDrain as i32;
        assert_eq!(parse_node_state("DRAIN").unwrap(), drain);
        assert_eq!(parse_node_state("Drain").unwrap(), drain);
    }

    #[test]
    fn parse_node_state_rejects_unknown() {
        let err = parse_node_state("DRAIM").unwrap_err().to_string();
        assert!(err.contains("DRAIM"), "error should echo the input: {err}");
        assert!(
            err.contains("DRAIN"),
            "error should list valid states: {err}"
        );
    }

    #[test]
    fn detect_entity_none_when_no_marker() {
        assert_eq!(
            detect_entity(&p(&["JobId=5", "Priority=10"])).unwrap(),
            None
        );
    }

    #[test]
    fn parse_node_state_rejects_empty() {
        assert!(parse_node_state("").is_err());
    }

    #[test]
    fn detect_entity_rejects_both_markers() {
        assert!(detect_entity(&p(&["PartitionName=gpu", "ReservationName=maint"])).is_err());
    }

    #[tokio::test]
    async fn scontrol_update_expands_hostlist() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        main_with_args(vec![
            "scontrol".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "update".into(),
            "NodeName=n[1-3]".into(),
            "State=DRAIN".into(),
            "Reason=test".into(),
        ])
        .await
        .unwrap();
        assert_eq!(capture.update_node_names(), vec!["n1", "n2", "n3"]);
    }

    #[tokio::test]
    async fn scontrol_update_best_effort_continues_on_failure() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_update_node_fail_names(["n2".to_string()].into());
        let err = main_with_args(vec![
            "scontrol".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "update".into(),
            "NodeName=n[1-3]".into(),
            "State=DRAIN".into(),
        ])
        .await
        .unwrap_err();

        let names = capture.update_node_names();
        assert_eq!(names, vec!["n1", "n2", "n3"]);
        let msg = err.to_string();
        assert!(
            msg.contains("n2"),
            "error should mention failed node: {msg}"
        );
        assert!(msg.contains("1 of 3"), "error should report counts: {msg}");
    }

    #[tokio::test]
    async fn scontrol_update_invalid_state_sends_no_rpcs() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let result = main_with_args(vec![
            "scontrol".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "update".into(),
            "NodeName=n[1-3]".into(),
            "State=BOGUS".into(),
        ])
        .await;
        assert!(result.is_err());
        assert!(capture.update_node_names().is_empty());
    }

    #[test]
    fn parse_reservation_duration_accepts_minutes_and_slurm_formats() {
        assert_eq!(parse_reservation_duration("60").unwrap(), 60);
        assert_eq!(parse_reservation_duration("01:00:00").unwrap(), 60);
        assert_eq!(
            parse_reservation_duration("30-00:00:00").unwrap(),
            30 * 24 * 60
        );
        assert_eq!(parse_reservation_duration("90m").unwrap(), 90);
        // Zero (and any zero-length encoding) is update-reservation's "no
        // change" sentinel; the create path rejects it via its own guard.
        assert_eq!(parse_reservation_duration("0").unwrap(), 0);
        assert_eq!(parse_reservation_duration("00:00:00").unwrap(), 0);
    }

    #[test]
    fn parse_reservation_duration_rejects_unparseable() {
        assert!(parse_reservation_duration("notatime").is_err());
        assert!(parse_reservation_duration("").is_err());
        assert!(parse_reservation_duration("1.5h").is_err());
    }

    #[test]
    fn parse_reservation_duration_rejects_unbounded() {
        // Reservations store a concrete end_time, so there is no unbounded
        // representation; UNLIMITED/INFINITE are rejected, not silently mapped.
        assert!(parse_reservation_duration("UNLIMITED").is_err());
        assert!(parse_reservation_duration("INFINITE").is_err());
    }

    #[test]
    fn parse_reservation_create_params_rejects_bad_duration() {
        let params = p(&[
            "ReservationName=r1",
            "StartTime=now",
            "Duration=notatime",
            "Nodes=n1",
        ]);
        let err = parse_reservation_create_params(&params).unwrap_err();
        assert!(
            err.to_string().contains("invalid reservation duration"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_reservation_create_params_requires_duration() {
        let params = p(&["ReservationName=r1", "StartTime=now", "Nodes=n1"]);
        let err = parse_reservation_create_params(&params).unwrap_err();
        assert!(
            err.to_string().contains("Duration= is required"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_reservation_create_params_accepts_slurm_duration() {
        let params = p(&[
            "ReservationName=r1",
            "StartTime=now",
            "Duration=30-00:00:00",
            "Nodes=n1",
        ]);
        let parsed = parse_reservation_create_params(&params).unwrap();
        assert_eq!(parsed.name, "r1");
        assert_eq!(parsed.duration_minutes, 30 * 24 * 60);
    }
}
