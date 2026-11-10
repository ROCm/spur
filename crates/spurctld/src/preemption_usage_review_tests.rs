// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

fn gpu(device_id: u64) -> ResourceAllocations {
    let mut allocation = scalar();
    allocation
        .devices
        .insert("gpu".into(), vec![AllocatedDevice::injectable(device_id)]);
    allocation
}

fn register_gpu_node(cm: &ClusterManager) {
    cm.apply_operation(&WalOperation::NodeRegister {
        name: "n1".into(),
        hostname: "n1".into(),
        resources: ResourceSet {
            cpus: 8,
            memory_mb: 200,
            gpus: [7, 8]
                .into_iter()
                .enumerate()
                .map(|(index, stable_id)| GpuResource {
                    device_id: index as u32,
                    gpu_type: "test".into(),
                    memory_mb: 100,
                    peer_gpus: Vec::new(),
                    link_type: GpuLinkType::PCIe,
                    stable_id,
                    topology: None,
                })
                .collect(),
            ..Default::default()
        },
        address: String::new(),
        port: 6818,
        wg_pubkey: String::new(),
        version: String::new(),
        labels: Default::default(),
        source: spur_core::node::NodeSource::NativeHost,
        capabilities: Vec::new(),
    });
}

fn canonical_preemption(cm: &ClusterManager, observation: u64) -> WalOperation {
    let victim = cm.get_job(1).expect("victim");
    WalOperation::JobPreemptRequeue {
        preemption_claim: Some(cm.preemption_claim(&victim, observation)),
        job_id: victim.job_id,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(1),
        preempt_qos: None,
        run_attempt: Some(victim.run_attempt),
        handoff: true,
        begin_time: chrono::DateTime::UNIX_EPOCH,
        at: Some(chrono::DateTime::UNIX_EPOCH),
    }
}

fn requester_reservation(observation: u64, allocation: ResourceAllocations) -> WalOperation {
    WalOperation::JobStart {
        preemption_observation: Some(observation),
        effective_partition: None,
        memory_adjustment: None,
        job_id: 10,
        nodes: vec!["n1".into()],
        resources: allocation.clone(),
        per_node_alloc: [("n1".into(), allocation)].into_iter().collect(),
        srun_step_dispatch: false,
        run_attempt: 1,
        idle_fill: false,
        at: Some(chrono::DateTime::UNIX_EPOCH),
    }
}

fn release_confirmation(observation: u64) -> WalOperation {
    let mut operation = WalOperation::preemption_release(10, 1, true);
    let WalOperation::EvictTerminalJobs {
        preemption_release: Some(release),
        ..
    } = &mut operation
    else {
        panic!("release confirmation operation");
    };
    release.observation_id = Some(observation);
    operation.stamp_occurred_at(chrono::DateTime::UNIX_EPOCH);
    operation
}

fn restored(cm: &ClusterManager) -> (tempfile::TempDir, ClusterManager) {
    let (dir, other) = cluster();
    other
        .restore_from_snapshot(&cm.snapshot_state().expect("snapshot"))
        .expect("restore snapshot");
    (dir, other)
}

fn replay(cm: &ClusterManager, operation: &WalOperation) -> ClientResponse {
    let bytes = serde_json::to_vec(operation).expect("serialize WAL");
    cm.apply_operation(&serde_json::from_slice(&bytes).expect("deserialize WAL"))
}

fn assert_usage(cm: &ClusterManager, used: u64, unused: u64) {
    let usage = cm.preemption_usage();
    assert_eq!(
        (usage.used, usage.unused_total(), usage.open),
        (used, unused, 0)
    );
    assert_eq!(
        usage.unused_count(Reason::StartedElsewhere, Initiator::System),
        unused
    );
    assert_eq!(cm.get_job(10).expect("requester").preemption_usage, usage);
}

fn ledger_snapshot(cm: &ClusterManager) -> serde_json::Value {
    let snapshot: serde_json::Value =
        serde_json::from_slice(&cm.snapshot_state().expect("snapshot")).expect("snapshot JSON");
    snapshot["preemption_usage"].clone()
}

