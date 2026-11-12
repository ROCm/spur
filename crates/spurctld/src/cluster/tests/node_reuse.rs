// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use spur_sched::traits::{Assignment, Scheduler as _};

fn node_cap_record(cap: u64) -> TresRecord {
    let mut tres = TresRecord::new();
    tres.set(TresType::Node, cap);
    tres
}

fn install_node_caps(cm: &ClusterManager, account: Option<u64>, qos: Option<u64>) {
    cm.association_cache().insert_limits(
        "testuser",
        "research",
        spur_core::accounting::AccountLimits {
            grp_tres: account.map(node_cap_record),
            ..Default::default()
        },
    );
    cm.qos_cache().insert(Qos {
        name: "pool".into(),
        limits: spur_core::accounting::QosLimits {
            grp_tres: qos.map(node_cap_record),
            ..Default::default()
        },
        ..Default::default()
    });
}

fn occupy_node(cm: &ClusterManager, id: JobId, name: &str, account: bool, qos: bool) {
    let mut holder = make_running_job(id, &[name], 1);
    holder.spec.account = account.then(|| "research".into());
    holder.spec.qos = qos.then(|| "pool".into());
    cm.jobs.write().insert(id, holder);
    let mut nodes = cm.nodes.write();
    let node = nodes.get_mut(name).expect("registered holder node");
    node.alloc_resources.cpus = 1;
    node.state = NodeState::Mixed;
}

fn reuse_spec(partitions: &str, nodelist: &str) -> JobSpec {
    JobSpec {
        account: Some("research".into()),
        qos: Some("pool".into()),
        partition: Some(partitions.into()),
        nodelist: Some(nodelist.into()),
        num_nodes: 2,
        num_tasks: 2,
        ..basic_spec("node-reuse")
    }
}

fn reuse_assignments(cm: &ClusterManager, pending: &[Job]) -> Vec<Assignment> {
    let nodes = cm.get_nodes();
    let partitions = cm.get_partitions();
    let reservations = cm.get_reservations();
    let busy_until = crate::scheduler_loop::running_jobs_busy_until(cm);
    let state = spur_sched::traits::ClusterState {
        fabric: None,
        preemption_handoffs: &HashMap::new(),
        busy_until: &busy_until,
        partition_node_usage: &Default::default(),
        nodes: &nodes,
        partitions: &partitions,
        reservations: &reservations,
        topology: None,
    };
    spur_sched::backfill::BackfillScheduler::new(10).schedule(pending, &state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additive_nodelist_reuses_a_filler_and_charges_the_mandatory_new_host() {
    for by in [GrpCap::Account, GrpCap::Qos] {
        for cap in [2, 3] {
            let dir = TempDir::new().unwrap();
            let cm = test_cluster(&dir).await;
            for name in ["n1", "n2", "n3", "n4"] {
                register_node(&cm, name, 8, 16000);
            }
            cm.nodes.write().get_mut("n4").unwrap().weight = 1000;
            install_node_caps(
                &cm,
                (by == GrpCap::Account).then_some(cap),
                (by == GrpCap::Qos).then_some(cap),
            );
            occupy_node(&cm, 101, "n1", true, true);
            occupy_node(&cm, 102, "n2", true, true);
            let id = submit_and_wait(&cm, reuse_spec("default", "n3"));
            let classification = cm.classify_pending_jobs();
            if cap == 2 {
                assert!(!admitted(&classification).contains(&id));
                cm.refresh_pending_reasons();
                assert_eq!(
                    cm.get_job(id).unwrap().pending_reason,
                    match by {
                        GrpCap::Account => PendingReason::AssocGrpNodeLimit,
                        GrpCap::Qos => PendingReason::QosGrpNodeLimit,
                    }
                );
                continue;
            }
            assert!(admitted(&classification).contains(&id));
            let assignments = reuse_assignments(&cm, &classification.jobs);
            let assignment = assignments.iter().find(|a| a.job_id == id).unwrap();
            assert_eq!(assignment.nodes.len(), 2);
            assert!(assignment.nodes.iter().any(|n| n == "n3"));
            assert_eq!(
                assignment
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.as_str(), "n1" | "n2"))
                    .count(),
                1
            );
            assert!(!assignment.nodes.iter().any(|n| n == "n4"));
        }
    }
}

