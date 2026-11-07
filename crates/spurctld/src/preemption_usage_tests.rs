// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::raft::StateMachineApply;
use spur_core::end_reason::CancelProvenance;
use spur_core::job::{JobSpec, PreemptPending};
use spur_core::partition::PreemptMode;
use spur_core::resource::AllocatedDevice;

fn cluster() -> (tempfile::TempDir, ClusterManager) {
    let dir = tempfile::tempdir().expect("temporary state");
    let config = serde_json::from_str(r#"{"cluster_name":"test"}"#).expect("config");
    let cm = ClusterManager::new(config, dir.path()).expect("cluster");
    for id in [1, 2, 10, 11] {
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: id,
            spec: Box::new(JobSpec {
                user: if id < 10 { "victim" } else { "owner" }.into(),
                ..Default::default()
            }),
        });
    }
    (dir, cm)
}

fn reserve(cm: &ClusterManager, id: JobId, attempt: u32, nodes: &[(&str, ResourceAllocations)]) {
    let mut total = ResourceAllocations::default();
    for (_, a) in nodes {
        total.add(a);
    }
    cm.apply_operation(&WalOperation::JobStart {
        preemption_observation: None,
        job_id: id,
        nodes: nodes.iter().map(|(n, _)| n.to_string()).collect(),
        resources: total,
        per_node_alloc: nodes
            .iter()
            .map(|(n, a)| (n.to_string(), a.clone()))
            .collect(),
        srun_step_dispatch: false,
        run_attempt: attempt,
        idle_fill: false,
        at: None,
    });
}

fn start(cm: &ClusterManager, id: JobId, attempt: u32, nodes: &[(&str, ResourceAllocations)]) {
    reserve(cm, id, attempt, nodes);
    assert!(
        cm.apply_operation(&WalOperation::job_state_change_activating(id, attempt))
            .state_change_applied
    );
}

fn scalar() -> ResourceAllocations {
    ResourceAllocations::with_scalar(4, 100)
}

fn preempt(
    cm: &ClusterManager,
    id: JobId,
    victim_attempt: u32,
    owner_attempt: u32,
) -> WalOperation {
    let op = WalOperation::JobPreemptRequeue {
        preemption_claim: Some(
            cm.preemption_claim(&cm.get_job(id).expect("victim"), u64::from(owner_attempt)),
        ),
        job_id: id,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(owner_attempt),
        preempt_qos: None,
        run_attempt: Some(victim_attempt),
        handoff: true,
        begin_time: chrono::DateTime::UNIX_EPOCH,
        at: None,
    };
    assert!(cm.apply_operation(&op).preempt_applied);
    op
}

#[path = "preemption_usage_review_tests.rs"]
mod review;

#[path = "preemption_usage_attempt_tests.rs"]
mod attempts;

fn confirm(cm: &ClusterManager, attempt: u32, released: bool) {
    cm.apply_operation(&WalOperation::preemption_release(10, attempt, released));
}

fn cancel(cm: &ClusterManager, origin: &str) {
    cm.apply_operation(&WalOperation::JobComplete {
        job_id: 10,
        exit_code: -1,
        state: JobState::Cancelled,
        unreported_nodes: Vec::new(),
        cancel: Some(CancelProvenance {
            origin: origin.into(),
            user: "actor".into(),
            uid: Some(1000),
        }),
        controller_initiated: false,
        at: None,
    });
}

#[test]
fn subset_of_victims_is_counted_individually_and_duplicate_entries_do_not_count_twice() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    start(&cm, 2, 1, &[("n2", scalar())]);
    let first = preempt(&cm, 1, 1, 1);
    preempt(&cm, 2, 1, 1);
    cm.apply_operation(&first);
    assert_eq!(cm.preemption_usage().open, 2);
    start(&cm, 10, 1, &[("n2", scalar())]);
    assert_eq!(
        cm.preemption_usage().open,
        2,
        "activation is not workload release"
    );
    confirm(&cm, 1, true);
    confirm(&cm, 1, true);
    let usage = cm.preemption_usage();
    assert_eq!((usage.used, usage.unused_total(), usage.open), (1, 1, 0));
    assert_eq!(
        usage.unused_count(Reason::StartedElsewhere, Initiator::System),
        1
    );
    assert_eq!(cm.get_job(10).expect("owner").preemption_usage, usage);
}