#[test]
fn restored_aggregate_gpu_allocation_cannot_credit_a_cpu_only_requester() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", gpu(7))]);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&cm.snapshot_state().expect("snapshot")).expect("snapshot JSON");
    let victim = snapshot["jobs"]
        .as_array_mut()
        .expect("jobs")
        .iter_mut()
        .find(|job| job["job_id"] == 1)
        .expect("victim fixture");
    victim
        .as_object_mut()
        .expect("job object")
        .remove("per_node_alloc");
    cm.restore_from_snapshot(&serde_json::to_vec(&snapshot).expect("legacy snapshot"))
        .expect("restore aggregate-only allocation");
    let victim = cm.get_job(1).expect("restored victim");
    assert!(victim.per_node_alloc.is_empty());
    assert!(victim
        .allocated_resources
        .expect("aggregate allocation")
        .has_devices());

    assert!(replay(&cm, &canonical_preemption(&cm, 41)).preempt_applied);
    assert_eq!(cm.preemption_usage().open, 1);
    replay(&cm, &requester_reservation(41, scalar()));
    replay(&cm, &WalOperation::job_state_change_activating(10, 1));
    replay(&cm, &release_confirmation(41));
    assert_usage(&cm, 0, 1);
    let (_restored_dir, other) = restored(&cm);
    replay(&other, &release_confirmation(41));
    assert_usage(&other, 0, 1);
    assert_eq!(ledger_snapshot(&cm), ledger_snapshot(&other));
}

#[test]
fn canonical_victim_claim_survives_leader_only_gpu_reconciliation_and_replay() {
    let (_dir, leader) = cluster();
    register_gpu_node(&leader);
    start(&leader, 1, 1, &[("n1", gpu(0))]);
    let (_follower_dir, follower) = restored(&leader);
    leader.reconcile_node_gpu_allocations("n1", &[(1, vec![7])]);
    assert_eq!(
        leader.get_job(1).expect("victim").per_node_alloc["n1"].device_ids("gpu"),
        vec![7]
    );
    assert_eq!(
        follower.get_job(1).expect("victim").per_node_alloc["n1"].device_ids("gpu"),
        vec![0]
    );

    let preemption = canonical_preemption(&leader, 42);
    for cm in [&leader, &follower] {
        assert!(replay(cm, &preemption).preempt_applied);
        assert!(!replay(cm, &preemption).preempt_applied);
        assert_eq!(cm.preemption_usage().open, 1);
    }
    assert_eq!(ledger_snapshot(&leader), ledger_snapshot(&follower));
    let (_restored_dir, other) = restored(&follower);
    for cm in [&leader, &follower, &other] {
        replay(cm, &requester_reservation(42, gpu(7)));
        replay(cm, &WalOperation::job_state_change_activating(10, 1));
        replay(cm, &release_confirmation(42));
        replay(cm, &release_confirmation(42));
        assert_usage(cm, 1, 0);
    }
    assert_eq!(ledger_snapshot(&leader), ledger_snapshot(&follower));
    assert_eq!(ledger_snapshot(&leader), ledger_snapshot(&other));
}

#[test]
fn requester_activation_uses_reserved_wal_allocation_after_gpu_reconciliation() {
    let (_dir, leader) = cluster();
    register_gpu_node(&leader);
    start(&leader, 1, 1, &[("n1", gpu(7))]);
    let (_follower_dir, follower) = restored(&leader);
    let preemption = canonical_preemption(&leader, 43);
    let reservation = requester_reservation(43, gpu(7));
    for cm in [&leader, &follower] {
        assert!(replay(cm, &preemption).preempt_applied);
        assert!(replay(cm, &reservation).job_started);
    }
    leader.reconcile_node_gpu_allocations("n1", &[(10, vec![8])]);
    assert_eq!(
        leader.get_job(10).expect("requester").per_node_alloc["n1"].device_ids("gpu"),
        vec![8]
    );
    assert_eq!(
        follower.get_job(10).expect("requester").per_node_alloc["n1"].device_ids("gpu"),
        vec![7]
    );
    let (_restored_dir, other) = restored(&leader);

    for cm in [&leader, &follower, &other] {
        assert!(replay(cm, &WalOperation::job_state_change_activating(10, 1)).state_change_applied);
        replay(cm, &release_confirmation(43));
        assert_usage(cm, 1, 0);
    }
    assert_eq!(ledger_snapshot(&leader), ledger_snapshot(&follower));
    assert_eq!(ledger_snapshot(&leader), ledger_snapshot(&other));
}

