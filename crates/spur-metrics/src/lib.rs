// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cluster metrics aggregation and OpenMetrics 1.0 export for spurctld.

pub mod export;
pub mod job;
pub mod k8s;
pub mod node;
pub mod partition;
pub mod reconcile;
pub mod rpc;
pub mod scheduler;
pub mod user_acct;

pub use export::jobs::{encode_job_metrics, job_state_metric_suffix};
pub use export::jobs_users_accts::encode_jobs_users_accts_metrics;
pub use export::k8s_cluster::{encode_k8s_cluster_metrics, encode_k8s_metrics};
pub use export::nodes::encode_nodes_metrics;
pub use export::partitions::encode_partitions_metrics;
pub use export::reconcile::encode_reconcile_metrics;
pub use export::rpc::encode_rpc_metrics;
pub use export::scheduler::encode_scheduler_metrics;
pub use export::CONTENT_TYPE;
pub use k8s::{K8sClusterMetricsSnapshot, K8sMetrics};
pub use node::node_state_metric_suffix;
pub use partition::PartitionMetricsSnapshot;
pub use reconcile::ReconcileStatsSnapshot;
pub use rpc::{RpcOperationSnapshot, RpcStatsSnapshot};
pub use scheduler::SchedStatsSnapshot;
pub use user_acct::UserAcctMetricsSnapshot;
