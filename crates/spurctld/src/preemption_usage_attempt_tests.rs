// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn preempt_observation(cm: &ClusterManager, victim_id: JobId, observation: u64, late: bool) {
    let victim = cm.get_job(victim_id).expect("victim");
    let claim = Some(cm.preemption_claim(&victim, observation));
    let owner_attempt = Some(cm.get_job(10).expect("requester").run_attempt + 1);
    let op = if late {
        WalOperation::JobPreemptCancel {
            preemption_claim: claim,
            preemptor_run_attempt: owner_attempt,
            job_id: victim_id,
            preempted_by: Some(10),
            preempt_qos: None,
            at: None,
            run_attempt: Some(victim.run_attempt),
        }
    } else {
        WalOperation::JobPreemptRequeue {
            preemption_claim: claim,
            preemptor_run_attempt: owner_attempt,
            job_id: victim_id,
            begin_time: chrono::DateTime::UNIX_EPOCH,
            handoff: true,
            preempted_by: Some(10),
            preempt_qos: None,
            run_attempt: Some(victim.run_attempt),
            at: None,
        }
    };
    assert!(cm.apply_operation(&op).preempt_applied);
}

fn confirm_observation(cm: &ClusterManager, observation: u64, run_attempt: u32, released: bool) {
    let mut op = WalOperation::preemption_release(10, run_attempt, released);
    let WalOperation::EvictTerminalJobs {
        preemption_release: Some(release),
        ..
    } = &mut op
    else {
        panic!("release operation");
    };
    release.observation_id = Some(observation);
    cm.apply_operation(&op);
}

fn start_observation(cm: &ClusterManager, observation: u64, run_attempt: u32, node: &str) {
    assert!(
        cm.apply_operation(&WalOperation::JobStart {
            preemption_observation: Some(observation),
            effective_partition: None,
            memory_adjustment: None,
            job_id: 10,
            nodes: vec![node.into()],
            resources: scalar(),
            per_node_alloc: [(node.into(), scalar())].into_iter().collect(),
            srun_step_dispatch: false,
            run_attempt,
            idle_fill: false,
            at: None,
        })
        .job_started
    );
    assert!(
        cm.apply_operation(&WalOperation::job_state_change_activating(10, run_attempt))
            .state_change_applied
    );
}

fn report_victim_node(cm: &ClusterManager, node: &str) {
    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: node.into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 1,
        at: None,
    });
}

fn user_requeue_requester(cm: &ClusterManager) {
    cm.apply_operation(&WalOperation::JobUserRequeue {
        job_id: 10,
        hold: false,
        begin_time: None,
        by: None,
        at: Some(chrono::DateTime::UNIX_EPOCH),
    });
    assert_eq!(cm.get_job(10).expect("requester").state, JobState::Pending);
}

fn restore(cm: &ClusterManager) -> (tempfile::TempDir, ClusterManager) {
    let (dir, restored) = cluster();
    restored
        .restore_from_snapshot(&cm.snapshot_state().expect("snapshot"))
        .expect("restore");
    (dir, restored)
}

#[test]
fn early_failure_and_pending_cancel_do_not_reuse_an_observation_on_the_same_run() {
    for cancelled in [false, true] {
        let (_dir, cm) = cluster();
        start(&cm, 1, 1, &[("n1", scalar()), ("n2", scalar())]);
        let first = cm.next_preemption_observation(10);
        preempt_observation(&cm, 1, first, true);
        report_victim_node(&cm, "n1");
        assert_eq!(cm.preemption_usage().open, 1);

        let (reason, initiator) = if cancelled {
            cancel(&cm, "User");
            (Reason::Cancelled, Initiator::User)
        } else {
            confirm_observation(&cm, first, 1, false);
            (Reason::LaunchFailed, Initiator::System)
        };
        assert_eq!(cm.get_job(10).expect("requester").run_attempt, 0);
        assert_eq!(cm.preemption_observation(10), None);
        assert_eq!(cm.preemption_usage().unused_count(reason, initiator), 1);

        let (_restored_dir, cm) = restore(&cm);
        if cancelled {
            user_requeue_requester(&cm);
        }
        assert_eq!(cm.get_job(10).expect("requester").run_attempt, 0);
        start(&cm, 2, 1, &[("n3", scalar())]);
        let second = cm.next_preemption_observation(10);
        assert!(second > first);
        preempt_observation(&cm, 2, second, false);
        assert_eq!(cm.preemption_observation(10), Some(second));
        assert_eq!(cm.preemption_usage().open, 1);

        confirm_observation(&cm, first, 1, true);
        confirm_observation(&cm, first, 1, false);
        assert_eq!(cm.preemption_usage().open, 1);
        assert_eq!(cm.preemption_usage().unused_count(reason, initiator), 1);

        let (_retry_dir, cm) = restore(&cm);
        assert_eq!(cm.preemption_observation(10), Some(second));
        start_observation(&cm, second, 1, "n3");
        confirm_observation(&cm, second, 1, true);
        report_victim_node(&cm, "n2");
        confirm_observation(&cm, first, 1, false);
        let usage = cm.preemption_usage();
        assert_eq!((usage.used, usage.unused_total(), usage.open), (1, 1, 0));
        assert_eq!(usage.unused_count(reason, initiator), 1);
        assert_eq!(cm.get_job(10).expect("requester").run_attempt, 1);
        assert_eq!(cm.get_job(10).expect("requester").preemption_usage, usage);
        assert!(cm.preemption_usage.read().requests.is_empty());
        assert!(cm.preemption_usage.read().victims.is_empty());
    }
}