#[test]
fn frozen_wal_payloads_default_new_observation_metadata() {
    let preemption: WalOperation = serde_json::from_str(
        r#"{"JobPreemptRequeue":{"job_id":1,"begin_time":"2026-01-01T00:00:00Z","preempted_by":10,"handoff":true,"preemptor_run_attempt":1}}"#,
    ).expect("old preemption WAL");
    assert!(matches!(
        preemption,
        WalOperation::JobPreemptRequeue {
            preemption_claim: None,
            ..
        }
    ));
    let reservation: WalOperation = serde_json::from_str(
        r#"{"JobStart":{"job_id":10,"nodes":["n1"],"resources":{"cpus":4,"memory_mb":100,"devices":{}},"run_attempt":1}}"#,
    ).expect("old reservation WAL");
    assert!(matches!(
        reservation,
        WalOperation::JobStart {
            preemption_observation: None,
            ..
        }
    ));
    let confirmation: WalOperation = serde_json::from_str(
        r#"{"EvictTerminalJobs":{"job_ids":[],"preemption_release":{"job_id":10,"run_attempt":1,"released":true}}}"#,
    ).expect("old confirmation WAL");
    let WalOperation::EvictTerminalJobs {
        preemption_release: Some(release),
        ..
    } = confirmation
    else {
        panic!("release confirmation");
    };
    assert_eq!(release.observation_id, None);
    assert!(!release.unconfirmed);
}

#[test]
fn frozen_ledger_snapshot_defaults_attempt_tracking_and_closure_provenance() {
    let ledger: UsageLedger = serde_json::from_str(
        r#"{"requests":{"10":{"1":{"user":"owner","victims":[[1,1]],"activated":null,"outcome":null}}},"victims":{"1":{"1":{"run_attempt":1,"owner":10,"owner_attempt":1,"allocations":{},"freed":[],"opened":false,"settled":false}}}}"#,
    ).expect("old ledger snapshot");
    assert!(ledger.high_water.is_empty());
    assert!(ledger.current.is_empty());
    let request = &ledger.requests[&10][&1];
    assert!(!request.closed);
    assert!(request.run_attempt.is_none());
    assert!(request.reserved.is_none());
    assert!(request.closure_cause.is_none());
    assert_eq!(ledger.victims[&1][&1].owner_attempt, 1);
}

#[test]
fn legacy_preemptions_do_not_depend_on_snapshot_position() {
    for kind in [
        "JobPreemptRequeue",
        "JobPreemptCancel",
        "JobPreemptSignaled",
    ] {
        let (_dir, replayed) = cluster();
        start(&replayed, 1, 1, &[("n1", scalar())]);
        let (_other_dir, snapshotted) = restored(&replayed);
        let payload = match kind {
            "JobPreemptSignaled" => serde_json::json!({kind: {"job_id": 1, "pending": {
                "signaled_at": "2026-01-01T00:00:00Z", "deadline": "2026-01-01T00:00:00Z",
                "preempted_by": 10, "mode": "Requeue", "run_attempt": 1, "requeue_hold_secs": 0
            }}}),
            _ => serde_json::json!({kind: {"job_id": 1, "begin_time": "2026-01-01T00:00:00Z",
                "preempted_by": 10, "handoff": true, "run_attempt": 1}}),
        };
        let old: WalOperation = serde_json::from_value(payload).expect("legacy entry");
        snapshotted.apply_job_operation(&old);
        let (_snapshot_dir, snapshotted) = restored(&snapshotted);
        replayed.apply_operation(&old);
        for cm in [&replayed, &snapshotted] {
            cm.apply_operation(&WalOperation::JobNodeComplete {
                job_id: 1,
                node_name: "n1".into(),
                exit_code: 0,
                signal: 0,
                run_attempt: 1,
                at: None,
            });
            start(cm, 10, 1, &[("n1", scalar())]);
            assert_eq!(cm.preemption_usage(), PreemptionUsage::default());
            assert!(!cm.preemption_usage.read().tracks_request(10));
        }
        assert_eq!(ledger_snapshot(&replayed), ledger_snapshot(&snapshotted));
    }
}

