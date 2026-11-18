// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A partition QOS caps every job in its partition, across accounts and job QOSes.

use super::*;
use spur_core::accounting::QosLimits;

fn node_cap(nodes: u64) -> QosLimits {
    let mut cap = TresRecord::new();
    cap.set(TresType::Node, nodes);
    QosLimits {
        max_tres_per_user: Some(cap),
        ..Default::default()
    }
}

async fn capped_cluster(dir: &TempDir, cap_nodes: u64) -> Arc<ClusterManager> {
    let cm = test_cluster(dir).await;
    for n in ["n1", "n2", "n3", "n4"] {
        register_node(&cm, n, 8, 16000);
    }
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: node_cap(cap_nodes),
        ..Default::default()
    });
    for team in ["qa", "qb"] {
        cm.qos_cache().insert(Qos {
            name: team.into(),
            ..Default::default()
        });
    }
    cm.create_partition(Partition {
        name: "gpu".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();
    cm
}

fn gpu_job(user: &str, account: &str, qos: &str) -> JobSpec {
    JobSpec {
        user: user.into(),
        account: Some(account.into()),
        qos: Some(qos.into()),
        partition: Some("gpu".into()),
        ..basic_spec("loop")
    }
}

fn run_on(cm: &ClusterManager, spec: JobSpec, node: &str) -> JobId {
    start_on_node(cm, submit_and_wait(cm, spec), node)
}

fn schedulable(cm: &ClusterManager, id: JobId) -> Option<Job> {
    cm.pending_jobs_and_tag_reasons()
        .into_iter()
        .find(|j| j.job_id == id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_at_the_partition_cap_is_held_whichever_account_submits() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-b", "qb"), "n2");

    let third = submit_and_wait(&cm, gpu_job("alice", "acct-a", "qa"));

    assert!(schedulable(&cm, third).is_none());
    assert_eq!(
        cm.get_job(third).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partitions_sharing_a_partition_qos_add_up_one_users_usage() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    cm.create_partition(Partition {
        name: "gpu2".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();
    let in_gpu2 = |account: &str, qos: &str| JobSpec {
        partition: Some("gpu2".into()),
        ..gpu_job("alice", account, qos)
    };
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, in_gpu2("acct-b", "qb"), "n2");

    let third = submit_and_wait(&cm, in_gpu2("acct-a", "qa"));

    assert!(schedulable(&cm, third).is_none());
    assert_eq!(
        cm.get_job(third).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

/// "gpu" and "gpu2" share "usercap", with room for one of alice's nodes. Her jobs in
/// each fit it alone, and only the first starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partitions_sharing_a_partition_qos_add_up_one_passs_starts() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 1).await;
    cm.create_partition(Partition {
        name: "gpu2".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();
    let exclusive = |spec: JobSpec| JobSpec {
        exclusive: true,
        ..spec
    };
    let first = submit_and_wait(&cm, exclusive(gpu_job("alice", "acct-a", "qa")));
    let in_gpu2 = JobSpec {
        partition: Some("gpu2".into()),
        ..gpu_job("alice", "acct-b", "qb")
    };
    let second = submit_and_wait(&cm, exclusive(in_gpu2));

    assert_eq!(one_pass(&cm).started(), vec![first]);
    assert_eq!(
        cm.get_job(second).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_job_keeps_charging_the_partition_qos() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    let suspended = run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-b", "qb"), "n2");
    cm.suspend_job(suspended, "alice").unwrap();
    settle(&cm, suspended, JobState::Suspended);

    let third = submit_and_wait(&cm, gpu_job("alice", "acct-a", "qa"));

    assert!(schedulable(&cm, third).is_none());
    assert_eq!(
        cm.get_job(third).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_in_its_kill_wait_charges_the_partition_qos_until_its_node_reports() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    let stopping = run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-b", "qb"), "n2");
    cm.cancel_job(stopping, "alice").unwrap();
    settle(&cm, stopping, JobState::Completing);

    let third = submit_and_wait(&cm, gpu_job("alice", "acct-a", "qa"));
    assert!(schedulable(&cm, third).is_none());

    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: stopping,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 15,
        at: None,
        run_attempt: 0,
    });
    assert!(schedulable(&cm, third).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_job_qos_alone_would_have_let_the_third_job_run() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    cm.update_partition(
        "gpu",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        PartitionLimitsUpdate::default(),
        Some(None),
    )
    .unwrap();
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-b", "qb"), "n2");

    let third = submit_and_wait(&cm, gpu_job("alice", "acct-a", "qa"));

    assert!(schedulable(&cm, third).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_user_is_not_charged_for_a_capped_users_jobs() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-b", "qb"), "n2");

    let bob = submit_and_wait(&cm, gpu_job("bob", "acct-a", "qa"));

    assert!(schedulable(&cm, bob).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_pass_cannot_start_past_the_cap() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    let exclusive = |account: &str, qos: &str| JobSpec {
        exclusive: true,
        ..gpu_job("alice", account, qos)
    };
    run_on(&cm, exclusive("acct-a", "qa"), "n1");
    let a = submit_and_wait(&cm, exclusive("acct-a", "qa"));
    let b = submit_and_wait(&cm, exclusive("acct-b", "qb"));

    let pass = one_pass(&cm);

    assert_eq!(
        pass.started(),
        vec![a],
        "the earlier job takes the last slot"
    );
    assert_eq!(
        cm.get_job(b).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

/// Ten of alice's jobs wait for n1, which bob's job fills, under her one-node cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_waiting_for_a_busy_node_hold_none_of_the_partition_qos() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 1).await;
    let exclusive = |spec: JobSpec| JobSpec {
        exclusive: true,
        ..spec
    };
    let elsewhere = JobSpec {
        partition: Some("default".into()),
        ..gpu_job("bob", "acct-b", "qb")
    };
    run_on(&cm, exclusive(elsewhere), "n1");
    for _ in 0..10 {
        let pinned = JobSpec {
            nodelist: Some("n1".into()),
            ..gpu_job("alice", "acct-a", "qa")
        };
        submit_and_wait(&cm, exclusive(pinned));
    }
    let behind = submit_and_wait(&cm, exclusive(gpu_job("alice", "acct-b", "qb")));

    assert_eq!(one_pass(&cm).started(), vec![behind]);
}

/// "pool" has room for the preemptor and its sibling, but their partition QOS has
/// room for one node of theirs. While the victim stops, the preemptor holds that room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preemptor_holds_its_partition_qos_room_while_its_victim_drains() {
    let dir = TempDir::new().unwrap();
    let cm = pool_over_burst_cluster(&dir).await;
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: node_cap(1),
        ..Default::default()
    });
    cm.create_partition(Partition {
        name: "gpu".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();
    let in_gpu = |spec: JobSpec| JobSpec {
        partition: Some("gpu".into()),
        ..spec
    };
    let victim = run_burst_job(&cm, "n2");
    let preemptor = submit_and_wait(&cm, in_gpu(pool_job("preemptor", 1, Some("n2"))));
    let sibling = submit_and_wait(&cm, in_gpu(pool_job("sibling", 1, None)));
    evict_for(&cm, preemptor, victim).await;

    assert!(one_pass(&cm).started().is_empty());
    assert_eq!(
        cm.get_job(sibling).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );

    report_nodes_released(&cm, victim);
    settle(&cm, victim, JobState::Cancelled);
    cm.tick_preempt_freed_earmarks();
    assert_eq!(one_pass(&cm).started(), vec![preemptor]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_partition_job_at_the_cap_is_narrowed_to_the_uncapped_partition() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 1).await;
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");

    let spec = JobSpec {
        partition: Some("gpu,default".into()),
        ..gpu_job("alice", "acct-b", "qb")
    };
    let id = submit_and_wait(&cm, spec);

    let job = schedulable(&cm, id).expect("the uncapped partition still admits it");
    assert_eq!(job.spec.partition.as_deref(), Some("default"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undefined_partition_qos_holds_jobs_when_accounting_is_on() {
    let view = PartitionQosView {
        by_partition: HashMap::from([("gpu".to_string(), ("nosuch".to_string(), None))]),
        unknown_blocks: true,
        ..Default::default()
    };
    let job = Job::new(1, gpu_job("alice", "acct-a", "qa"));

    assert_eq!(
        view.admit(
            &job,
            &QuotaLedger::default(),
            Placement::Packing(&ReuseView::uncapped(&HashMap::new())),
            &EvictionHolds::default()
        )
        .map(|_| ()),
        Err(PendingReason::InvalidQOS)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_packing_onto_a_node_the_partition_qos_holds_fits_its_group_node_cap() {
    let dir = TempDir::new().unwrap();
    let cm = one_node_group_cap(&dir).await;

    let packed = submit_and_wait(&cm, gpu_job("bob", "acct-b", "qb"));

    let job = schedulable(&cm, packed).expect("fits on the node the QOS already holds");
    assert!(job.preferred_nodes.contains("n1"));
}

async fn one_node_group_cap(dir: &TempDir) -> Arc<ClusterManager> {
    let cm = capped_cluster(dir, 4).await;
    let mut one_node = TresRecord::new();
    one_node.set(TresType::Node, 1);
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: QosLimits {
            grp_tres: Some(one_node),
            ..Default::default()
        },
        ..Default::default()
    });
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    cm
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_too_big_for_the_held_node_is_held_at_the_group_node_cap() {
    let dir = TempDir::new().unwrap();
    let cm = one_node_group_cap(&dir).await;

    let wide = submit_and_wait(
        &cm,
        JobSpec {
            cpus_per_task: 8,
            ..gpu_job("bob", "acct-b", "qb")
        },
    );

    assert!(schedulable(&cm, wide).is_none());
    assert_eq!(
        cm.get_job(wide).unwrap().pending_reason,
        PendingReason::QosGrpNodeLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_that_fit_on_the_held_node_all_start_there_in_one_pass() {
    let dir = TempDir::new().unwrap();
    let cm = one_node_group_cap(&dir).await;

    let first = submit_and_wait(&cm, gpu_job("bob", "acct-b", "qb"));
    let second = submit_and_wait(&cm, gpu_job("carol", "acct-b", "qb"));

    let pass = one_pass(&cm);
    assert_eq!(pass.started(), vec![first, second]);
    assert!(pass.assignments.iter().all(|a| a.nodes == ["n1"]));
}

/// The held node has room for one of the two jobs. The other, hinted to it too, waits
/// for it rather than start on a second node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_jobs_in_one_pass_do_not_both_start_on_the_held_nodes_room() {
    let dir = TempDir::new().unwrap();
    let cm = one_node_group_cap(&dir).await;
    let half_a_node = |user: &str| JobSpec {
        cpus_per_task: 4,
        ..gpu_job(user, "acct-b", "qb")
    };

    let first = submit_and_wait(&cm, half_a_node("bob"));
    let second = submit_and_wait(&cm, half_a_node("carol"));

    let pass = one_pass(&cm);
    assert_eq!(pass.admitted(), vec![first, second]);
    assert_eq!(pass.started(), vec![first]);
    assert_eq!(pass.assignments[0].nodes, ["n1"]);
    assert!(pass.limits.refusals().is_empty());
}

/// `user` running on two nodes under "usercap".
fn two_nodes_under_usercap(user: &str) -> QuotaLedger {
    let running: Vec<Job> = ["n1", "n2"]
        .into_iter()
        .zip(100..)
        .map(|(node, id)| {
            let mut job = Job::new(id, gpu_job(user, "acct-a", "qa"));
            job.state = JobState::Running;
            job.allocated_nodes = vec![node.to_string()];
            job
        })
        .collect();
    let mut ledger = QuotaLedger::default();
    ledger.add_partition_qos(&running, |_| Some("usercap"));
    ledger
}

#[test]
fn a_job_with_no_partition_is_checked_against_the_default_partitions_qos() {
    let view = PartitionQosView {
        by_partition: HashMap::from([(
            "gpu".to_string(),
            (
                "usercap".to_string(),
                Some(Qos {
                    name: "usercap".into(),
                    limits: node_cap(2),
                    ..Default::default()
                }),
            ),
        )]),
        defaults: vec!["gpu".to_string()],
        ..Default::default()
    };
    let job = Job::new(
        1,
        JobSpec {
            partition: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    assert_eq!(
        view.admit(
            &job,
            &two_nodes_under_usercap("alice"),
            Placement::Packing(&ReuseView::uncapped(&HashMap::new())),
            &EvictionHolds::default()
        )
        .map(|_| ()),
        Err(PendingReason::QosMaxNodePerUserLimit)
    );
    assert_eq!(
        view.qos_names(&job),
        BTreeSet::from(["usercap".to_string()])
    );
}

#[test]
fn a_start_placed_in_no_partition_is_checked_by_no_partition_qos() {
    let view = PartitionQosView {
        by_partition: HashMap::from([(
            "gpu".to_string(),
            (
                "usercap".to_string(),
                Some(Qos {
                    name: "usercap".into(),
                    limits: node_cap(2),
                    ..Default::default()
                }),
            ),
        )]),
        defaults: vec!["gpu".to_string()],
        ..Default::default()
    };
    let job = Job::new(1, gpu_job("alice", "acct-a", "qa"));
    let nodes = ["n3".to_string()];
    let on = |partition| Placement::On {
        nodes: &nodes,
        partition,
    };
    let ledger = two_nodes_under_usercap("alice");
    let held = EvictionHolds::default();

    assert!(view.admit(&job, &ledger, on(None), &held).is_ok());
    assert_eq!(
        view.admit(&job, &ledger, on(Some("gpu")), &held)
            .map(|_| ()),
        Err(PendingReason::QosMaxNodePerUserLimit)
    );
}

#[test]
fn a_job_with_no_partition_keeps_only_the_defaults_whose_qos_admits_it() {
    let partitions = vec![
        Partition {
            name: "gpu".into(),
            nodes: "ALL".into(),
            is_default: true,
            qos: Some("usercap".into()),
            ..Default::default()
        },
        Partition {
            name: "cpu".into(),
            nodes: "ALL".into(),
            is_default: true,
            ..Default::default()
        },
    ];
    let view = PartitionQosView {
        by_partition: HashMap::from([(
            "gpu".to_string(),
            (
                "usercap".to_string(),
                Some(Qos {
                    name: "usercap".into(),
                    limits: node_cap(2),
                    ..Default::default()
                }),
            ),
        )]),
        defaults: vec!["gpu".to_string(), "cpu".to_string()],
        ..Default::default()
    };
    let ledger = two_nodes_under_usercap("alice");
    let mut job = Job::new(
        1,
        JobSpec {
            partition: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    let outcomes: Vec<(Job, QuotaVerdict)> = quota_probes(&job, &partitions)
        .into_iter()
        .map(|probe| {
            let partition_qos = view
                .admit(
                    &probe,
                    &ledger,
                    Placement::Packing(&ReuseView::uncapped(&HashMap::new())),
                    &EvictionHolds::default(),
                )
                .expect("the default without a QOS admits it");
            let gate = || GateAdmitted {
                charge: NodeCharge {
                    nodes: 1,
                    credited: HashSet::new(),
                },
                admits_without_credit: true,
            };
            let admitted = QuotaVerdict::Admitted {
                account: gate(),
                partition_qos,
                qos: gate(),
            };
            (probe, admitted)
        })
        .collect();
    QuotaClaim::of_admitted(&outcomes).unwrap().narrow(&mut job);

    assert_eq!(job.spec.partition.as_deref(), Some("cpu"));
}

/// Idle-fill on, "qa" capped at one node for its group and "usercap" at one node per
/// user.
async fn team_capped_idle_fill_cluster(dir: &TempDir) -> Arc<ClusterManager> {
    let mut config = test_config();
    config.scheduler.idle_fill_enabled = true;
    let cm = test_cluster_with_config(dir, config).await;
    for n in ["n1", "n2", "n3", "n4"] {
        register_node(&cm, n, 8, 16000);
    }
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: node_cap(1),
        ..Default::default()
    });
    let mut team_cap = TresRecord::new();
    team_cap.set(TresType::Node, 1);
    cm.qos_cache().insert(Qos {
        name: "qa".into(),
        limits: QosLimits {
            grp_tres: Some(team_cap),
            ..Default::default()
        },
        ..Default::default()
    });
    cm.create_partition(Partition {
        name: "gpu".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();
    cm
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_over_its_team_node_cap_is_not_lent_past_the_partition_cap() {
    let dir = TempDir::new().unwrap();
    let cm = team_capped_idle_fill_cluster(&dir).await;
    let exclusive = |spec: JobSpec| JobSpec {
        exclusive: true,
        ..spec
    };
    run_on(&cm, exclusive(gpu_job("alice", "acct-a", "qa")), "n1");

    let over = submit_and_wait(&cm, exclusive(gpu_job("alice", "acct-a", "qa")));
    let (_, lent) = cm.pending_jobs_with_idle_fill_candidates();

    assert!(lent.iter().all(|j| j.job_id != over));
    assert_eq!(
        cm.get_job(over).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

/// alice's "qa" job fills n1, her team's one node, and her "qb" job is on n2, the one
/// node "usercap" lets its jobs hold. Her next "qa" job fits "usercap" only by joining
/// n2, and a borrower can't be steered there, so it is not lent a node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_its_partition_qos_admits_only_on_a_node_it_holds_is_not_lent() {
    let dir = TempDir::new().unwrap();
    let cm = team_capped_idle_fill_cluster(&dir).await;
    let mut one_node = TresRecord::new();
    one_node.set(TresType::Node, 1);
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: QosLimits {
            grp_tres: Some(one_node),
            ..Default::default()
        },
        ..Default::default()
    });
    cm.qos_cache().insert(Qos {
        name: "qb".into(),
        ..Default::default()
    });
    let filling = JobSpec {
        partition: Some("default".into()),
        cpus_per_task: 8,
        ..gpu_job("alice", "acct-a", "qa")
    };
    start_on(&cm, submit_and_wait(&cm, filling), "n1");
    run_on(&cm, gpu_job("alice", "acct-a", "qb"), "n2");

    let over = JobSpec {
        cpus_per_task: 4,
        time_limit: Some(chrono::Duration::hours(1)),
        ..gpu_job("alice", "acct-a", "qa")
    };
    let over = submit_and_wait(&cm, over);
    let (_, lent) = cm.pending_jobs_with_idle_fill_candidates();

    assert!(lent.iter().all(|j| j.job_id != over));
    assert_eq!(
        cm.get_job(over).unwrap().pending_reason,
        PendingReason::QosGrpNodeLimit
    );
}

/// "qa" holds n1, in "gpu", with room to share. alice is at "usercap" in "gpu", so her
/// next "qa" job may run only in "cpu", where it needs a node "qa" has no room for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_gets_no_packing_credit_in_a_partition_its_partition_qos_refuses() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for n in ["n1", "n2", "n3", "n4"] {
        register_node(&cm, n, 8, 16000);
    }
    let mut one_node = TresRecord::new();
    one_node.set(TresType::Node, 1);
    for (name, limits) in [
        ("usercap", node_cap(1)),
        (
            "qa",
            QosLimits {
                grp_tres: Some(one_node),
                ..Default::default()
            },
        ),
        ("qb", QosLimits::default()),
    ] {
        cm.qos_cache().insert(Qos {
            name: name.into(),
            limits,
            ..Default::default()
        });
    }
    for (name, nodes, qos) in [("gpu", "n[1-2]", Some("usercap")), ("cpu", "n[3-4]", None)] {
        cm.create_partition(Partition {
            name: name.into(),
            nodes: nodes.into(),
            qos: qos.map(Into::into),
            ..Default::default()
        })
        .unwrap();
    }
    run_on(&cm, gpu_job("bob", "acct-b", "qa"), "n1");
    run_on(&cm, gpu_job("alice", "acct-a", "qb"), "n2");

    let over = JobSpec {
        partition: Some("gpu,cpu".into()),
        ..gpu_job("alice", "acct-a", "qa")
    };
    let over = submit_and_wait(&cm, over);

    assert!(schedulable(&cm, over).is_none());
    assert_eq!(
        cm.get_job(over).unwrap().pending_reason,
        PendingReason::QosGrpNodeLimit
    );
}

/// alice is at "usercap" in "gpu", and bob's "qa" job fills the team's one node. Her
/// next "qa" job may borrow, but only in "cpu", which carries no partition QOS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lent_job_keeps_only_the_partitions_whose_qos_admits_it() {
    let dir = TempDir::new().unwrap();
    let cm = team_capped_idle_fill_cluster(&dir).await;
    cm.qos_cache().insert(Qos {
        name: "qb".into(),
        ..Default::default()
    });
    cm.create_partition(Partition {
        name: "cpu".into(),
        nodes: "ALL".into(),
        ..Default::default()
    })
    .unwrap();
    run_on(&cm, gpu_job("alice", "acct-a", "qb"), "n1");
    let filling = JobSpec {
        partition: Some("default".into()),
        cpus_per_task: 8,
        ..gpu_job("bob", "acct-b", "qa")
    };
    start_on(&cm, submit_and_wait(&cm, filling), "n2");

    let over = JobSpec {
        partition: Some("gpu,cpu".into()),
        time_limit: Some(chrono::Duration::hours(1)),
        ..gpu_job("alice", "acct-a", "qa")
    };
    let over = submit_and_wait(&cm, over);
    let (_, lent) = cm.pending_jobs_with_idle_fill_candidates();

    let lent = lent.iter().find(|j| j.job_id == over).expect("lent a node");
    assert_eq!(lent.spec.partition.as_deref(), Some("cpu"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_users_only_running_job_counts_toward_the_partition_cap_when_borrowed() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 1).await;
    let borrowed = run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    cm.jobs.write().get_mut(&borrowed).unwrap().idle_fill = true;

    let next = submit_and_wait(&cm, gpu_job("alice", "acct-b", "qb"));

    assert!(schedulable(&cm, next).is_none());
    assert_eq!(
        cm.get_job(next).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partition_qos_that_is_also_the_job_qos_still_applies_as_the_job_qos() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    let elsewhere = |spec: JobSpec| JobSpec {
        partition: Some("default".into()),
        ..spec
    };
    run_on(&cm, elsewhere(gpu_job("alice", "acct-a", "usercap")), "n1");
    run_on(&cm, elsewhere(gpu_job("alice", "acct-a", "usercap")), "n2");

    let in_gpu = submit_and_wait(&cm, gpu_job("alice", "acct-a", "usercap"));

    assert!(schedulable(&cm, in_gpu).is_none());
    assert_eq!(
        cm.get_job(in_gpu).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
}

/// "usercap" is both the partition QOS of "gpu" and the job QOS, with room for two of
/// alice's jobs. Each start counts once in each role, so two start and the third waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_counts_once_under_a_qos_that_is_both_its_partition_qos_and_job_qos() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 4).await;
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: QosLimits {
            max_jobs_per_user: Some(2),
            ..Default::default()
        },
        ..Default::default()
    });
    let jobs: Vec<JobId> = (0..3)
        .map(|_| submit_and_wait(&cm, gpu_job("alice", "acct-a", "usercap")))
        .collect();

    assert_eq!(one_pass(&cm).started(), jobs[..2]);
    assert_eq!(
        cm.get_job(jobs[2]).unwrap().pending_reason,
        PendingReason::QoSMaxJobsPerUser
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partition_qos_has_no_effect_with_accounting_disabled() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    assert!(!cm.config().accounting.enabled());
    register_node(&cm, "n1", 8, 16000);
    cm.create_partition(Partition {
        name: "gpu".into(),
        nodes: "ALL".into(),
        qos: Some("usercap".into()),
        ..Default::default()
    })
    .unwrap();

    let id = submit_and_wait(
        &cm,
        JobSpec {
            account: None,
            qos: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    assert!(schedulable(&cm, id).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_with_no_time_limit_defaults_to_the_partition_qos_max_wall() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    cm.qos_cache().insert(Qos {
        name: "usercap".into(),
        limits: QosLimits {
            max_wall_minutes: Some(60),
            ..Default::default()
        },
        ..Default::default()
    });

    let id = submit_and_wait(
        &cm,
        JobSpec {
            time_limit: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    assert_eq!(
        cm.get_job(id).unwrap().spec.time_limit,
        Some(chrono::Duration::minutes(60))
    );
}

#[test]
fn a_partition_update_logged_before_partition_qos_replays_unchanged() {
    const LEGACY: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":"DRAIN","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null}}"#;
    match serde_json::from_str(LEGACY).expect("an old entry must still replay") {
        WalOperation::PartitionUpdate { qos, .. } => assert_eq!(qos, None),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn a_cleared_partition_qos_survives_the_log() {
    for qos in [None, Some(None), Some(Some("usercap".to_string()))] {
        let op = WalOperation::PartitionUpdate {
            name: "gpu".into(),
            nodes: None,
            selector: None,
            state: None,
            max_time_minutes: None,
            default_time_minutes: None,
            max_nodes: None,
            min_nodes: None,
            allow_accounts: None,
            allow_groups: None,
            deny_accounts: None,
            deny_qos: None,
            allow_qos: None,
            priority_tier: None,
            preempt_mode: None,
            is_default: None,
            preempt_exempt_time: None,
            grace_time: None,
            resource_limits: None,
            qos: qos.clone(),
        };
        let json = serde_json::to_string(&op).unwrap();
        match serde_json::from_str(&json).unwrap() {
            WalOperation::PartitionUpdate { qos: replayed, .. } => assert_eq!(replayed, qos),
            _ => panic!("wrong variant"),
        }
    }
}
