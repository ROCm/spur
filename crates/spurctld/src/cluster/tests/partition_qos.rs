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
        view.admit(&mut job, &PassReservations::default()),
        Err(PendingReason::InvalidQOS)
    );
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
