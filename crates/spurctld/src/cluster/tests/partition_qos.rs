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
async fn one_pass_cannot_admit_past_the_cap() {
    let dir = TempDir::new().unwrap();
    let cm = capped_cluster(&dir, 2).await;
    run_on(&cm, gpu_job("alice", "acct-a", "qa"), "n1");
    let a = submit_and_wait(&cm, gpu_job("alice", "acct-a", "qa"));
    let b = submit_and_wait(&cm, gpu_job("alice", "acct-b", "qb"));

    let admitted: Vec<JobId> = cm
        .pending_jobs_and_tag_reasons()
        .into_iter()
        .map(|j| j.job_id)
        .filter(|id| *id == a || *id == b)
        .collect();

    assert_eq!(admitted, vec![a], "the earlier job takes the last slot");
    assert_eq!(
        cm.get_job(b).unwrap().pending_reason,
        PendingReason::QosMaxNodePerUserLimit
    );
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
    let mut job = Job::new(1, gpu_job("alice", "acct-a", "qa"));

    assert_eq!(
        view.admit(
            &mut job,
            &ReuseView::uncapped(&HashMap::new()),
            &PassReservations::default()
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
async fn two_jobs_in_one_pass_do_not_both_claim_the_held_node() {
    let dir = TempDir::new().unwrap();
    let cm = one_node_group_cap(&dir).await;

    let first = submit_and_wait(&cm, gpu_job("bob", "acct-b", "qb"));
    let second = submit_and_wait(&cm, gpu_job("carol", "acct-b", "qb"));

    let kept = cm
        .pending_jobs_and_tag_reasons()
        .into_iter()
        .filter(|j| j.job_id == first || j.job_id == second)
        .count();
    assert_eq!(kept, 1);
}

#[test]
fn a_job_with_no_partition_is_checked_against_the_default_partitions_qos() {
    let mut at_cap = TresRecord::new();
    at_cap.set(TresType::Node, 2);
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
        usage: HashMap::from([(
            "usercap".to_string(),
            PartitionQosUsage {
                user_tres: HashMap::from([("alice".to_string(), at_cap)]),
                ..Default::default()
            },
        )]),
        defaults: vec!["gpu".to_string()],
        ..Default::default()
    };
    let mut job = Job::new(
        1,
        JobSpec {
            partition: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    assert_eq!(
        view.admit(
            &mut job,
            &ReuseView::uncapped(&HashMap::new()),
            &PassReservations::default()
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
fn a_job_with_no_partition_keeps_only_the_defaults_whose_qos_admits_it() {
    let mut at_cap = TresRecord::new();
    at_cap.set(TresType::Node, 2);
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
        usage: HashMap::from([(
            "usercap".to_string(),
            PartitionQosUsage {
                user_tres: HashMap::from([("alice".to_string(), at_cap)]),
                ..Default::default()
            },
        )]),
        defaults: vec!["gpu".to_string(), "cpu".to_string()],
        ..Default::default()
    };
    let mut job = Job::new(
        1,
        JobSpec {
            partition: None,
            ..gpu_job("alice", "acct-a", "qa")
        },
    );

    let outcomes: Vec<(Job, QuotaVerdict)> = quota_probes(&job, &partitions, false)
        .into_iter()
        .map(|mut probe| {
            assert!(view
                .admit(
                    &mut probe,
                    &ReuseView::uncapped(&HashMap::new()),
                    &PassReservations::default()
                )
                .is_ok());
            let admitted = QuotaVerdict::Admitted {
                account: AccountAdmitted {
                    charge: 1,
                    admits_without_credit: true,
                },
                partition_qos: PartitionQosAdmitted::default(),
                qos_charge: 1,
            };
            (probe, admitted)
        })
        .collect();
    QuotaClaim::of_admitted(&outcomes).unwrap().narrow(&mut job);

    assert_eq!(job.spec.partition.as_deref(), Some("cpu"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_over_its_team_node_cap_is_not_lent_past_the_partition_cap() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.scheduler.idle_fill_enabled = true;
    let cm = test_cluster_with_config(&dir, config).await;
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