#[test]
fn cancelling_between_victims_preserves_authenticated_initiator() {
    for (origin, initiator) in [
        ("User", Initiator::User),
        ("Admin", Initiator::Admin),
        ("System", Initiator::System),
        ("legacy", Initiator::Unknown),
    ] {
        let (_dir, cm) = cluster();
        start(&cm, 1, 1, &[("n1", scalar())]);
        start(&cm, 2, 1, &[("n2", scalar())]);
        preempt(&cm, 1, 1, 1);
        cancel(&cm, origin);
        preempt(&cm, 2, 1, 1);
        assert_eq!(
            cm.preemption_usage()
                .unused_count(Reason::Cancelled, initiator),
            2
        );
        assert_eq!(cm.preemption_usage().open, 0);
    }
}

#[test]
fn repeated_victim_runs_and_stale_confirmations_use_run_identity() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    confirm(&cm, 1, false);
    start(&cm, 1, 2, &[("n1", scalar())]);
    preempt(&cm, 1, 2, 2);
    confirm(&cm, 1, true);
    assert_eq!(cm.preemption_usage().open, 1);
    start(&cm, 10, 2, &[("n1", scalar())]);
    confirm(&cm, 2, true);
    assert_eq!(
        (
            cm.preemption_usage().used,
            cm.preemption_usage().unused_total()
        ),
        (1, 1)
    );
}

#[test]
fn reservation_and_release_failure_are_not_successful_use() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobEvict {
        job_id: 10,
        detail: Some("release failed".into()),
        at: None,
    });
    confirm(&cm, 1, false);
    assert_eq!(
        cm.preemption_usage()
            .unused_count(Reason::LaunchFailed, Initiator::System),
        1
    );
    assert_eq!(cm.preemption_usage().used, 0);
}

#[test]
fn completion_or_cancellation_before_success_ack_does_not_erase_release_evidence() {
    for cancelled in [false, true] {
        let (_dir, cm) = cluster();
        start(&cm, 1, 1, &[("n1", scalar())]);
        preempt(&cm, 1, 1, 1);
        start(&cm, 10, 1, &[("n1", scalar())]);
        if cancelled {
            cancel(&cm, "User");
        } else {
            cm.apply_operation(&WalOperation::job_complete(10, 0, JobState::Completed));
        }
        assert_eq!(cm.preemption_usage().open, 1);
        confirm(&cm, 1, true);
        assert_eq!(cm.preemption_usage().used, 1);
        assert_eq!(cm.preemption_usage().unused_total(), 0);
    }
}

#[test]
fn grace_opens_only_on_release_and_partial_reports_still_count_one_victim() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar()), ("n2", scalar())]);
    let pending = PreemptPending {
        preemption_claim: None,
        preemptor_run_attempt: Some(1),
        signaled_at: chrono::DateTime::UNIX_EPOCH,
        deadline: chrono::DateTime::UNIX_EPOCH,
        preempted_by: 10,
        preempt_qos: None,
        mode: PreemptMode::Requeue,
        run_attempt: 1,
        requeue_hold_secs: 30,
    };
    cm.apply_operation(&WalOperation::JobPreemptSignaled { job_id: 1, pending });
    assert_eq!(cm.preemption_usage().open, 0);
    let report = WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    };
    cm.apply_operation(&report);
    cm.apply_operation(&report);
    assert_eq!(cm.preemption_usage().open, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    confirm(&cm, 1, true);
    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n2".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    });
    assert_eq!(
        (
            cm.preemption_usage().used,
            cm.preemption_usage().unused_total(),
            cm.preemption_usage().open
        ),
        (1, 0, 0)
    );
    assert!(cm.preemption_usage.read().victims.is_empty());
}

#[test]
fn capacity_released_after_requester_launch_is_unused() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    assert!(
        cm.apply_operation(&WalOperation::JobPreemptCancel {
            preemption_claim: None,
            job_id: 1,
            preempted_by: Some(10),
            preemptor_run_attempt: Some(1),
            preempt_qos: None,
            run_attempt: Some(1),
            at: None,
        })
        .preempt_applied
    );
    assert_eq!(cm.preemption_usage().open, 0);
    start(&cm, 10, 1, &[("n1", scalar())]);
    confirm(&cm, 1, true);
    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    });
    assert_eq!(
        cm.preemption_usage()
            .unused_count(Reason::StartedElsewhere, Initiator::System),
        1
    );
    assert_eq!(cm.preemption_usage().used, 0);
}

#[test]
fn reclaimed_capacity_without_a_handoff_is_excluded() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobPreemptRequeue {
        preemption_claim: None,
        job_id: 1,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(1),
        preempt_qos: None,
        run_attempt: Some(1),
        handoff: false,
        begin_time: chrono::DateTime::UNIX_EPOCH,
        at: None,
    });
    assert_eq!(cm.preemption_usage(), PreemptionUsage::default());
}

