// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Every `SlurmController`/`SlurmAccounting` method is classified here once, so
//! coverage is this table's property; the completeness test enforces that.

use crate::accounting::{TxnAction, TxnEntity};

/// Where an audit row may be written from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuditScope {
    /// Mutating controller RPCs forward to the leader, so a follower must not
    /// record a row for work it did not apply.
    LeaderOnly,
    /// Wherever the RPC executes. Accounting writes go straight to PostgreSQL
    /// and have no leader to defer to.
    Local,
}

/// `action` and `entity` are fixed per method, so the layer knows them without
/// decoding the body; only the target name and parameters need the handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mutating {
    pub action: TxnAction,
    pub entity: TxnEntity,
    pub scope: AuditScope,
    /// A `true` entry leaving the slot empty warns; `false` covers both a
    /// targetless action (`Reconfigure`) and a handler not yet annotated.
    pub targeted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RpcClass {
    /// Changes cluster state on a user's behalf; gets a `txn` row.
    Mutating(Mutating),
    /// Serves state without changing it. Tier 1 logs it; no `txn` row.
    ReadOnly,
    /// Daemon traffic, or client traffic at per-step/per-poll rates. Tier 1
    /// only: a row per heartbeat would bury the operator actions.
    Internal,
}

const fn targeted(action: TxnAction, entity: TxnEntity, scope: AuditScope) -> RpcClass {
    RpcClass::Mutating(Mutating {
        action,
        entity,
        scope,
        targeted: true,
    })
}

const fn untargeted(action: TxnAction, entity: TxnEntity, scope: AuditScope) -> RpcClass {
    RpcClass::Mutating(Mutating {
        action,
        entity,
        scope,
        targeted: false,
    })
}