#[test]
fn additive_node_charge_counts_only_mandatory_hosts_without_reuse_credit() {
    let nodes = ["n1", "n2", "n3", "n4"]
        .into_iter()
        .map(|name| {
            let mut node = Node::new(
                name.into(),
                ResourceSet {
                    cpus: 8,
                    memory_mb: 16000,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            (name.into(), node)
        })
        .collect();
    let mut spec = reuse_spec("", "n1,n3");
    spec.partition = None;
    spec.num_nodes = 3;
    spec.num_tasks = 3;
    let job = Job::new(1, spec);
    let occupied = HashSet::from(["n1".into(), "n2".into(), "n4".into()]);
    let (charge, _) =
        new_distinct_nodes_needed(&job, &occupied, &ReuseView::uncapped(&nodes), false);
    assert_eq!(charge, 1);
}

#[test]
fn scoped_quota_probes_keep_a_fallback_when_no_partition_is_configured() {
    let job = Job::new(1, basic_spec("fallback"));
    let probes = quota_probes(&job, &[], true);
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].spec.partition, None);
    assert_eq!(probes[0].spec.num_nodes, job.spec.num_nodes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuse_credits_stay_with_the_partition_that_admitted_them() {
    for other_memory in [128, 512] {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].nodes = "n1,n3".into();
        config.partitions[0].def_mem_per_node = Some(128);
        let mut other = config.partitions[0].clone();
        other.name = "other".into();
        other.default = false;
        other.nodes = "n2,n4".into();
        other.def_mem_per_node = Some(other_memory);
        config.partitions.push(other);
        let cm = test_cluster_with_config(&dir, config).await;
        for name in ["n1", "n2", "n3", "n4"] {
            register_node(&cm, name, 8, 16000);
        }
        install_node_caps(&cm, Some(3), Some(3));
        occupy_node(&cm, 101, "n1", true, true);
        occupy_node(&cm, 102, "n2", true, true);
        let id = submit_and_wait(&cm, reuse_spec("default,other", "n1,n2,n3,n4"));
        let classification = cm.classify_pending_jobs();
        assert!(admitted(&classification).contains(&id));
        let assignments = reuse_assignments(&cm, &classification.jobs);
        let assignment = assignments.iter().find(|a| a.job_id == id).unwrap();
        let expected = match assignment.partition.as_deref() {
            Some("default") => HashSet::from(["n1".to_string(), "n3".to_string()]),
            Some("other") => HashSet::from(["n2".to_string(), "n4".to_string()]),
            partition => panic!("unexpected placement partition {partition:?}"),
        };
        assert_eq!(
            assignment.nodes.iter().cloned().collect::<HashSet<_>>(),
            expected
        );
        let expected_memory = if assignment.partition.as_deref() == Some("other") {
            other_memory
        } else {
            128
        };
        assert!(assignment
            .per_node_alloc
            .values()
            .all(|allocation| allocation.memory_mb == expected_memory));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_and_qos_credits_cannot_substitute_for_each_other() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for name in ["n1", "n2", "n3", "n4"] {
        register_node(&cm, name, 8, 16000);
    }
    install_node_caps(&cm, Some(2), Some(2));
    occupy_node(&cm, 101, "n1", true, false);
    occupy_node(&cm, 102, "n2", true, false);
    occupy_node(&cm, 103, "n3", false, true);
    occupy_node(&cm, 104, "n4", false, true);
    let id = submit_and_wait(&cm, reuse_spec("default", "n1,n2,n3,n4"));
    let classification = cm.classify_pending_jobs();
    assert!(admitted(&classification).contains(&id));
    assert!(reuse_assignments(&cm, &classification.jobs).is_empty());
}