#[test]
fn matching_gpu_identity_and_generation_are_required() {
    for (device, generation, expected) in [(7, 3, 1), (8, 3, 0), (7, 4, 0)] {
        let (_dir, cm) = cluster();
        let mut victim = scalar();
        victim.generation = 3;
        victim
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(7)]);
        let mut owner = scalar();
        owner.generation = generation;
        owner
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(device)]);
        start(&cm, 1, 1, &[("n1", victim)]);
        preempt(&cm, 1, 1, 1);
        start(&cm, 10, 1, &[("n1", owner)]);
        confirm(&cm, 1, true);
        assert_eq!(cm.preemption_usage().used, expected);
        assert_eq!(cm.preemption_usage().unused_total(), 1 - expected);
    }
}

#[test]
fn scalar_budget_cannot_credit_unlimited_victims() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    start(&cm, 2, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    preempt(&cm, 2, 1, 1);
    start(
        &cm,
        10,
        1,
        &[("n1", ResourceAllocations::with_scalar(1, 0))],
    );
    confirm(&cm, 1, true);
    assert_eq!(
        (
            cm.preemption_usage().used,
            cm.preemption_usage().unused_total()
        ),
        (1, 1)
    );
}

#[test]
fn snapshot_restores_open_claims_and_lifetime_totals_survive_job_eviction() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    let (_restored_dir, restored) = cluster();
    restored
        .restore_from_snapshot(&cm.snapshot_state().expect("snapshot"))
        .expect("restore");
    start(&restored, 10, 1, &[("n1", scalar())]);
    confirm(&restored, 1, true);
    cancel(&restored, "User");
    restored.apply_operation(&WalOperation::EvictTerminalJobs {
        preemption_release: None,
        before: None,
        at: None,
        job_ids: vec![10],
    });
    let (_final_dir, final_cm) = cluster();
    final_cm
        .restore_from_snapshot(&restored.snapshot_state().expect("snapshot"))
        .expect("restore");
    assert_eq!(final_cm.preemption_usage().used, 1);
    assert_eq!(final_cm.preemption_usage().used, 1);
    assert!(final_cm.preemption_usage.read().requests.is_empty());
}

#[test]
fn frozen_old_snapshot_and_grace_default_to_no_usage_and_no_request_epoch() {
    let (_dir, cm) = cluster();
    cm.restore_from_snapshot(
        br#"{"jobs":[],"nodes":[],"reservations":[],"steps":[],"license_pool":{}}"#,
    )
    .expect("pre-feature snapshot");
    assert_eq!(cm.preemption_usage(), PreemptionUsage::default());
    let grace: PreemptPending = serde_json::from_str(r#"{"signaled_at":"2026-01-01T00:00:00Z","deadline":"2026-01-01T00:00:30Z","preempted_by":10,"preempt_qos":null,"mode":"Requeue","run_attempt":1,"requeue_hold_secs":30}"#).expect("pre-feature grace");
    assert_eq!(grace.preemptor_run_attempt, None);
    let op: WalOperation = serde_json::from_str(
        r#"{"JobStateChange":{"job_id":10,"old_state":"PENDING","new_state":"RUNNING"}}"#,
    )
    .expect("pre-feature WAL");
    assert!(matches!(
        op,
        WalOperation::JobStateChange {
            expected_run_attempt: None,
            ..
        }
    ));
}

#[test]
fn absent_claims_require_no_raft_write() {
    let (_dir, cm) = cluster();
    cm.confirm_preemption_observation(10, 1, Some(1), true)
        .expect("no Raft handle needed for an untracked launch");
}

#[test]
fn success_before_late_release_survives_a_snapshot() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobPreemptCancel {
        preemption_claim: None,
        job_id: 1,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(1),
        preempt_qos: None,
        run_attempt: Some(1),
        at: None,
    });
    start(&cm, 10, 1, &[("n2", scalar())]);
    confirm(&cm, 1, true);
    let (_other_dir, other) = cluster();
    other
        .restore_from_snapshot(&cm.snapshot_state().expect("snapshot"))
        .expect("restore");
    other.apply_operation(&WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    });
    assert_eq!(other.preemption_usage().unused_total(), 1);
    assert_eq!(other.preemption_usage().open, 0);
}