#[test]
fn a_new_run_does_not_discard_an_activated_observations_late_success() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    let first = cm.next_preemption_observation(10);
    preempt_observation(&cm, 1, first, false);
    start_observation(&cm, first, 1, "n1");
    user_requeue_requester(&cm);
    assert_eq!(cm.preemption_observation(10), None);
    assert_eq!(cm.preemption_usage().open, 1);

    start(&cm, 2, 1, &[("n2", scalar())]);
    let second = cm.next_preemption_observation(10);
    assert!(second > first);
    preempt_observation(&cm, 2, second, false);
    start_observation(&cm, second, 2, "n2");
    assert_eq!(cm.preemption_usage().open, 2);

    let (_restored_dir, cm) = restore(&cm);
    confirm_observation(&cm, first, 1, true);
    assert_eq!(cm.preemption_observation(10), Some(second));
    assert_eq!(cm.preemption_usage().used, 1);
    assert_eq!(cm.preemption_usage().open, 1);
    confirm_observation(&cm, first, 1, false);
    confirm_observation(&cm, second, 2, true);
    let usage = cm.preemption_usage();
    assert_eq!((usage.used, usage.unused_total(), usage.open), (2, 0, 0));
    assert!(cm.preemption_usage.read().requests.is_empty());
    assert!(cm.preemption_usage.read().victims.is_empty());
}

#[test]
fn delayed_launch_ack_preserves_its_first_cancellation_after_requeue() {
    for released in [false, true] {
        for cancelled_again in [false, true] {
            for legacy_confirmation in [false, true] {
                let (_dir, cm) = cluster();
                start(&cm, 1, 1, &[("n1", scalar())]);
                let observation = cm.next_preemption_observation(10);
                preempt_observation(&cm, 1, observation, false);
                start_observation(&cm, observation, 1, "n1");
                cancel(&cm, "User");
                user_requeue_requester(&cm);
                assert!(cm.get_job(10).expect("requester").cancelled_by.is_none());
                if cancelled_again {
                    cancel(&cm, "Admin");
                }
                assert_eq!(cm.preemption_usage().open, 1);

                let (_restored_dir, cm) = restore(&cm);
                if legacy_confirmation {
                    cm.apply_operation(&WalOperation::preemption_release(10, 1, released));
                } else {
                    confirm_observation(&cm, observation, 1, released);
                }
                let usage = cm.preemption_usage();
                let expected_used = u64::from(released);
                let expected_cancelled = u64::from(!released);
                assert_eq!(
                    (usage.used, usage.unused_total(), usage.open),
                    (expected_used, expected_cancelled, 0)
                );
                assert_eq!(
                    usage.unused_count(Reason::Cancelled, Initiator::User),
                    expected_cancelled
                );
            }
        }
    }
}

#[test]
fn later_cancellation_does_not_relabel_an_earlier_requeued_launch_failure() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    let observation = cm.next_preemption_observation(10);
    preempt_observation(&cm, 1, observation, false);
    start_observation(&cm, observation, 1, "n1");
    user_requeue_requester(&cm);
    cancel(&cm, "Admin");
    assert_eq!(cm.get_job(10).expect("requester").run_attempt, 1);

    let (_restored_dir, cm) = restore(&cm);
    confirm_observation(&cm, observation, 1, false);
    let usage = cm.preemption_usage();
    assert_eq!((usage.used, usage.unused_total(), usage.open), (0, 1, 0));
    assert_eq!(
        usage.unused_count(Reason::LaunchFailed, Initiator::System),
        1
    );
}

#[test]
fn observation_high_water_survives_settlement_until_the_requester_is_evicted() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    let observation = cm.next_preemption_observation(10);
    preempt_observation(&cm, 1, observation, false);
    start_observation(&cm, observation, 1, "n1");
    confirm_observation(&cm, observation, 1, true);
    assert!(cm.preemption_usage.read().requests.is_empty());

    let (_restored_dir, cm) = restore(&cm);
    assert!(cm.next_preemption_observation(10) > observation);
    cm.apply_operation(&WalOperation::EvictTerminalJobs {
        preemption_release: None,
        job_ids: vec![10],
        before: None,
        at: None,
    });
    assert!(cm.get_job(10).is_some());
    assert_eq!(
        cm.preemption_usage.read().high_water.get(&10),
        Some(&observation)
    );

    cancel(&cm, "User");
    cm.apply_operation(&WalOperation::EvictTerminalJobs {
        preemption_release: None,
        job_ids: vec![10],
        before: None,
        at: None,
    });
    assert!(cm.get_job(10).is_none());
    assert!(!cm.preemption_usage.read().high_water.contains_key(&10));
    assert_eq!(cm.preemption_observation(10), None);
    assert_eq!(cm.preemption_usage().used, 1);
    assert_eq!(cm.preemption_usage().used, 1);
}
