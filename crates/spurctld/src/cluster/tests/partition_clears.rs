// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A partition update applied the way a follower or a restarting controller applies
//! it: from the JSON in the log, not from the leader's in-memory entry.

use super::*;
use spur_core::partition::PartitionClears;

fn limited_partition() -> Partition {
    Partition {
        name: "gpu".into(),
        nodes: "ALL".into(),
        max_time_minutes: Some(60),
        default_time_minutes: Some(30),
        max_nodes: Some(8),
        preempt_exempt_time: Some(120),
        ..Default::default()
    }
}

/// The four legacy limits of the `gpu` partition.
fn limits_of(cm: &ClusterManager) -> [Option<u32>; 4] {
    let part = cm
        .get_partitions()
        .into_iter()
        .find(|p| p.name == "gpu")
        .unwrap();
    [
        part.max_time_minutes,
        part.default_time_minutes,
        part.max_nodes,
        part.preempt_exempt_time,
    ]
}

async fn cluster_with_limited_partition(dir: &TempDir) -> Arc<ClusterManager> {
    let cm = test_cluster(dir).await;
    cm.apply_operation(&WalOperation::PartitionCreate {
        partition: limited_partition(),
    });
    assert_eq!(limits_of(&cm), [Some(60), Some(30), Some(8), Some(120)]);
    cm
}

fn replicated(op: &WalOperation) -> WalOperation {
    let json = serde_json::to_string(op).unwrap();
    serde_json::from_str(&json).unwrap()
}

fn apply_replicated(cm: &ClusterManager, op: WalOperation) {
    cm.apply_operation(&replicated(&op));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_limit_clear_applies_after_a_json_round_trip() {
    let patches = [
        (
            PartitionPatch {
                max_time: Some("INFINITE".into()),
                ..Default::default()
            },
            [None, Some(30), Some(8), Some(120)],
        ),
        (
            PartitionPatch {
                default_time: Some("UNLIMITED".into()),
                ..Default::default()
            },
            [Some(60), None, Some(8), Some(120)],
        ),
        (
            PartitionPatch {
                clear_max_nodes: true,
                ..Default::default()
            },
            [Some(60), Some(30), None, Some(120)],
        ),
        (
            PartitionPatch {
                preempt_exempt_time: Some(None),
                ..Default::default()
            },
            [Some(60), Some(30), Some(8), None],
        ),
    ];
    for (patch, expected) in patches {
        let dir = TempDir::new().unwrap();
        let cm = cluster_with_limited_partition(&dir).await;

        apply_replicated(&cm, patch.clone().into_wal("gpu").unwrap());

        assert_eq!(limits_of(&cm), expected, "{patch:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_every_limit_at_once_applies_after_a_json_round_trip() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;
    let patch = PartitionPatch {
        max_time: Some("INFINITE".into()),
        default_time: Some("INFINITE".into()),
        clear_max_nodes: true,
        preempt_exempt_time: Some(None),
        ..Default::default()
    };

    apply_replicated(&cm, patch.into_wal("gpu").unwrap());

    assert_eq!(limits_of(&cm), [None; 4]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_sets_apply_after_a_json_round_trip() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;
    let patch = PartitionPatch {
        max_time: Some("90".into()),
        default_time: Some("45".into()),
        max_nodes: Some(16),
        preempt_exempt_time: Some(Some(300)),
        ..Default::default()
    };

    apply_replicated(&cm, patch.into_wal("gpu").unwrap());

    assert_eq!(limits_of(&cm), [Some(90), Some(45), Some(16), Some(300)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clear_wins_over_a_value_for_the_same_limit_when_applied() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;
    let both = WalOperation::PartitionUpdate {
        name: "gpu".into(),
        nodes: None,
        selector: None,
        state: None,
        max_time_minutes: Some(999),
        default_time_minutes: Some(999),
        max_nodes: Some(999),
        min_nodes: None,
        allow_accounts: None,
        allow_groups: None,
        deny_accounts: None,
        deny_qos: None,
        allow_qos: None,
        priority_tier: None,
        preempt_mode: None,
        is_default: None,
        preempt_exempt_time: Some(999),
        grace_time: None,
        clear: PartitionClears {
            max_time: true,
            max_nodes: true,
            ..Default::default()
        },
        resource_limits: None,
    };

    apply_replicated(&cm, both);

    assert_eq!(limits_of(&cm), [None, Some(999), None, Some(999)]);
}

// Every entry written before clears existed holds `null` for "unchanged"; replaying
// one must not remove a limit. Never regenerate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_entry_with_null_limits_leaves_the_limits_untouched() {
    const NULLS: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":"DRAIN","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"grace_time":null,"resource_limits":null}}"#;
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;

    cm.apply_operation(&serde_json::from_str(NULLS).unwrap());

    assert_eq!(limits_of(&cm), [Some(60), Some(30), Some(8), Some(120)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_entry_with_numeric_limits_sets_them() {
    const NUMBERS: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":90,"default_time_minutes":45,"max_nodes":16,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":300}}"#;
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;

    cm.apply_operation(&serde_json::from_str(NUMBERS).unwrap());

    assert_eq!(limits_of(&cm), [Some(90), Some(45), Some(16), Some(300)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_drops_limits_removed_from_the_config_on_a_replica() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with_limited_partition(&dir).await;
    let from_config = Partition {
        max_time_minutes: None,
        default_time_minutes: Some(30),
        max_nodes: None,
        preempt_exempt_time: None,
        ..limited_partition()
    };

    apply_replicated(&cm, crate::partition_patch::wal_from_config(&from_config));

    assert_eq!(
        limits_of(&cm),
        [None, Some(30), None, Some(120)],
        "a limit absent from the config is cleared; preempt_exempt_time is only ever pushed when set"
    );
}