#[test]
fn unresolved_launch_gets_an_accounting_repair_window_before_eviction() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    let now = chrono::DateTime::UNIX_EPOCH + chrono::Duration::hours(1);
    let mut complete = WalOperation::job_complete(10, 0, JobState::Completed);
    complete.stamp_occurred_at(chrono::DateTime::UNIX_EPOCH);
    cm.apply_operation(&complete);
    let op = WalOperation::EvictTerminalJobs {
        preemption_release: None,
        job_ids: vec![10],
        before: Some(now - chrono::Duration::minutes(2)),
        at: Some(now),
    };
    cm.apply_operation(&op);
    assert!(cm.get_job(10).is_some());
    assert_eq!(
        cm.preemption_usage()
            .unused_count(Reason::EndedOther, Initiator::Unknown),
        1
    );
    cm.apply_operation(&op);
    assert!(
        cm.get_job(10).is_some(),
        "duplicate eviction cannot remove the repair window"
    );
    cm.apply_operation(&WalOperation::EvictTerminalJobs {
        preemption_release: None,
        job_ids: vec![10],
        before: Some(now + chrono::Duration::minutes(2)),
        at: Some(now + chrono::Duration::minutes(4)),
    });
    assert!(cm.get_job(10).is_none());
}

#[test]
fn frozen_pre_feature_preemption_and_eviction_entries_remain_readable() {
    let requeue: WalOperation = serde_json::from_str(r#"{"JobPreemptRequeue":{"job_id":1,"begin_time":"2026-01-01T00:00:00Z","preempted_by":10,"handoff":true}}"#).expect("old requeue");
    assert!(matches!(
        requeue,
        WalOperation::JobPreemptRequeue {
            preemptor_run_attempt: None,
            ..
        }
    ));
    let cancel: WalOperation =
        serde_json::from_str(r#"{"JobPreemptCancel":{"job_id":1,"preempted_by":10}}"#)
            .expect("old cancel");
    assert!(matches!(
        cancel,
        WalOperation::JobPreemptCancel {
            preemptor_run_attempt: None,
            ..
        }
    ));
    let eviction: WalOperation =
        serde_json::from_str(r#"{"EvictTerminalJobs":{"job_ids":[1]}}"#).expect("old eviction");
    assert!(matches!(
        eviction,
        WalOperation::EvictTerminalJobs {
            before: None,
            at: None,
            ..
        }
    ));
}

#[test]
fn two_runs_of_one_victim_can_be_open_at_the_same_time() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 1, 2, &[("n1", scalar())]);
    preempt(&cm, 1, 2, 1);
    assert_eq!(cm.preemption_usage().open, 2);
    cancel(&cm, "User");
    assert_eq!(
        cm.preemption_usage()
            .unused_count(Reason::Cancelled, Initiator::User),
        2
    );
    assert_eq!(cm.preemption_usage().open, 0);
    assert!(cm.preemption_usage.read().victims.is_empty());
}

#[test]
fn cancelled_requester_is_retained_until_its_warned_victims_release_capacity() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobPreemptCancel {
        preemption_claim: None,
        job_id: 1,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(1),
        preempt_qos: None,
        run_attempt: Some(1),
        at: None,
    });
    cancel(&cm, "User");
    cm.apply_operation(&WalOperation::EvictTerminalJobs {
        preemption_release: None,
        job_ids: vec![10],
        before: None,
        at: None,
    });
    assert!(cm.get_job(10).is_some());
    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    });
    assert_eq!(
        cm.get_job(10)
            .expect("owner")
            .preemption_usage
            .unused_total(),
        1
    );
}

#[test]
fn a_backoff_that_loses_to_activation_does_not_condemn_the_launch() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobDispatchBackoff {
        job_id: 10,
        begin_time: chrono::DateTime::UNIX_EPOCH,
        spare_requeue_budget: false,
        run_attempt: 1,
        at_max_requeue: false,
    });
    assert_eq!(cm.preemption_usage().open, 1);
    confirm(&cm, 1, true);
    assert_eq!(cm.preemption_usage().used, 1);
}

