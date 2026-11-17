// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller behavior of the `admission.node_names` list.

use super::*;

fn register_nodes(cluster: &ClusterManager, names: &[&str]) {
    for name in names {
        if cluster.get_node(name).is_some() {
            continue;
        }
        cluster.apply_operation(&WalOperation::NodeRegister {
            name: (*name).into(),
            hostname: (*name).into(),
            resources: spur_core::resource::ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
            address: "127.0.0.1".into(),
            port: 1,
            wg_pubkey: String::new(),
            version: String::new(),
            labels: Default::default(),
            source: Default::default(),
            capabilities: Vec::new(),
            agent_instance: String::new(),
        });
    }
}

fn inventory(names: &[&str]) -> NodeInventory {
    NodeInventory {
        names: Some(names.iter().map(|name| (*name).into()).collect()),
    }
}

fn inventory_update(names: &[&str]) -> WalOperation {
    WalOperation::NodeInventoryUpdate {
        inventory: inventory(names),
    }
}

fn inventory_registration(name: &str) -> WalOperation {
    WalOperation::NodeRegister {
        name: name.into(),
        hostname: name.into(),
        resources: ResourceSet {
            cpus: 4,
            ..Default::default()
        },
        address: String::new(),
        port: 6818,
        wg_pubkey: String::new(),
        version: String::new(),
        labels: HashMap::new(),
        source: NodeSource::NativeHost,
        capabilities: Vec::new(),
        agent_instance: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_deletes_and_persists_and_can_reenable() {
    let dir = TempDir::new().unwrap();
    let base = "cluster_name = 'test'\n[admission]\nnode_names = ['n[1-2]']\n";
    let (cm, path) = test_cluster_with_conf_file(&dir, base).await;
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    try_register_node(&cm, "n2", 4, 8000).unwrap();
    assert_eq!(*cm.node_inventory.read(), None);
    cm.reconfigure().unwrap();
    assert_eq!(*cm.node_inventory.read(), Some(inventory(&["n1", "n2"])));
    std::fs::write(
        &path,
        "cluster_name = 'test'\n[admission]\nnode_names = ['n1']\n",
    )
    .unwrap();
    cm.reconfigure().unwrap();
    assert!(cm.get_node("n2").is_none());
    assert!(matches!(
        try_register_node(&cm, "n2", 4, 8000),
        Err(RegisterNodeError::PermissionDenied(_))
    ));
    let restored_dir = TempDir::new().unwrap();
    let restored = ClusterManager::new(test_config(), restored_dir.path()).unwrap();
    restored
        .restore_from_snapshot(&cm.snapshot_state().unwrap())
        .unwrap();
    assert!(restored.check_node_inventory("n2").is_err());
    assert!(restored.get_node("n2").is_none());
    assert_eq!(restored.get_nodes().len(), 1);
    std::fs::write(&path, base).unwrap();
    cm.reconfigure().unwrap();
    assert!(
        cm.get_node("n2").is_none(),
        "a listed name must not invent a live agent"
    );
    try_register_node(&cm, "n2", 4, 8000).unwrap();
    std::fs::write(
        &path,
        "cluster_name = 'test'\n[admission]\nnode_names = []\n",
    )
    .unwrap();
    cm.reconfigure().unwrap();
    assert!(cm.get_nodes().is_empty());
    std::fs::write(&path, "cluster_name = 'test'\n").unwrap();
    cm.reconfigure().unwrap();
    try_register_node(&cm, "dynamic", 4, 8000).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_can_clear_an_unsaved_startup_list() {
    for names_toml in ["[]", "['n1']"] {
        let dir = TempDir::new().unwrap();
        let (cm, path) = test_cluster_with_conf_file(
            &dir,
            &format!("cluster_name = 'test'\n[admission]\nnode_names = {names_toml}\n"),
        )
        .await;
        let startup_config = cm.config().as_ref().clone();
        assert_eq!(*cm.node_inventory.read(), None);
        assert!(cm.check_node_inventory("dynamic").is_err());
        std::fs::write(&path, "cluster_name = 'test'\n").unwrap();
        cm.reconfigure().unwrap();
        try_register_node(&cm, "dynamic", 4, 8000).unwrap();
        assert_eq!(*cm.node_inventory.read(), Some(NodeInventory::default()));
        let restored = ClusterManager::new(startup_config, dir.path()).unwrap();
        restored
            .restore_from_snapshot(&cm.snapshot_state().unwrap())
            .unwrap();
        assert!(restored.check_node_inventory("another").is_ok());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_inventory_rejects_stale_k0s_membership_before_success_or_metrics() {
    use spur_core::k0s::K0sPhase;
    for selection in 0..4 {
        let dir = TempDir::new().unwrap();
        let cm = Arc::new(ClusterManager::new(test_config(), dir.path()).unwrap());
        cm.apply_operation(&inventory_registration("n1"));
        cm.apply_operation(&inventory_registration("n2"));
        cm.apply_operation(&inventory_update(&["n1", "n2"]));
        if selection == 3 {
            cm.apply_operation(&WalOperation::K0sSetPhase {
                phase: K0sPhase::Provisioning,
                control_plane_node: Some("n1".into()),
                control_plane_nodes: vec!["n1".into()],
                member_nodes: vec!["n1".into()],
                reset_requested: false,
            });
        }
        let before = cm.k0s_state();
        let metrics = || {
            let body =
                spur_metrics::encode_k8s_metrics(&cm.k8s_cluster_metrics(), &cm.k8s_metrics());
            let mut lines: Vec<_> = body.lines().map(str::to_owned).collect();
            lines.sort_unstable();
            lines
        };
        let metrics_before = metrics();
        let recorder = Arc::new(RecordingApplier {
            inner: cm.clone(),
            ops: parking_lot::Mutex::new(Vec::new()),
            before_apply: |cluster, op| {
                if matches!(
                    op,
                    WalOperation::K0sSetPhase { .. } | WalOperation::K0sMemberNodesAdd { .. }
                ) {
                    assert!(cluster
                        .apply_operation(&inventory_update(&["n1"]))
                        .node_inventory_error
                        .is_none());
                }
            },
        });
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), recorder)
            .await
            .unwrap();
        handle
            .raft
            .wait(None)
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .unwrap();
        cm.set_raft(handle.raft);
        let result = if selection == 3 {
            cm.add_k0s_member_nodes(vec!["n2".into()])
        } else {
            cm.set_k0s_phase(
                K0sPhase::Provisioning,
                Some(if selection == 0 { "n2" } else { "n1" }.into()),
                if selection == 1 {
                    vec!["n2".into()]
                } else {
                    Vec::new()
                },
                if selection == 2 {
                    vec!["n2".into()]
                } else {
                    Vec::new()
                },
                false,
            )
        };
        assert!(result.unwrap_err().to_string().contains("n2"));
        assert!(cm.get_node("n2").is_none());
        assert_eq!(
            serde_json::to_value(cm.k0s_state()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert_eq!(metrics(), metrics_before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_without_node_inventory_preserves_dynamic_nodes() {
    let dir = TempDir::new().unwrap();
    let (cm, path) = test_cluster_with_conf_file(
        &dir,
        "cluster_name = 'test'\n[[nodes]]\nnames = 'n1'\ncpus = 2\n",
    )
    .await;
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    try_register_node(&cm, "dynamic", 4, 8000).unwrap();
    std::fs::write(&path, "cluster_name = 'test'\n").unwrap();
    cm.reconfigure().unwrap();
    assert_eq!(cm.get_nodes().len(), 2);
    assert_eq!(*cm.node_inventory.read(), None);
    assert_eq!(cm.get_node("n1").unwrap().total_resources.cpus, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_saves_node_names_only_when_the_list_changes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spur.conf");
    std::fs::write(
        &path,
        "cluster_name = 'test'\n[admission]\nnode_names = ['n1', 'n2']\n",
    )
    .unwrap();
    let config = SlurmConfig::load_from_file(&path).unwrap();
    let cm = Arc::new(
        ClusterManager::new_with_config_path(config, dir.path(), Some(path.clone())).unwrap(),
    );
    let recorder = Arc::new(RecordingApplier {
        inner: cm.clone(),
        ops: parking_lot::Mutex::new(Vec::new()),
        before_apply: |_, _| {},
    });
    let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), recorder.clone())
        .await
        .unwrap();
    handle
        .raft
        .wait(None)
        .metrics(|m| m.current_leader == Some(1), "leader elected")
        .await
        .unwrap();
    cm.set_raft(handle.raft);
    let saves = || {
        recorder
            .ops
            .lock()
            .iter()
            .filter(|op| matches!(op, WalOperation::NodeInventoryUpdate { .. }))
            .count()
    };

    cm.reconfigure().unwrap();
    cm.reconfigure().unwrap();
    assert_eq!(saves(), 1, "an unchanged list is not written again");
    assert_eq!(*cm.node_inventory.read(), Some(inventory(&["n1", "n2"])));

    // The startup list still names n1 and n2, but only the saved list is compared.
    std::fs::write(&path, "cluster_name = 'test'\n").unwrap();
    cm.reconfigure().unwrap();
    cm.reconfigure().unwrap();
    assert_eq!(saves(), 2, "a cleared list equals an unset one");
    assert_eq!(*cm.node_inventory.read(), Some(NodeInventory::default()));
    try_register_node(&cm, "n3", 4, 8000).unwrap();
}

#[test]
fn node_inventory_clear_saves_a_cleared_list() {
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    let clear =
        serde_json::from_str(r#"{"NodeInventoryUpdate":{"inventory":{"names":null}}}"#).unwrap();
    assert!(cm.apply_operation(&clear).node_inventory_error.is_none());
    assert_eq!(*cm.node_inventory.read(), Some(NodeInventory::default()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconfigure_node_inventory_waits_for_a_timed_out_earlier_write() {
    for names_toml in ["", "[admission]\nnode_names = ['n1', 'n2']\n"] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spur.conf");
        std::fs::write(&path, format!("cluster_name = 'test'\n{names_toml}")).unwrap();
        let config = SlurmConfig::load_from_file(&path).unwrap();
        let (cm, gate) = gated::cluster_with_config_path(&dir, config, Some(path)).await;
        try_register_node(&cm, "n1", 4, 8000).unwrap();
        try_register_node(&cm, "n2", 4, 8000).unwrap();
        cm.reconfigure().unwrap();
        let saved = cm.node_inventory.read().clone();
        let closed = gate.close();
        let error = cm.propose(inventory_update(&["n1"])).unwrap_err();
        assert!(error.is::<OutcomeUnknown>());

        let error = cm.reconfigure().unwrap_err();
        assert!(error.to_string().contains("prior Raft writes timed out"));
        assert_eq!(*cm.node_inventory.read(), saved);
        drop(closed);

        // The late write deletes n2; the retry must still restore the file's list.
        cm.reconfigure().unwrap();
        assert!(cm.get_node("n2").is_none());
        let desired = saved.unwrap_or_default();
        assert_eq!(*cm.node_inventory.read(), Some(desired));
        cm.check_node_inventory("n2").unwrap();
        try_register_node(&cm, "n2", 4, 8000).unwrap();
        assert!(cm.get_node("n2").is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_refuses_held_pending_placement_without_swapping_config() {
    let dir = TempDir::new().unwrap();
    let (cm, path) = test_cluster_with_conf_file(
        &dir,
        "cluster_name = 'test'\n[admission]\nnode_names = ['n1']\n",
    )
    .await;
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    cm.reconfigure().unwrap();
    let mut job = Job::new(42, JobSpec::default());
    job.allocated_nodes = vec!["n1".into()];
    cm.jobs.write().insert(42, job);
    std::fs::write(
        &path,
        "cluster_name = 'test'\n[admission]\nnode_names = []\n",
    )
    .unwrap();
    assert!(cm
        .reconfigure()
        .unwrap_err()
        .to_string()
        .contains("job allocations"));
    assert!(cm.get_node("n1").is_some());
    assert_eq!(cm.config().admission.node_names, Some(vec!["n1".into()]));
    assert_eq!(*cm.node_inventory.read(), Some(inventory(&["n1"])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_rechecks_allocation_after_preflight() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spur.conf");
    let partition = "[[partitions]]\nname = 'default'\nnodes = 'n1'\ndefault = true\n";
    std::fs::write(
        &path,
        format!("cluster_name = 'test'\n[admission]\nnode_names = ['n1']\n{partition}max_time = '01:00:00'\n"),
    )
    .unwrap();
    let config = SlurmConfig::load_from_file(&path).unwrap();
    let cm = Arc::new(
        ClusterManager::new_with_config_path(config, dir.path(), Some(path.clone())).unwrap(),
    );
    let recorder = Arc::new(RecordingApplier {
        inner: cm.clone(),
        ops: parking_lot::Mutex::new(Vec::new()),
        before_apply: |cluster, op| {
            if matches!(op, WalOperation::NodeInventoryUpdate { .. }) {
                let response = cluster.apply_operation(&WalOperation::job_start(
                    42,
                    vec!["n1".into()],
                    ResourceAllocations::with_scalar(1, 100),
                    HashMap::from([("n1".into(), ResourceAllocations::with_scalar(1, 100))]),
                ));
                assert!(response.job_started);
            }
        },
    });
    let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), recorder)
        .await
        .unwrap();
    handle
        .raft
        .wait(None)
        .metrics(|m| m.current_leader == Some(1), "leader elected")
        .await
        .unwrap();
    cm.set_raft(handle.raft);
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    cm.apply_operation(&WalOperation::JobSubmit {
        job_id: 42,
        spec: Box::new(JobSpec::default()),
    });
    std::fs::write(
        &path,
        format!("cluster_name = 'test'\n[admission]\nnode_names = []\n{partition}max_time = '02:00:00'\n"),
    )
    .unwrap();
    assert!(cm
        .reconfigure()
        .unwrap_err()
        .to_string()
        .contains("job allocations"));
    assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 1);
    assert_eq!(cm.get_job(42).unwrap().allocated_nodes, vec!["n1"]);
    assert_eq!(*cm.node_inventory.read(), None);
    assert_eq!(cm.config().admission.node_names, Some(vec!["n1".into()]));
    assert_eq!(
        cm.get_partitions()[0].max_time_minutes,
        Some(60),
        "a refused node list must not leave partition changes behind"
    );
}

#[test]
fn node_inventory_apply_refuses_busy_reserved_and_k0s_nodes_atomically() {
    for state in [
        JobState::Pending,
        JobState::Running,
        JobState::Completing,
        JobState::Suspended,
    ] {
        let dir = TempDir::new().unwrap();
        let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
        cm.apply_operation(&inventory_registration("n1"));
        cm.apply_operation(&inventory_registration("n2"));
        let mut job = Job::new(42, JobSpec::default());
        job.state = state;
        job.allocated_nodes = vec!["n2".into()];
        cm.jobs.write().insert(42, job);
        let before: serde_json::Value =
            serde_json::from_slice(&cm.snapshot_state().unwrap()).unwrap();
        assert!(cm
            .apply_operation(&inventory_update(&[]))
            .node_inventory_error
            .unwrap()
            .contains("job allocations"));
        assert_eq!(cm.get_nodes().len(), 2);
        assert_eq!(*cm.node_inventory.read(), None);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&cm.snapshot_state().unwrap()).unwrap(),
            before,
            "rejection must preserve the entire allocation in {state:?}"
        );
    }
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    cm.apply_operation(&inventory_registration("n1"));
    let reservation: Reservation = serde_json::from_str(r#"{"name":"maintenance","start_time":"2099-01-01T00:00:00Z","end_time":"2099-01-02T00:00:00Z","nodes":["n1"],"accounts":[],"users":[]}"#).unwrap();
    cm.apply_operation(&WalOperation::ReservationCreate { reservation });
    assert!(cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap()
        .contains("reservation"));
    cm.reservations.write().clear();
    cm.nodes.write().get_mut("n1").unwrap().k0s_role = Some(spur_core::k0s::K0sRole::Worker);
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(
        error.contains("retry once the teardown finishes"),
        "{error}"
    );
    assert!(cm.get_node("n1").is_some());
    cm.nodes.write().get_mut("n1").unwrap().k0s_role = None;
    cm.k0s.write().phase = spur_core::k0s::K0sPhase::Provisioning;
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(
        error.contains("spans every node; run `spur k8s down`"),
        "{error}"
    );
    cm.nodes.write().get_mut("n1").unwrap().k0s_role = Some(spur_core::k0s::K0sRole::Worker);
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(
        error.contains("spans every node; run `spur k8s down`"),
        "{error}"
    );
    cm.nodes.write().get_mut("n1").unwrap().k0s_role = None;
    cm.k0s.write().member_nodes = vec!["n1".into()];
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(error.contains("spur k8s remove-nodes"), "{error}");
    cm.k0s.write().phase = spur_core::k0s::K0sPhase::Degraded;
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(
        error.contains("degraded Kubernetes cluster; run `spur k8s down`"),
        "{error}"
    );
    cm.k0s.write().phase = spur_core::k0s::K0sPhase::Provisioning;
    cm.nodes.write().get_mut("n1").unwrap().k0s_role = Some(spur_core::k0s::K0sRole::Controller);
    let error = cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .unwrap();
    assert!(
        error.contains("control-plane node; run `spur k8s down`"),
        "{error}"
    );
    assert!(cm.get_node("n1").is_some());
}

#[test]
fn committed_node_inventory_fences_late_registration_placement_and_reservation() {
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    cm.apply_operation(&inventory_registration("n1"));
    cm.apply_operation(&inventory_update(&[]));
    assert!(
        cm.apply_operation(&inventory_registration("n1"))
            .node_registration_rejected
    );
    let update = WalOperation::NodeUpdate {
        name: "n1".into(),
        hostname: "n1".into(),
        resources: ResourceSet {
            cpus: 4,
            ..Default::default()
        },
        address: "node.invalid".into(),
        port: 6818,
        wg_pubkey: String::new(),
        version: String::new(),
        source: NodeSource::NativeHost,
        capabilities: Vec::new(),
        agent_instance: String::new(),
    };
    assert!(cm.apply_operation(&update).node_registration_rejected);
    cm.jobs.write().insert(42, Job::new(42, JobSpec::default()));
    let placement = WalOperation::job_start(
        42,
        vec!["n1".into()],
        ResourceAllocations::with_scalar(1, 100),
        HashMap::new(),
    );
    assert!(!cm.apply_operation(&placement).job_started);
    assert!(cm.get_job(42).unwrap().allocated_nodes.is_empty());
    let mut reservation: Reservation = serde_json::from_str(r#"{"name":"maintenance","start_time":"2099-01-01T00:00:00Z","end_time":"2099-01-02T00:00:00Z","nodes":["n1"],"accounts":[],"users":[]}"#).unwrap();
    assert!(cm
        .apply_operation(&WalOperation::ReservationCreate {
            reservation: reservation.clone()
        })
        .node_inventory_error
        .is_some());
    assert!(cm.get_reservations().is_empty());
    reservation.nodes.clear();
    cm.apply_operation(&WalOperation::ReservationCreate { reservation });
    assert!(cm
        .apply_operation(&WalOperation::ReservationUpdate {
            name: "maintenance".into(),
            duration_minutes: 0,
            add_nodes: vec!["n1".into()],
            remove_nodes: Vec::new(),
            add_users: Vec::new(),
            remove_users: Vec::new(),
            add_accounts: Vec::new(),
            remove_accounts: Vec::new(),
        })
        .node_inventory_error
        .is_some());
    assert!(cm.get_reservations()[0].nodes.is_empty());
    assert!(cm.get_nodes().is_empty());
    cm.apply_operation(&WalOperation::NodeInventoryUpdate {
        inventory: NodeInventory::default(),
    });
    assert!(cm.check_node_inventory("n1").is_ok());
    let restored = ClusterManager::new(test_config(), dir.path()).unwrap();
    restored
        .restore_from_snapshot(&cm.snapshot_state().unwrap())
        .unwrap();
    assert_eq!(
        *restored.node_inventory.read(),
        Some(NodeInventory::default())
    );
    // A cleared list fences nothing, the same as a list that was never set.
    assert!(restored.apply_operation(&placement).job_started);
    let reserved: Reservation = serde_json::from_str(r#"{"name":"cleared","start_time":"2099-01-01T00:00:00Z","end_time":"2099-01-02T00:00:00Z","nodes":["n1"],"accounts":[],"users":[]}"#).unwrap();
    assert!(cm
        .apply_operation(&WalOperation::ReservationCreate {
            reservation: reserved
        })
        .node_inventory_error
        .is_none());
    assert!(cm
        .k0s_inventory_error(&cm.nodes.read(), &["n1".to_string()])
        .is_none());
}

fn inventory_resource_cases() -> [ResourceAllocations; 3] {
    [
        ResourceAllocations::with_scalar(1, 0),
        ResourceAllocations::with_scalar(0, 100),
        ResourceAllocations {
            devices: HashMap::from([(
                "gpu".into(),
                vec![spur_core::resource::AllocatedDevice::injectable(1)],
            )]),
            ..Default::default()
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_startup_list_never_replaces_the_saved_one() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.admission.node_names = Some(vec!["n1".into()]);
    let cm = test_cluster_with_config(&dir, config).await;
    cm.apply_operation(&inventory_update(&["n1", "n2"]));
    try_register_node(&cm, "n2", 4, 8000).unwrap();
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    assert_eq!(*cm.node_inventory.read(), Some(inventory(&["n1", "n2"])));
    assert!(cm
        .node_names_mismatch_warned
        .load(std::sync::atomic::Ordering::Relaxed));
    assert!(!cm
        .node_names_unsaved_warned
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placement_onto_an_unregistered_node_is_refused_with_a_saved_list() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    register_node(&cm, "n1", 8, 16000);
    cm.apply_operation(&inventory_update(&["n1", "n2"]));
    let id = submit_and_wait(&cm, basic_spec("a"));
    let res = scalar_alloc(4, 8000);
    assert!(cm
        .reserve_placement(
            id,
            vec!["n2".into()],
            res.clone(),
            per_node_for(&["n2"], res.clone()),
            false,
            false,
        )
        .is_err());
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.state, JobState::Pending);
    assert!(job.allocated_nodes.is_empty());
    cm.reserve_placement(
        id,
        vec!["n1".into()],
        res.clone(),
        per_node_for(&["n1"], res),
        false,
        false,
    )
    .unwrap();
}

#[test]
fn controller_refuses_to_start_with_an_invalid_node_list() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.admission.node_names = Some(vec!["node*".into()]);
    assert!(ClusterManager::new(config, dir.path()).is_err());
}

#[test]
fn removed_unlisted_node_cannot_reregister_before_the_list_is_saved() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.admission.node_names = Some(vec!["n1".into()]);
    let cm = Arc::new(ClusterManager::new(config, dir.path()).unwrap());
    cm.apply_operation(&inventory_registration("old"));
    let node_lock = cm.node_registration_lock("old");
    let held = node_lock.lock();
    let registering = {
        let cm = cm.clone();
        std::thread::spawn(move || try_register_node(&cm, "old", 4, 8000))
    };
    // The registration takes its own handle to the per-node lock once its pre-check passed.
    while Arc::strong_count(&node_lock) < 3 {
        assert!(
            !registering.is_finished(),
            "registration ended before the per-node lock"
        );
        std::thread::yield_now();
    }
    cm.apply_operation(&WalOperation::NodeRemove {
        name: "old".into(),
        reason: None,
        at: None,
        agent_instance: String::new(),
        comm_url: None,
    });
    drop(held);
    assert!(matches!(
        registering.join().unwrap(),
        Err(RegisterNodeError::PermissionDenied(_))
    ));
    assert!(cm.get_node("old").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_never_saves_the_node_list() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.admission.node_names = Some(vec!["n1".into()]);
    let cm = Arc::new(ClusterManager::new(config, dir.path()).unwrap());
    let recorder = Arc::new(RecordingApplier {
        inner: cm.clone(),
        ops: parking_lot::Mutex::new(Vec::new()),
        before_apply: |_, _| {},
    });
    let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), recorder.clone())
        .await
        .unwrap();
    handle
        .raft
        .wait(None)
        .metrics(|m| m.current_leader == Some(1), "leader elected")
        .await
        .unwrap();
    cm.set_raft(handle.raft);
    cm.apply_operation(&inventory_registration("old"));
    cm.nodes.write().get_mut("old").unwrap().alloc_resources =
        ResourceAllocations::with_scalar(1, 0);

    try_register_node(&cm, "n1", 4, 8000).unwrap();
    try_register_node(&cm, "old", 4, 8000).unwrap();
    assert!(matches!(
        try_register_node(&cm, "new", 4, 8000),
        Err(RegisterNodeError::PermissionDenied(_))
    ));
    let saves = || {
        recorder
            .ops
            .lock()
            .iter()
            .filter(|op| matches!(op, WalOperation::NodeInventoryUpdate { .. }))
            .count()
    };
    assert_eq!(saves(), 0, "only scontrol reconfigure saves the list");
    assert_eq!(*cm.node_inventory.read(), None);
    assert!(cm.get_node("old").is_some());

    cm.nodes.write().get_mut("old").unwrap().alloc_resources = Default::default();
    cm.remove_node("old", false, None).unwrap();
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    assert_eq!(
        saves(),
        0,
        "a list that deletes nothing is not saved either"
    );
    assert_eq!(*cm.node_inventory.read(), None);
    assert!(cm
        .node_names_unsaved_warned
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_refuses_local_resources_then_retries() {
    for resources in inventory_resource_cases() {
        let dir = TempDir::new().unwrap();
        let (cm, path) = test_cluster_with_conf_file(
            &dir,
            "cluster_name = 'test'\n[admission]\nnode_names = ['n1', 'n2']\n",
        )
        .await;
        try_register_node(&cm, "n1", 4, 8000).unwrap();
        try_register_node(&cm, "n2", 4, 8000).unwrap();
        cm.nodes.write().get_mut("n2").unwrap().alloc_resources = resources.clone();
        std::fs::write(
            &path,
            "cluster_name = 'test'\n[admission]\nnode_names = []\n",
        )
        .unwrap();
        let before: serde_json::Value =
            serde_json::from_slice(&cm.snapshot_state().unwrap()).unwrap();
        assert!(cm
            .reconfigure()
            .unwrap_err()
            .to_string()
            .contains("allocated resources"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&cm.snapshot_state().unwrap()).unwrap(),
            before
        );
        assert_eq!(
            cm.config().admission.node_names,
            Some(vec!["n1".into(), "n2".into()])
        );

        cm.nodes.write().get_mut("n2").unwrap().alloc_resources = Default::default();
        cm.reconfigure().unwrap();
        assert!(cm.get_nodes().is_empty());
        assert_eq!(*cm.node_inventory.read(), Some(inventory(&[])));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_preserves_reservation_then_retries() {
    let dir = TempDir::new().unwrap();
    let (cm, path) = test_cluster_with_conf_file(
        &dir,
        "cluster_name = 'test'\n[admission]\nnode_names = ['n1', 'n2']\n",
    )
    .await;
    try_register_node(&cm, "n1", 4, 8000).unwrap();
    try_register_node(&cm, "n2", 4, 8000).unwrap();
    let reservation: Reservation = serde_json::from_str(r#"{"name":"maintenance","start_time":"2099-01-01T00:00:00Z","end_time":"2099-01-02T00:00:00Z","nodes":["n2"],"accounts":[],"users":[]}"#).unwrap();
    cm.create_reservation(reservation).unwrap();
    std::fs::write(
        &path,
        "cluster_name = 'test'\n[admission]\nnode_names = []\n",
    )
    .unwrap();
    let before: serde_json::Value = serde_json::from_slice(&cm.snapshot_state().unwrap()).unwrap();
    assert!(cm
        .reconfigure()
        .unwrap_err()
        .to_string()
        .contains("reservation"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&cm.snapshot_state().unwrap()).unwrap(),
        before
    );
    assert_eq!(
        cm.config().admission.node_names,
        Some(vec!["n1".into(), "n2".into()])
    );

    cm.delete_reservation("maintenance").unwrap();
    cm.reconfigure().unwrap();
    assert!(cm.get_nodes().is_empty());
    assert!(cm.get_reservations().is_empty());
    assert_eq!(*cm.node_inventory.read(), Some(inventory(&[])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_node_inventory_preserves_kubernetes_then_retries() {
    use spur_core::k0s::{K0sPhase, K0sRole};

    for assigned_role in [false, true] {
        let dir = TempDir::new().unwrap();
        let (cm, path) = test_cluster_with_conf_file(
            &dir,
            "cluster_name = 'test'\n[admission]\nnode_names = ['n1', 'n2']\n",
        )
        .await;
        try_register_node(&cm, "n1", 4, 8000).unwrap();
        try_register_node(&cm, "n2", 4, 8000).unwrap();
        if assigned_role {
            cm.assign_node_k0s("n2", K0sRole::Worker, "192.0.2.2", "192.0.2.0/24")
                .unwrap();
        } else {
            cm.set_k0s_phase(
                K0sPhase::Provisioning,
                Some("n2".into()),
                vec!["n2".into()],
                vec!["n2".into()],
                false,
            )
            .unwrap();
        }
        std::fs::write(
            &path,
            "cluster_name = 'test'\n[admission]\nnode_names = []\n",
        )
        .unwrap();
        let before: serde_json::Value =
            serde_json::from_slice(&cm.snapshot_state().unwrap()).unwrap();
        assert!(cm
            .reconfigure()
            .unwrap_err()
            .to_string()
            .contains("Kubernetes"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&cm.snapshot_state().unwrap()).unwrap(),
            before
        );
        assert_eq!(
            cm.config().admission.node_names,
            Some(vec!["n1".into(), "n2".into()])
        );

        if assigned_role {
            cm.clear_node_k0s("n2").unwrap();
        } else {
            cm.set_k0s_phase(K0sPhase::Down, None, Vec::new(), Vec::new(), false)
                .unwrap();
        }
        cm.reconfigure().unwrap();
        assert!(cm.get_nodes().is_empty());
        assert_eq!(*cm.node_inventory.read(), Some(inventory(&[])));
    }
}

#[test]
fn node_inventory_removes_nodes_with_finalized_legacy_preemption_history() {
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    cm.apply_operation(&inventory_registration("n1"));
    cm.apply_operation(&WalOperation::JobSubmit {
        job_id: 1,
        spec: Box::new(basic_spec("legacy")),
    });
    let slice = ResourceAllocations::with_scalar(1, 0);
    cm.apply_operation(&WalOperation::job_start(
        1,
        vec!["n1".into()],
        slice.clone(),
        HashMap::from([("n1".into(), slice)]),
    ));
    cm.apply_operation(&WalOperation::job_state_change(
        1,
        JobState::Pending,
        JobState::Running,
    ));
    let old: WalOperation =
        serde_json::from_str(r#"{"JobComplete":{"job_id":1,"exit_code":-1,"state":"PREEMPTED"}}"#)
            .unwrap();
    cm.apply_operation(&old);
    assert_eq!(cm.get_job(1).unwrap().state, JobState::Preempted);
    assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 0);
    assert!(cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .is_none());
    assert!(cm.get_node("n1").is_none());
    assert_eq!(cm.get_job(1).unwrap().allocated_nodes, vec!["n1"]);
}

#[test]
fn node_inventory_apply_ignores_replica_local_gpu_accounting() {
    let leader_dir = TempDir::new().unwrap();
    let follower_dir = TempDir::new().unwrap();
    let leader = ClusterManager::new(test_config(), leader_dir.path()).unwrap();
    let follower = ClusterManager::new(test_config(), follower_dir.path()).unwrap();
    let stable_ids = [0x63_0000, 0x83_0000];
    let mut ops: Vec<_> = gpu_id_change_ops(&stable_ids, &[0, 1], &stable_ids)
        .into_iter()
        .take(3)
        .collect();
    ops.push(WalOperation::job_state_change(
        1,
        JobState::Pending,
        JobState::Running,
    ));
    ops.push(WalOperation::JobNodeComplete {
        job_id: 1,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        run_attempt: 0,
        at: None,
    });
    for op in &ops {
        leader.apply_operation(op);
        follower.apply_operation(op);
    }
    assert_eq!(leader.get_job(1).unwrap().state, JobState::Completed);
    assert!(leader.validate_node_inventory(&inventory(&[])).is_ok());
    leader.reconcile_node_gpu_allocations("n1", &[(1, stable_ids.to_vec())]);
    for cluster in [&leader, &follower] {
        assert!(cluster
            .apply_operation(&inventory_update(&[]))
            .node_inventory_error
            .is_none());
        assert!(cluster.get_node("n1").is_none());
        assert_eq!(*cluster.node_inventory.read(), Some(inventory(&[])));
    }
}

#[test]
fn node_inventory_k0s_guards_preserve_legacy_replay_and_teardown() {
    use spur_core::k0s::K0sPhase;
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    let mut phase = WalOperation::K0sSetPhase {
        phase: K0sPhase::Provisioning,
        control_plane_node: Some("n1".into()),
        control_plane_nodes: vec!["n1".into()],
        member_nodes: vec!["n1".into()],
        reset_requested: false,
    };
    assert!(cm.apply_operation(&phase).node_inventory_error.is_none());
    cm.apply_operation(&inventory_update(&["n1"]));
    assert!(cm.apply_operation(&phase).node_inventory_error.is_some());
    assert!(cm
        .apply_operation(&WalOperation::K0sMemberNodesAdd {
            nodes: vec!["n1".into()]
        })
        .node_inventory_error
        .is_some());
    cm.apply_operation(&inventory_registration("n1"));
    assert!(cm.apply_operation(&phase).node_inventory_error.is_none());
    assert!(cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .is_some());
    if let WalOperation::K0sSetPhase { phase, .. } = &mut phase {
        *phase = K0sPhase::Down;
    }
    assert!(cm.apply_operation(&phase).node_inventory_error.is_none());
    assert!(cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .is_none());
    assert!(cm.apply_operation(&phase).node_inventory_error.is_none());
    assert_eq!(cm.k0s_state().phase, K0sPhase::Down);
    assert!(cm.k0s_state().member_nodes.is_empty());
}

#[test]
fn node_inventory_preserves_legacy_missing_node_placement_on_replay() {
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    register_nodes(&cm, &["n1", "n2"]);
    cm.apply_operation(&WalOperation::JobSubmit {
        job_id: 1,
        spec: Box::new(basic_spec("legacy-placement")),
    });
    let removal =
        serde_json::from_str(r#"{"NodeRemove":{"name":"n1","reason":null,"at":null}}"#).unwrap();
    let placement = serde_json::from_str(
        r#"{"JobStart":{"job_id":1,"nodes":["n1","n2"],"resources":{"cpus":2,"memory_mb":2000,"devices":{}},"per_node_alloc":{"n1":{"cpus":1,"memory_mb":1000,"devices":{}},"n2":{"cpus":1,"memory_mb":1000,"devices":{}}},"run_attempt":1}}"#,
    ).unwrap();
    cm.apply_operation(&removal);
    assert!(cm.apply_operation(&placement).job_started);
    assert!(cm.get_node("n1").is_none());
    assert_eq!(cm.get_node("n2").unwrap().alloc_resources.cpus, 1);
    assert_eq!(cm.get_job(1).unwrap().allocated_nodes, vec!["n1", "n2"]);
    cm.apply_operation(&WalOperation::job_state_change(
        1,
        JobState::Pending,
        JobState::Running,
    ));
    let completion =
        serde_json::from_str(r#"{"JobComplete":{"job_id":1,"exit_code":0,"state":"COMPLETED"}}"#)
            .unwrap();
    cm.apply_operation(&completion);
    assert_eq!(cm.get_job(1).unwrap().state, JobState::Completed);
    assert_eq!(cm.get_node("n2").unwrap().alloc_resources.cpus, 0);
    assert_eq!(*cm.node_inventory.read(), None);
}

#[test]
fn node_inventory_old_snapshot_and_completed_history_remain_compatible() {
    let old_response: ClientResponse = serde_json::from_str("{}").unwrap();
    assert!(old_response.node_inventory_error.is_none());
    assert!(!old_response.node_registration_rejected);
    let dir = TempDir::new().unwrap();
    let cm = ClusterManager::new(test_config(), dir.path()).unwrap();
    cm.restore_from_snapshot(
        br#"{"jobs":[],"nodes":[],"reservations":[],"steps":[],"license_pool":{}}"#,
    )
    .unwrap();
    assert_eq!(*cm.node_inventory.read(), None);
    cm.apply_operation(&inventory_registration("n1"));
    let mut job = Job::new(42, JobSpec::default());
    job.state = JobState::Completed;
    job.allocated_nodes = vec!["n1".into()];
    cm.jobs.write().insert(42, job);
    cm.next_job_id.store(43, Ordering::Relaxed);
    assert!(cm
        .apply_operation(&inventory_update(&[]))
        .node_inventory_error
        .is_none());
    let snapshot = cm.snapshot_state().unwrap();
    let restored = ClusterManager::new(test_config(), dir.path()).unwrap();
    restored.restore_from_snapshot(&snapshot).unwrap();
    assert_eq!(restored.get_job(42).unwrap().allocated_nodes, vec!["n1"]);
    assert_eq!(restored.next_job_id.load(Ordering::Relaxed), 43);
    assert_eq!(*restored.node_inventory.read(), Some(inventory(&[])));
    assert!(restored.get_nodes().is_empty());
}