/// Classify a gRPC method name. `None` means unclassified, which the
/// completeness test makes impossible in a build that passes.
pub(crate) fn classify(method: &str) -> Option<RpcClass> {
    use AuditScope::{LeaderOnly, Local};
    use TxnAction::{Create, Delete, Update};

    let class = match method {
        // --- Nodes ---
        "UpdateNode" => targeted(Update, TxnEntity::Node, LeaderOnly),
        "DrainNode" => targeted(Update, TxnEntity::Node, LeaderOnly),
        "DeregisterNode" => targeted(Delete, TxnEntity::Node, LeaderOnly),
        "DeregisterAgent" => targeted(Delete, TxnEntity::Node, LeaderOnly),

        // --- Reservations ---
        "CreateReservation" => targeted(Create, TxnEntity::Reservation, LeaderOnly),
        "UpdateReservation" => targeted(Update, TxnEntity::Reservation, LeaderOnly),
        "DeleteReservation" => targeted(Delete, TxnEntity::Reservation, LeaderOnly),

        // --- Jobs. Annotated in a later phase; recorded meanwhile. ---
        "SubmitJob" => untargeted(Create, TxnEntity::Job, LeaderOnly),
        "CancelJob" => untargeted(Delete, TxnEntity::Job, LeaderOnly),
        "UpdateJob" => untargeted(Update, TxnEntity::Job, LeaderOnly),
        "RequeueJob" => untargeted(Update, TxnEntity::Job, LeaderOnly),
        "SuspendJob" => untargeted(Update, TxnEntity::Job, LeaderOnly),
        "ResumeJob" => untargeted(Update, TxnEntity::Job, LeaderOnly),
        // Runs a command inside somebody's already-running job.
        "ExecInJob" => untargeted(Create, TxnEntity::Job, LeaderOnly),
        "RunStep" => untargeted(Create, TxnEntity::Job, LeaderOnly),

        // --- Partitions and configuration ---
        "CreatePartition" => untargeted(Create, TxnEntity::Partition, LeaderOnly),
        "UpdatePartition" => untargeted(Update, TxnEntity::Partition, LeaderOnly),
        "DeletePartition" => untargeted(Delete, TxnEntity::Partition, LeaderOnly),
        // Cluster-wide, so there is no target to name.
        "Reconfigure" => untargeted(Update, TxnEntity::Config, LeaderOnly),

        // --- Credentials ---
        "CreateToken" => untargeted(Create, TxnEntity::Token, LeaderOnly),
        "RevokeToken" => untargeted(Delete, TxnEntity::Token, LeaderOnly),

        // --- k0s cluster lifecycle ---
        "ClusterUp" => untargeted(Create, TxnEntity::Cluster, LeaderOnly),
        "ClusterDown" => untargeted(Delete, TxnEntity::Cluster, LeaderOnly),
        "ClusterAddNodes" => untargeted(Update, TxnEntity::Cluster, LeaderOnly),
        "ClusterRemoveNodes" => untargeted(Update, TxnEntity::Cluster, LeaderOnly),
        // `--user` mints a Kubernetes service-account token, and the layer
        // cannot tell that from a plain read, so record both.
        "ClusterKubeconfig" => untargeted(Create, TxnEntity::Cluster, LeaderOnly),

        // --- Accounting entities. Slurm's txn_table covers exactly these. ---
        "CreateAccount" => untargeted(Create, TxnEntity::Account, Local),
        "DeleteAccount" => untargeted(Delete, TxnEntity::Account, Local),
        "AddUser" => untargeted(Create, TxnEntity::User, Local),
        "RemoveUser" => untargeted(Delete, TxnEntity::User, Local),
        "CreateQos" => untargeted(Create, TxnEntity::Qos, Local),
        "DeleteQos" => untargeted(Delete, TxnEntity::Qos, Local),

        // --- Reads ---
        "GetJobs"
        | "GetJob"
        | "GetNodes"
        | "GetNode"
        | "GetPartitions"
        | "GetJobSteps"
        | "ListReservations"
        | "ListTokens"
        | "ClusterStatus"
        | "Ping"
        | "GetJobMetrics"
        | "GetNodeMetrics"
        | "GetRpcStats"
        | "GetSchedStats"
        | "GetAssocMgrInfo"
        | "GetJobHistory"
        | "GetUsage"
        | "ListAccounts"
        | "ListUsers"
        | "ListQos"
        | "GetFairshareFactors"
        | "GetTransactions" => RpcClass::ReadOnly,
        // `sdiag --reset` zeroes in-memory counters, losing observability data
        // rather than cluster state, so the audit log is not answerable for it.
        "ResetDiagStats" => RpcClass::ReadOnly,

        // --- Daemon-to-daemon ---
        "RegisterAgent" | "Heartbeat" | "ReportJobStatus" | "RecordJobStart" | "RecordJobEnd" => {
            RpcClass::Internal
        }
        // User-initiated, but srun/salloc drive these per step or per poll
        // rather than per operator decision.
        "JobKeepalive" | "CreateJobStep" | "CompleteJobStep" | "CompleteJob" => RpcClass::Internal,

        _ => return None,
    };
    Some(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baked in at compile time so the test does not depend on the runner's
    /// working directory.
    const PROTO: &str = include_str!("../../../../proto/slurm.proto");

    /// Method names the named services declare in `proto`.
    fn methods_in(proto: &str, services: &[&str]) -> Vec<String> {
        let mut current = None;
        let mut methods = Vec::new();
        for line in proto.lines() {
            if let Some(rest) = line.strip_prefix("service ") {
                current = rest.split_whitespace().next().map(str::to_owned);
                continue;
            }
            if line == "}" {
                current = None;
                continue;
            }
            if !current.as_deref().is_some_and(|s| services.contains(&s)) {
                continue;
            }
            if let Some(rest) = line.trim().strip_prefix("rpc ") {
                if let Some(name) = rest.split('(').next() {
                    methods.push(name.trim().to_owned());
                }
            }
        }
        methods
    }

    /// Method names declared by the two services this layer covers.
    fn audited_service_methods() -> Vec<String> {
        methods_in(PROTO, &["SlurmController", "SlurmAccounting"])
    }

    #[test]
    fn the_proto_parser_finds_the_services_it_expects() {
        // Guards the test below: a parser that silently matched nothing would
        // make the completeness assertion vacuous.
        let methods = audited_service_methods();
        assert!(
            methods.len() > 50,
            "expected both services' methods, found {}",
            methods.len()
        );
        assert!(methods.contains(&"UpdateNode".to_string()));
        assert!(methods.contains(&"GetTransactions".to_string()));
        assert!(
            !methods.contains(&"LaunchJob".to_string()),
            "SlurmAgent methods must not be pulled in"
        );
    }

    #[test]
    fn every_proto_method_is_classified() {
        let unclassified: Vec<_> = audited_service_methods()
            .into_iter()
            .filter(|m| classify(m).is_none())
            .collect();
        assert!(
            unclassified.is_empty(),
            "these RPCs are not classified in the audit registry, so they would \
             go unaudited: {unclassified:?}. Add each to `classify`."
        );
    }

    #[test]
    fn an_unknown_method_is_not_classified() {
        assert!(classify("NoSuchRpc").is_none());
    }

    #[test]
    fn the_completeness_check_catches_a_newly_added_rpc() {
        // Synthetic rather than the real proto: adding an RPC there would fail
        // to compile before any test could run.
        let synthetic = "service SlurmController {\n  \
             rpc BrandNewThing(BrandNewThingRequest) returns (BrandNewThingResponse);\n}\n";

        let found = methods_in(synthetic, &["SlurmController"]);
        assert_eq!(found, vec!["BrandNewThing".to_string()]);
        assert!(
            found.iter().any(|m| classify(m).is_none()),
            "an unclassified RPC must be reported, otherwise the completeness \
             test would pass while coverage silently regressed"
        );
    }

    #[test]
    fn the_parser_ignores_services_this_layer_does_not_cover() {
        let synthetic = "service SlurmAgent {\n  rpc LaunchJob(A) returns (B);\n}\n";
        assert!(methods_in(synthetic, &["SlurmController", "SlurmAccounting"]).is_empty());
    }

    #[test]
    fn accounting_mutations_are_recorded_off_the_leader() {
        // Accounting bypasses Raft, so gating its rows on leadership would drop
        // every one served by a follower.
        for method in ["CreateAccount", "AddUser", "DeleteQos"] {
            let Some(RpcClass::Mutating(m)) = classify(method) else {
                panic!("{method} must be mutating");
            };
            assert_eq!(m.scope, AuditScope::Local, "{method}");
        }
    }

    #[test]
    fn controller_mutations_are_recorded_only_on_the_leader() {
        for method in ["UpdateNode", "DrainNode", "CreateReservation", "SubmitJob"] {
            let Some(RpcClass::Mutating(m)) = classify(method) else {
                panic!("{method} must be mutating");
            };
            assert_eq!(m.scope, AuditScope::LeaderOnly, "{method}");
        }
    }

    #[test]
    fn node_and_reservation_handlers_name_their_target() {
        for method in [
            "UpdateNode",
            "DrainNode",
            "DeregisterNode",
            "DeregisterAgent",
            "CreateReservation",
            "UpdateReservation",
            "DeleteReservation",
        ] {
            let Some(RpcClass::Mutating(m)) = classify(method) else {
                panic!("{method} must be mutating");
            };
            assert!(m.targeted, "{method} is annotated, so it must be targeted");
        }
    }

    #[test]
    fn reads_and_daemon_traffic_get_no_row() {
        assert_eq!(classify("GetNodes"), Some(RpcClass::ReadOnly));
        assert_eq!(classify("ResetDiagStats"), Some(RpcClass::ReadOnly));
        assert_eq!(classify("Heartbeat"), Some(RpcClass::Internal));
        assert_eq!(classify("JobKeepalive"), Some(RpcClass::Internal));
    }
}
