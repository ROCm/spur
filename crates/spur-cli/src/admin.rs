// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur admin` subcommands: cluster operations that need an operator.

use anyhow::Result;
use clap::{Parser, Subcommand};

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    RaftAddLearnerRequest, RaftMembershipRequest, RaftPromoteVoterRequest, RaftRemoveVoterRequest,
};

#[derive(Parser, Debug)]
#[command(name = "admin", about = "Cluster administration")]
pub struct AdminArgs {
    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817",
        global = true
    )]
    controller: String,

    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Grow or shrink the Raft controller set.
    #[command(subcommand)]
    Raft(RaftCommand),
}

#[derive(Subcommand, Debug)]
pub enum RaftCommand {
    /// Add a controller as a learner. It receives the log but does not vote.
    AddLearner {
        /// Raft node id of the new controller, as set in its controller.node_id.
        node_id: u64,
        /// Its Raft address, "host:port" (the controller.raft_listen_addr port, 6821).
        address: String,
    },
    /// Promote a caught-up learner to a voter.
    Promote {
        /// Raft node id of the learner.
        node_id: u64,
    },
    /// Remove a member, voter or learner, from the cluster.
    Remove {
        /// Raft node id of the member.
        node_id: u64,
    },
    /// Show the membership, the leader, and how far each member has caught up.
    Status,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let parsed = AdminArgs::try_parse_from(args)?;
    let controller = parsed.controller;
    match parsed.command {
        AdminCommand::Raft(cmd) => raft_command(&controller, cmd).await,
    }
}

fn effective_user() -> String {
    whoami::username().unwrap_or_else(|_| "unknown".into())
}

async fn raft_command(controller: &str, cmd: RaftCommand) -> Result<()> {
    let mut client = SlurmControllerClient::new(crate::authclient::connect(controller).await?);
    match cmd {
        RaftCommand::AddLearner { node_id, address } => {
            client
                .raft_add_learner(RaftAddLearnerRequest {
                    node_id,
                    address: address.clone(),
                    user: effective_user(),
                })
                .await?;
            println!("added node {node_id} at {address} as a learner");
            println!("watch it catch up with: spur admin raft status");
        }
        RaftCommand::Promote { node_id } => {
            client
                .raft_promote_voter(RaftPromoteVoterRequest {
                    node_id,
                    user: effective_user(),
                })
                .await?;
            println!("promoted node {node_id} to a voter");
        }
        RaftCommand::Remove { node_id } => {
            client
                .raft_remove_voter(RaftRemoveVoterRequest {
                    node_id,
                    user: effective_user(),
                })
                .await?;
            println!("removed node {node_id} from the cluster");
            println!(
                "stop that controller now and wipe its state directory before you start it again"
            );
        }
        RaftCommand::Status => {
            let resp = client
                .raft_membership(RaftMembershipRequest {})
                .await?
                .into_inner();
            print_status(&resp);
        }
    }
    Ok(())
}

fn print_status(resp: &spur_proto::proto::RaftMembershipResponse) {
    let leader = match resp.leader {
        0 => "none".to_string(),
        id => id.to_string(),
    };
    println!(
        "answered by node {} ({}), leader {}, last_log_index {}",
        resp.this_node, resp.state, leader, resp.last_log_index
    );
    println!("{:<8} {:<30} {:<8} MATCHED", "NODE", "ADDRESS", "ROLE");
    for m in &resp.members {
        let role = if m.voter { "voter" } else { "learner" };
        let matched = match m.matched_index {
            i if i < 0 => "-".to_string(),
            i => i.to_string(),
        };
        println!("{:<8} {:<30} {:<8} {}", m.node_id, m.address, role, matched);
    }
    if resp.leader != resp.this_node {
        println!("(ask the leader for MATCHED; only it tracks how far each member has caught up)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_learner() {
        let args =
            AdminArgs::try_parse_from(vec!["admin", "raft", "add-learner", "4", "ctrl4:6821"])
                .unwrap();
        match args.command {
            AdminCommand::Raft(RaftCommand::AddLearner { node_id, address }) => {
                assert_eq!(node_id, 4);
                assert_eq!(address, "ctrl4:6821");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_status() {
        let args = AdminArgs::try_parse_from(vec!["admin", "raft", "status"]).unwrap();
        assert!(matches!(
            args.command,
            AdminCommand::Raft(RaftCommand::Status)
        ));
    }
}
