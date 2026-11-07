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