#[test]
fn a_controller_ignoring_the_confirmation_cannot_resume_a_suspended_job() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobSuspend {
        job_id: 10,
        at: chrono::DateTime::UNIX_EPOCH,
        preempted_by: None,
        preempt_qos: None,
        by: None,
    });
    let confirmation = WalOperation::preemption_release(10, 1, true);
    cm.apply_job_operation(&confirmation);
    assert_eq!(
        cm.get_job(10).expect("requester").state,
        JobState::Suspended
    );
    assert_eq!(cm.preemption_usage().open, 1);
    cm.apply_operation(&confirmation);
    assert_eq!(
        cm.get_job(10).expect("requester").state,
        JobState::Suspended
    );
    assert_eq!(cm.preemption_usage().used, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounting_acknowledgement_gates_eviction_and_is_not_in_snapshots() {
    use crate::accounting::preemption::Checkpoint;
    let (dir, cm) = cluster();
    let mut cfg = (*cm.config()).clone();
    cfg.accounting.database_url = "postgres://unused-test".into();
    cfg.controller.terminal_job_retention_secs = 0;
    let old = chrono::Utc::now() - chrono::Duration::days(1);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&cm.snapshot_state().expect("snapshot")).expect("JSON");
    let mut job = cm.get_job(10).expect("requester");
    job.state = JobState::Completed;
    job.end_time = Some(old);
    job.preemption_usage.used = 3;
    job.preemption_usage_updated_at = Some(old);
    snapshot["jobs"] = serde_json::json!([job]);
    let path = dir.path().join("accounting");
    let other = std::sync::Arc::new(ClusterManager::new(cfg, &path).expect("cluster"));
    let handle = crate::raft::start_raft(1, &["[::1]:0".into()], &path, other.clone())
        .await
        .expect("Raft");
    handle
        .raft
        .wait(None)
        .metrics(
            |metrics| metrics.current_leader == Some(1),
            "leader elected",
        )
        .await
        .expect("leader");
    other.set_raft(handle.raft);
    other
        .restore_from_snapshot(&serde_json::to_vec(&snapshot).expect("snapshot bytes"))
        .expect("restore");
    other.evict_expired_terminal_jobs();
    assert!(
        other.get_job(10).is_some(),
        "an outage must retain the unexported job"
    );
    let checkpoint = Checkpoint::from(&other.get_job(10).expect("job"));
    let mut stale = checkpoint.clone();
    stale.usage.used = 2;
    other.acknowledge_preemption_checkpoint(&stale);
    assert_eq!(other.pending_preemption_checkpoints(100).len(), 1);
    other.acknowledge_preemption_checkpoint(&checkpoint);
    assert!(other.pending_preemption_checkpoints(100).is_empty());
    let (_, restarted) = cluster();
    restarted
        .restore_from_snapshot(&other.snapshot_state().expect("snapshot"))
        .expect("restore");
    assert_eq!(
        restarted.pending_preemption_checkpoints(100).len(),
        1,
        "restart must recheck PostgreSQL"
    );
    other
        .restore_from_snapshot(&other.snapshot_state().expect("snapshot"))
        .expect("restore over existing cache");
    assert_eq!(other.pending_preemption_checkpoints(100).len(), 1);
    other.acknowledge_preemption_checkpoint(&checkpoint);
    other.evict_expired_terminal_jobs();
    assert!(other.get_job(10).is_none());
    assert!(other.preemption_accounted.read().is_empty());
}

#[test]
fn legacy_user_totals_deserialize_without_retaining_a_lifetime_map() {
    let frozen = br#"{"cluster":{"used":5,"open":1},"users":{"alice":{"used":5,"open":1}},"requests":{},"victims":{}}"#;
    let ledger: crate::cluster::preemption_usage::UsageLedger =
        serde_json::from_slice(frozen).expect("legacy ledger");
    assert_eq!(ledger.cluster.used, 5);
    assert_eq!(ledger.cluster.open, 1);
    let saved = serde_json::to_value(ledger).expect("saved ledger");
    assert!(saved.get("users").is_none());
}

#[test]
fn accounting_identity_survives_submission_replay() {
    use crate::accounting::preemption::Checkpoint;
    let (_, first) = cluster();
    let (_, second) = cluster();
    let mut operation = WalOperation::JobSubmit {
        job_id: 21,
        spec: Box::default(),
    };
    operation.stamp_occurred_at(chrono::DateTime::UNIX_EPOCH);
    let replay: WalOperation =
        serde_json::from_slice(&serde_json::to_vec(&operation).expect("entry")).expect("replay");
    first.apply_operation(&operation);
    second.apply_operation(&replay);
    let first_job = first.get_job(21).expect("job");
    let second_job = second.get_job(21).expect("job");
    assert_eq!(first_job.submit_time, chrono::DateTime::UNIX_EPOCH);
    assert_eq!(first_job.submit_time, second_job.submit_time);
    assert_eq!(
        Checkpoint::from(&first_job).incarnation,
        Checkpoint::from(&second_job).incarnation
    );
    let legacy = WalOperation::JobSubmit {
        job_id: 22,
        spec: Box::default(),
    };
    first.apply_operation(&legacy);
    second.apply_operation(&legacy);
    assert_eq!(
        Checkpoint::from(&first.get_job(22).expect("job")).incarnation,
        "legacy"
    );
    assert_eq!(
        Checkpoint::from(&second.get_job(22).expect("job")).incarnation,
        "legacy"
    );
}