#[test]
fn terminal_request_with_draining_victim_does_not_propose_eviction() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    cm.apply_operation(&WalOperation::JobPreemptCancel {
        job_id: 1,
        preempted_by: Some(10),
        preemptor_run_attempt: Some(1),
        preempt_qos: None,
        run_attempt: Some(1),
        at: None,
        preemption_claim: Some(cm.preemption_claim(&cm.get_job(1).expect("victim"), 1)),
    });
    cancel(&cm, "User");
    cm.jobs.write().get_mut(&10).expect("requester").end_time = Some(chrono::DateTime::UNIX_EPOCH);
    cm.evict_expired_terminal_jobs();
    assert!(cm.get_job(10).is_some());
    assert!(cm.preemption_usage.read().tracks_request(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preemption_confirmation_timeout_keeps_the_observed_outcome_for_retry() {
    let dir = tempfile::tempdir().expect("state");
    let (cm, gate) = crate::cluster::gated::cluster(
        &dir,
        serde_json::from_str(r#"{"cluster_name":"test"}"#).expect("config"),
    )
    .await;
    for id in [1, 10] {
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: id,
            spec: Box::default(),
        });
    }
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    let closed = gate.close();
    let error = cm
        .confirm_preemption_observation(10, 1, Some(1), true)
        .expect_err("blocked confirmation");
    assert!(crate::cluster::gated::late(&error).is_some());
    assert_eq!(cm.preemption_confirmations.read().len(), 1);
    drop(closed);
    cm.recover_preemption_confirmations(true)
        .expect("retry confirmed evidence before uncertainty");
    assert_usage(&cm, 1, 0);
    assert_eq!(cm.preemption_usage().open, 0);
    assert!(cm.preemption_confirmations.read().is_empty());
    cm.recover_preemption_confirmations(true)
        .expect("idempotent recovery");
    assert_usage(&cm, 1, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preemption_recovery_after_restart_reports_unknown_without_proof_of_release() {
    for terminal in [false, true] {
        let (_dir, cm) = cluster();
        start(&cm, 1, 1, &[("n1", scalar())]);
        preempt(&cm, 1, 1, 1);
        start(&cm, 10, 1, &[("n1", scalar())]);
        if terminal {
            cancel(&cm, "User");
        }
        let (dir, restored) = restored(&cm);
        let restored = std::sync::Arc::new(restored);
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), restored.clone())
            .await
            .expect("raft");
        handle
            .raft
            .wait(None)
            .metrics(|m| m.current_leader == Some(1), "leader")
            .await
            .expect("elected");
        restored.set_raft(handle.raft);
        restored
            .recover_preemption_confirmations(false)
            .expect("dispatch grace");
        assert_eq!(restored.preemption_usage().open, u64::from(!terminal));
        restored
            .recover_preemption_confirmations(true)
            .expect("uncertain settlement");
        let usage = restored.preemption_usage();
        assert_eq!((usage.used, usage.open, usage.unused_total()), (0, 0, 1));
        assert_eq!(
            usage.unused_count(Reason::EndedOther, Initiator::Unknown),
            1
        );
        assert_eq!(
            usage.unused_count(Reason::LaunchFailed, Initiator::System),
            0
        );
        assert!(!restored.preemption_usage.read().tracks_request(10));
        restored
            .recover_preemption_confirmations(true)
            .expect("idempotent");
        assert_eq!(restored.preemption_usage(), usage);
    }
}

#[test]
fn preemption_eviction_discards_cached_confirmations_before_job_id_reuse() {
    let (_dir, cm) = cluster();
    start(&cm, 1, 1, &[("n1", scalar())]);
    preempt(&cm, 1, 1, 1);
    start(&cm, 10, 1, &[("n1", scalar())]);
    confirm(&cm, 1, true);
    cm.preemption_confirmations.write().insert(
        (10, 1),
        PreemptionRelease {
            job_id: 10,
            run_attempt: 1,
            observation_id: Some(1),
            released: true,
            unconfirmed: false,
        },
    );
    cancel(&cm, "User");
    cm.apply_operation(&WalOperation::EvictTerminalJobs {
        job_ids: vec![10],
        before: None,
        at: None,
        preemption_release: None,
    });
    assert!(cm.get_job(10).is_none());
    assert!(cm.preemption_confirmations.read().is_empty());
}

#[test]
fn aggregate_scalar_preemption_slices_preserve_the_full_budget_in_node_order() {
    let nodes = vec!["n3".into(), "n1".into(), "n2".into()];
    let total = ResourceAllocations::with_scalar(10, 11);
    let slices = allocation_slices(&nodes, Some(&total), &HashMap::new());
    assert_eq!(slices.values().map(|a| a.cpus).sum::<u32>(), total.cpus);
    assert_eq!(
        slices.values().map(|a| a.memory_mb).sum::<u64>(),
        total.memory_mb
    );
    assert_eq!((slices["n3"].cpus, slices["n3"].memory_mb), (4, 4));
    assert_eq!((slices["n1"].cpus, slices["n1"].memory_mb), (3, 4));
    assert_eq!((slices["n2"].cpus, slices["n2"].memory_mb), (3, 3));
    assert!(allocation_slices(&[], Some(&total), &HashMap::new()).is_empty());
}

#[test]
fn aggregate_scalar_preemption_remainder_can_supply_the_requesting_launch() {
    for (cpus, memory_mb) in [(1, 0), (0, 1)] {
        let (_dir, cm) = cluster();
        let legacy_start: WalOperation = serde_json::from_value(serde_json::json!({
            "JobStart": {
                "job_id": 1,
                "nodes": ["n1", "n2", "n3"],
                "resources": {"cpus": cpus, "memory_mb": memory_mb, "devices": {}},
                "run_attempt": 1
            }
        }))
        .expect("aggregate-only reservation");
        assert!(replay(&cm, &legacy_start).job_started);
        assert!(replay(&cm, &WalOperation::job_state_change_activating(1, 1)).state_change_applied);
        assert!(cm
            .get_job(1)
            .expect("legacy victim")
            .per_node_alloc
            .is_empty());
        let (_follower_dir, follower) = restored(&cm);
        let preemption = canonical_preemption(&cm, 51);
        for replica in [&cm, &follower] {
            assert!(replay(replica, &preemption).preempt_applied);
            assert_eq!(replica.preemption_usage().open, 1);
            let reservation =
                requester_reservation(51, ResourceAllocations::with_scalar(cpus, memory_mb));
            assert!(replay(replica, &reservation).job_started);
            replay(replica, &WalOperation::job_state_change_activating(10, 1));
            replay(replica, &release_confirmation(51));
            assert_usage(replica, 1, 0);
        }
        assert_eq!(ledger_snapshot(&cm), ledger_snapshot(&follower));
    }
}

#[test]
fn signaled_preemption_without_completion_unpins_its_terminal_owner_on_node_loss() {
    for removed in [false, true] {
        let (_dir, cm) = cluster();
        register_gpu_node(&cm);
        start(&cm, 1, 1, &[("n1", scalar())]);
        let at = chrono::DateTime::UNIX_EPOCH;
        let pending = PreemptPending {
            preemption_claim: Some(cm.preemption_claim(&cm.get_job(1).expect("victim"), 1)),
            preemptor_run_attempt: Some(1),
            signaled_at: at,
            deadline: at + chrono::Duration::seconds(30),
            preempted_by: 10,
            preempt_qos: None,
            mode: PreemptMode::Requeue,
            run_attempt: 1,
            requeue_hold_secs: 0,
        };
        assert!(
            replay(
                &cm,
                &WalOperation::JobPreemptSignaled { job_id: 1, pending }
            )
            .preempt_signal_applied
        );
        start(&cm, 10, 1, &[("n2", scalar())]);
        confirm(&cm, 1, true);
        let mut complete = WalOperation::job_complete(10, 0, JobState::Completed);
        complete.stamp_occurred_at(at);
        replay(&cm, &complete);
        assert_eq!(cm.preemption_usage(), PreemptionUsage::default());
        assert!(cm.preemption_usage.read().tracks_request(10));
        assert_eq!(cm.get_job(1).expect("victim").state, JobState::Running);
        assert!(cm.get_job(1).expect("victim").node_completions.is_empty());
        let (_restored_dir, restored) = restored(&cm);
        let loss = if removed {
            WalOperation::NodeRemove {
                name: "n1".into(),
                reason: None,
                at: Some(at),
            }
        } else {
            WalOperation::NodeStateChange {
                name: "n1".into(),
                old_state: spur_core::node::NodeState::Mixed,
                new_state: spur_core::node::NodeState::Down,
                reason: None,
                admin_locked: false,
                reason_uid: None,
                reason_time: None,
                at: Some(at),
            }
        };
        for replica in [&cm, &restored] {
            let response = replay(replica, &loss);
            assert_eq!(response.jobs_finalized.len(), 1);
            let victim = replica.get_job(1).expect("victim");
            assert_eq!(victim.state, JobState::NodeFail);
            assert!(victim.preempt_pending.is_none());
            assert!(victim.node_completions.is_empty());
            assert_usage(replica, 0, 1);
            assert!(!replica.preemption_usage.read().tracks_request(10));
            replay(replica, &loss);
            assert_usage(replica, 0, 1);
            replay(
                replica,
                &WalOperation::EvictTerminalJobs {
                    job_ids: vec![10],
                    before: Some(at + chrono::Duration::seconds(1)),
                    at: Some(at),
                    preemption_release: None,
                },
            );
            assert!(replica.get_job(10).is_none());
        }
        assert_eq!(ledger_snapshot(&cm), ledger_snapshot(&restored));
    }
}
