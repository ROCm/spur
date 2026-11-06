// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller behavior of per-partition resource caps and memory policy.

use super::*;
use spur_core::config::EnforcePartLimits;
use spur_core::job::PartitionTresOvershoot;
use spur_core::partition_limits::MemoryAdjustment;

const GIB: u64 = 1024;

fn partition(name: &str, caps: &[(&str, u64)]) -> Partition {
    Partition {
        name: name.into(),
        nodes: "ALL".into(),
        max_tres_per_node: caps.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        ..Default::default()
    }
}

async fn cluster_with(
    dir: &TempDir,
    mode: EnforcePartLimits,
    partitions: Vec<Partition>,
) -> Arc<ClusterManager> {
    let mut config = test_config();
    config.scheduler.enforce_part_limits = mode;
    let cm = test_cluster_with_config(dir, config).await;
    for p in partitions {
        cm.create_partition(p).unwrap();
    }
    cm
}

fn spec_in(partition: &str, cpus: u32) -> JobSpec {
    JobSpec {
        partition: Some(partition.into()),
        cpus_per_task: cpus,
        ..basic_spec("capped")
    }
}

fn exclusive_in(partition: &str) -> JobSpec {
    JobSpec {
        exclusive: true,
        ..spec_in(partition, 1)
    }
}

fn update_limits(
    cm: &ClusterManager,
    name: &str,
    limits: PartitionLimitsUpdate,
) -> Result<(), PartitionError> {
    cm.update_partition(
        name, None, None, None, None, None, None, None, false, None, None, None, None, None, None,
        None, None, None, None, limits,
    )
}

fn schedulable(cm: &ClusterManager, id: JobId) -> bool {
    cm.pending_jobs_and_tag_reasons()
        .iter()
        .any(|j| j.job_id == id)
}

fn submit_err(cm: &ClusterManager, spec: JobSpec) -> String {
    match cm.submit_job(spec) {
        Err(SubmitError::InvalidArgument(msg)) => msg,
        other => panic!("expected an invalid-argument rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_runnable_exclusive_job_pends_with_a_description_until_the_cap_is_raised() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 4)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    let id = submit_and_wait(&cm, exclusive_in("cpu"));

    assert!(!schedulable(&cm, id));
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.pending_reason, PendingReason::PartitionTresLimit);
    assert_eq!(
        job.state_reason_display(),
        "Partition cpu can never run this job as configured: exclusive job needs 16 CPUs, \
         max_tres_per_node.cpu is 4 on every eligible node"
    );

    update_limits(
        &cm,
        "cpu",
        PartitionLimitsUpdate {
            max_cpus_per_node: Some(Some(16)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(schedulable(&cm, id), "a raised cap makes the job runnable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_runnable_jobs_are_rejected_at_submit_under_all() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::All,
        vec![partition("cpu", &[("cpu", 4)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);

    let exclusive = submit_err(&cm, exclusive_in("cpu"));
    assert!(
        exclusive.contains("exclusive job needs 16 CPUs"),
        "{exclusive}"
    );
    let per_node = submit_err(&cm, spec_in("cpu", 8));
    assert!(per_node.contains("job needs 8 CPUs"), "{per_node}");

    assert!(cm.submit_job(spec_in("cpu", 4)).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_runnable_per_node_request_pends_under_no() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("gpu-free", &[("gres/gpu", 0)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    let mut spec = spec_in("gpu-free", 1);
    spec.gres = vec!["gpu:1".into()];
    let id = submit_and_wait(&cm, spec);

    assert!(!schedulable(&cm, id));
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.pending_reason, PendingReason::PartitionTresLimit);
    assert!(
        job.state_reason_display().ends_with(
            "job needs 1 gres/gpu, max_tres_per_node.gres/gpu is 0 on every eligible node"
        ),
        "{}",
        job.state_reason_display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_total_the_cap_can_never_spread_pends_as_never_runnable() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("gpu", &[("gres/gpu", 1)])],
    )
    .await;
    for name in ["n1", "n2"] {
        register_gpu_node(
            &cm,
            name,
            (0..4).map(|i| gpu_resource(i, i.into())).collect(),
        );
    }
    let id = submit_and_wait(
        &cm,
        JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            gpus: Some(spur_core::gpu_request::GpuRequest {
                count: 4,
                gpu_type: None,
            }),
            ..spec_in("gpu", 1)
        },
    );

    assert!(!schedulable(&cm, id));
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.pending_reason, PendingReason::PartitionTresLimit);
    assert!(
        job.state_reason_display().ends_with(
            "job needs 2 gres/gpu, max_tres_per_node.gres/gpu is 1 on every eligible node"
        ),
        "{}",
        job.state_reason_display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforce_part_limits_any_and_all_weigh_every_requested_partition() {
    let parts = || vec![partition("cpu", &[("cpu", 4)]), partition("wide", &[])];

    let dir = TempDir::new().unwrap();
    let any = cluster_with(&dir, EnforcePartLimits::Any, parts()).await;
    register_node(&any, "n1", 16, 64 * GIB);
    let id = submit_and_wait(&any, spec_in("cpu,wide", 8));
    assert!(
        schedulable(&any, id),
        "an uncapped candidate keeps the job runnable"
    );

    let dir = TempDir::new().unwrap();
    let all = cluster_with(&dir, EnforcePartLimits::All, parts()).await;
    register_node(&all, "n1", 16, 64 * GIB);
    let msg = submit_err(&all, spec_in("cpu,wide", 8));
    assert!(msg.contains("Partition cpu"), "{msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_mem_per_node_is_a_partition_limit_governed_by_enforce_part_limits() {
    let limited = || Partition {
        max_mem_per_node: Some(GIB),
        ..partition("m", &[])
    };
    let big_mem = |partition: &str| JobSpec {
        memory_per_node_mb: Some(2 * GIB),
        ..spec_in(partition, 1)
    };

    let dir = TempDir::new().unwrap();
    let all = cluster_with(
        &dir,
        EnforcePartLimits::All,
        vec![limited(), partition("other", &[])],
    )
    .await;
    let msg = submit_err(&all, big_mem("m"));
    assert!(msg.contains("max_mem_per_node 1024M"), "{msg}");
    assert!(submit_err(&all, big_mem("m,other")).contains("max_mem_per_node"));

    let dir = TempDir::new().unwrap();
    let any = cluster_with(
        &dir,
        EnforcePartLimits::Any,
        vec![limited(), partition("other", &[])],
    )
    .await;
    assert!(any.submit_job(big_mem("m,other")).is_ok());
    assert!(submit_err(&any, big_mem("m")).contains("max_mem_per_node"));

    let dir = TempDir::new().unwrap();
    let no = cluster_with(&dir, EnforcePartLimits::No, vec![limited()]).await;
    let id = submit_and_wait(&no, big_mem("m"));
    assert!(!schedulable(&no, id));
    assert_eq!(
        no.get_job(id).unwrap().pending_reason,
        PendingReason::PartitionConfig
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_only_overshoot_never_rejects_at_submit() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::All,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    {
        let mut running = make_running_job(500, &["n1"], 8);
        running.spec.partition = Some("cpu".into());
        running.effective_partition = Some("cpu".into());
        cm.jobs.write().insert(500, running);
    }
    assert!(cm.submit_job(spec_in("cpu", 4)).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforce_part_limits_adds_no_checks_without_caps() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(&dir, EnforcePartLimits::All, vec![partition("plain", &[])]).await;
    register_node(&cm, "n1", 4, GIB);
    let spec = JobSpec {
        memory_per_node_mb: Some(512 * GIB),
        ..exclusive_in("plain")
    };
    assert!(cm.submit_job(spec).is_ok());
}

#[test]
fn quota_probes_are_each_admitting_partitions_adjusted_request() {
    let partitions = vec![
        Partition {
            max_mem_per_cpu: Some(2 * GIB),
            ..partition("a", &[])
        },
        Partition {
            max_mem_per_cpu: Some(GIB),
            ..partition("b", &[])
        },
        partition("plain", &[]),
        Partition {
            state: spur_core::partition::PartitionState::Down,
            ..partition("down", &[])
        },
    ];
    let job = Job::new(
        1,
        JobSpec {
            memory_per_cpu_mb: Some(3 * GIB),
            ..spec_in("b,down,a,plain", 2)
        },
    );
    let probes: Vec<(String, u32)> = quota_probes(&job, &partitions)
        .into_iter()
        .map(|p| (p.spec.partition.unwrap_or_default(), p.spec.cpus_per_task))
        .collect();
    assert_eq!(
        probes,
        [("b".into(), 6), ("a".into(), 4), ("plain".into(), 2)],
        "request order, one probe per request, the Down partition left out"
    );
}

#[test]
fn a_claim_covers_the_most_of_each_resource_among_admitted_requests() {
    let probe = |partition: &str, cpus: u32, mem: u64| {
        Job::new(
            1,
            JobSpec {
                memory_per_node_mb: Some(mem),
                ..spec_in(partition, cpus)
            },
        )
    };
    let admitted = |qos_charge| QuotaVerdict::Admitted {
        account: AccountAdmitted {
            charge: 1,
            admits_without_credit: true,
        },
        qos_charge,
    };
    let outcomes = [
        (probe("cpus", 4, GIB), admitted(1)),
        (
            probe("big", 8, 8 * GIB),
            QuotaVerdict::AccountBlocked(PendingReason::AssocGrpCpuLimit),
        ),
        (probe("mem", 2, 4 * GIB), admitted(0)),
    ];
    let claim = QuotaClaim::of_admitted(&outcomes).unwrap();
    assert_eq!(
        (
            claim.tres.get(TresType::Cpu),
            claim.tres.get(TresType::Memory),
            claim.qos_charge,
        ),
        (4, 4 * GIB, 1)
    );
    let mut job = probe("cpus,big,mem", 1, GIB);
    claim.narrow(&mut job);
    assert_eq!(job.spec.partition.as_deref(), Some("cpus,mem"));

    let refused = [(
        probe("big", 8, 8 * GIB),
        QuotaVerdict::AccountBlocked(PendingReason::AssocGrpCpuLimit),
    )];
    assert!(QuotaClaim::of_admitted(&refused).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_default_in_one_candidate_does_not_strand_the_job() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![
            partition("a", &[]),
            Partition {
                def_mem_per_node: Some(512 * GIB),
                ..partition("b", &[])
            },
        ],
    )
    .await;
    register_node(&cm, "n1", 8, 16 * GIB);
    let id = submit_and_wait(&cm, spec_in("a,b", 1));
    assert!(schedulable(&cm, id), "partition a can still run it");
    assert_eq!(cm.get_job(id).unwrap().spec.memory_per_node_mb, None);
}

#[test]
fn a_placement_is_planned_with_the_chosen_partitions_adjusted_request() {
    use spur_sched::partition_cap::planned_memory_adjustment;
    let p = Partition {
        max_mem_per_cpu: Some(2 * GIB),
        ..partition("cpu", &[])
    };
    let job = |mem_per_cpu, mem_per_node| {
        Job::new(
            1,
            JobSpec {
                memory_per_cpu_mb: mem_per_cpu,
                memory_per_node_mb: mem_per_node,
                ..spec_in("cpu", 2)
            },
        )
    };
    let cpus = |job: &Job| {
        planned_memory_adjustment(job, Some(&p))
            .unwrap()
            .cpus_per_task
    };

    assert_eq!(
        planned_memory_adjustment(&job(Some(3 * GIB), None), Some(&p)),
        Some(MemoryAdjustment {
            cpus_per_task: 4,
            memory_per_node_mb: None,
            memory_per_cpu_mb: Some(1536),
            memory_per_gpu_mb: None,
        })
    );
    assert_eq!(cpus(&job(Some(5 * GIB), None)), 6);
    assert_eq!(cpus(&job(None, Some(8 * GIB))), 4);
    assert_eq!(
        planned_memory_adjustment(&job(Some(GIB), None), Some(&p)),
        None
    );
    assert_eq!(planned_memory_adjustment(&job(None, None), None), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_memory_is_applied_at_start_when_the_job_names_none() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![Partition {
            def_mem_per_cpu: Some(512),
            ..partition("d", &[])
        }],
    )
    .await;
    register_node(&cm, "n1", 8, 16 * GIB);
    let id = submit_and_wait(&cm, spec_in("d", 2));
    let job = cm.get_job(id).unwrap();
    let partitions = cm.get_partitions();
    let d = partitions.iter().find(|p| p.name == "d");
    let attribution = RunAttribution {
        effective_partition: Some("d".into()),
        memory_adjustment: spur_sched::partition_cap::planned_memory_adjustment(&job, d),
        ..Default::default()
    };
    let res = scalar_alloc(2, GIB);
    cm.reserve_attributed_placement(
        id,
        vec!["n1".into()],
        res.clone(),
        per_node_for(&["n1"], res),
        false,
        attribution,
    )
    .unwrap();
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.spec.memory_per_cpu_mb, Some(512));
    assert_eq!(job.effective_partition.as_deref(), Some("d"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effective_partition_and_adjusted_request_survive_a_controller_restart() {
    let dir = TempDir::new().unwrap();
    let first = Arc::new(ClusterManager::new(test_config(), dir.path()).unwrap());
    let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), first.clone())
        .await
        .unwrap();
    handle
        .raft
        .wait(Some(std::time::Duration::from_secs(5)))
        .metrics(|m| m.current_leader == Some(1), "leader elected")
        .await
        .unwrap();
    first.set_raft(handle.raft.clone());
    register_node(&first, "n1", 8, 16 * GIB);
    let id = submit_and_wait(&first, spec_in("default", 2));
    let res = scalar_alloc(4, GIB);
    first
        .reserve_attributed_placement(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
            false,
            RunAttribution {
                idle_fill: false,
                effective_partition: Some("default".into()),
                memory_adjustment: Some(MemoryAdjustment {
                    cpus_per_task: 4,
                    memory_per_node_mb: None,
                    memory_per_cpu_mb: Some(1536),
                    memory_per_gpu_mb: None,
                }),
            },
        )
        .unwrap();
    handle.raft.shutdown().await.unwrap();

    let restarted = test_cluster(&dir).await;
    wait_for("job replayed", || {
        restarted.get_job(id).is_some_and(|j| j.holds_a_placement())
    });
    let job = restarted.get_job(id).unwrap();
    assert_eq!(job.effective_partition.as_deref(), Some("default"));
    assert_eq!(job.spec.cpus_per_task, 4);
    assert_eq!(job.spec.memory_per_cpu_mb, Some(1536));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_block_sets_the_descriptive_pending_reason() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    let id = submit_and_wait(&cm, spec_in("default", 24));
    let job = cm.get_job(id).unwrap();
    let blocks = HashMap::from([(
        id,
        spur_sched::backfill::PartitionCapBlock {
            partition: "cpu".into(),
            overshoots: vec![PartitionTresOvershoot {
                node: "n03".into(),
                resource: "cpu".into(),
                used: 48,
                requested: 24,
                limit: 64,
            }],
        },
    )]);
    cm.annotate_partition_cap_pending(&[&job], &blocks);

    let job = cm.get_job(id).unwrap();
    assert_eq!(job.pending_reason, PendingReason::PartitionTresLimit);
    assert_eq!(
        job.state_reason_display(),
        "Partition cpu limit reached on n03: cpu 48 in use + 24 requested = 72 > max 64"
    );

    cm.hold_job(id).unwrap();
    wait_for("held", || {
        cm.get_job(id).unwrap().pending_reason.is_scheduling_hold()
    });
    let held = cm.get_job(id).unwrap();
    cm.annotate_partition_cap_pending(&[&held], &blocks);
    assert!(cm.get_job(id).unwrap().pending_reason.is_scheduling_hold());
}

#[test]
fn overcommitted_caps_are_reported_per_node_and_resource() {
    let mut node = Node::new(
        "n1".into(),
        ResourceSet {
            cpus: 16,
            memory_mb: 64 * GIB,
            ..Default::default()
        },
    );
    node.partitions = vec!["a".into(), "b".into(), "solo".into()];
    let partitions = vec![
        partition("a", &[("cpu", 12), ("mem", 32 * GIB)]),
        partition("b", &[("cpu", 8), ("mem", 32 * GIB)]),
        partition("solo", &[("gres/gpu", 64)]),
        partition("elsewhere", &[("cpu", 64)]),
    ];
    assert_eq!(
        partition_cap_overcommits(&node, &partitions),
        vec![CapOvercommit {
            key: "cpu".into(),
            total: 20,
            capacity: 16,
            partitions: vec!["a".into(), "b".into()],
        }]
    );

    node.total_resources.cpus = 0;
    assert!(partition_cap_overcommits(&node, &partitions).is_empty());
}

#[test]
fn typed_gpu_capacity_counts_only_that_type() {
    let gpu = |ty: &str| spur_core::resource::GpuResource {
        device_id: 0,
        gpu_type: ty.into(),
        memory_mb: 0,
        peer_gpus: Vec::new(),
        link_type: spur_core::resource::GpuLinkType::PCIe,
        topology: None,
        stable_id: 0,
    };
    let resources = ResourceSet {
        gpus: vec![gpu("a100"), gpu("a100"), gpu("mi300x")],
        ..Default::default()
    };
    assert_eq!(node_tres_capacity(&resources, "gres/gpu"), 3);
    assert_eq!(node_tres_capacity(&resources, "gres/gpu:a100"), 2);
    assert_eq!(node_tres_capacity(&resources, "gres/gpu:h100"), 0);
}

#[test]
fn limits_update_merges_validates_and_clears() {
    let current = Partition {
        max_mem_per_node: Some(GIB),
        ..partition("p", &[("cpu", 8), ("mem", 4 * GIB)])
    };
    assert_eq!(PartitionLimitsUpdate::default().merged(&current), Ok(None));

    let cpu_only = PartitionLimitsUpdate {
        max_cpus_per_node: Some(Some(16)),
        ..Default::default()
    }
    .merged(&current)
    .unwrap()
    .unwrap();
    assert_eq!(cpu_only.max_tres_per_node["cpu"], 16);
    assert_eq!(cpu_only.max_tres_per_node["mem"], 4 * GIB);
    assert_eq!(cpu_only.max_mem_per_node, Some(GIB));

    let cleared = PartitionLimitsUpdate {
        max_cpus_per_node: Some(None),
        max_mem_per_node: Some(None),
        ..Default::default()
    }
    .merged(&current)
    .unwrap()
    .unwrap();
    assert!(!cleared.max_tres_per_node.contains_key("cpu"));
    assert_eq!(cleared.max_mem_per_node, None);

    let replaced = PartitionLimitsUpdate {
        max_tres_per_node: Some(BTreeMap::from([("gres/gpu".to_string(), 0)])),
        max_cpus_per_node: Some(Some(4)),
        ..Default::default()
    }
    .merged(&current)
    .unwrap()
    .unwrap();
    assert_eq!(
        replaced.max_tres_per_node,
        BTreeMap::from([("cpu".to_string(), 4), ("gres/gpu".to_string(), 0)])
    );

    let conflict = PartitionLimitsUpdate {
        max_tres_per_node: Some(BTreeMap::from([("cpu".to_string(), 4)])),
        max_cpus_per_node: Some(Some(4)),
        ..Default::default()
    };
    assert!(conflict
        .merged(&current)
        .unwrap_err()
        .contains("max_cpus_per_node"));

    let both_max = PartitionLimitsUpdate {
        max_mem_per_cpu: Some(Some(GIB)),
        ..Default::default()
    };
    assert!(
        both_max.merged(&current).is_err(),
        "max_mem_per_node is already set"
    );

    let bad_key = PartitionLimitsUpdate {
        max_tres_per_node: Some(BTreeMap::from([("cpus".to_string(), 4)])),
        ..Default::default()
    };
    assert!(bad_key.merged(&current).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_api_rejects_invalid_limits_and_leaves_the_partition_unchanged() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("p", &[("cpu", 8)])],
    )
    .await;
    let bad = Partition {
        def_mem_per_cpu: Some(1),
        def_mem_per_node: Some(1),
        ..partition("q", &[])
    };
    assert!(matches!(
        cm.create_partition(bad),
        Err(PartitionError::InvalidArgument(_))
    ));
    let err = update_limits(
        &cm,
        "p",
        PartitionLimitsUpdate {
            max_tres_per_node: Some(BTreeMap::from([("disk".to_string(), 1)])),
            ..Default::default()
        },
    );
    assert!(matches!(err, Err(PartitionError::InvalidArgument(_))));
    let p = cm
        .get_partitions()
        .into_iter()
        .find(|p| p.name == "p")
        .unwrap();
    assert_eq!(
        p.max_tres_per_node,
        BTreeMap::from([("cpu".to_string(), 8)])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_changes_caps_on_an_existing_partition() {
    let dir = TempDir::new().unwrap();
    let base = "cluster_name = \"test\"\n\n[[partitions]]\nname = \"cpu\"\nnodes = \"ALL\"\n";
    let (cm, conf_path) =
        test_cluster_with_conf_file(&dir, &format!("{base}max_cpus_per_node = 8\n")).await;
    let cpu = |cm: &Arc<ClusterManager>| {
        cm.get_partitions()
            .into_iter()
            .find(|p| p.name == "cpu")
            .unwrap()
    };
    assert_eq!(cpu(&cm).max_tres_per_node["cpu"], 8);

    std::fs::write(
        &conf_path,
        format!(
            "{base}max_mem_per_cpu = 2048\n[partitions.max_tres_per_node]\ncpu = 16\nmem = 65536\n"
        ),
    )
    .unwrap();
    cm.reconfigure().unwrap();
    let p = cpu(&cm);
    assert_eq!(
        p.max_tres_per_node,
        BTreeMap::from([("cpu".to_string(), 16), ("mem".to_string(), 65536)])
    );
    assert_eq!(p.max_mem_per_cpu, Some(2048));

    std::fs::write(&conf_path, base).unwrap();
    cm.reconfigure().unwrap();
    let p = cpu(&cm);
    assert!(p.max_tres_per_node.is_empty());
    assert_eq!(p.max_mem_per_cpu, None);
}

fn cpu_overcommit(total: u64, partitions: &[&str]) -> (String, CapOvercommit) {
    (
        "n1".into(),
        CapOvercommit {
            key: "cpu".into(),
            total,
            capacity: 16,
            partitions: partitions.iter().map(|p| p.to_string()).collect(),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_create_and_update_can_overcommit_a_node() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("a", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    assert!(cm.node_cap_overcommits(|_| true).is_empty());

    cm.create_partition(partition("b", &[("cpu", 8)])).unwrap();
    assert!(cm.node_cap_overcommits(|_| true).is_empty());

    update_limits(
        &cm,
        "a",
        PartitionLimitsUpdate {
            max_cpus_per_node: Some(Some(12)),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        cm.node_cap_overcommits(|_| true),
        vec![cpu_overcommit(20, &["a", "b"])]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_can_overcommit_a_node() {
    let dir = TempDir::new().unwrap();
    let conf = |b_cpus: u64| {
        format!(
            "cluster_name = \"test\"\n\n\
             [[partitions]]\nname = \"a\"\nnodes = \"ALL\"\nmax_cpus_per_node = 8\n\n\
             [[partitions]]\nname = \"b\"\nnodes = \"ALL\"\nmax_cpus_per_node = {b_cpus}\n"
        )
    };
    let (cm, conf_path) = test_cluster_with_conf_file(&dir, &conf(8)).await;
    register_node(&cm, "n1", 16, 64 * GIB);
    assert!(cm.node_cap_overcommits(|_| true).is_empty());

    std::fs::write(&conf_path, conf(10)).unwrap();
    cm.reconfigure().unwrap();
    assert_eq!(
        cm.node_cap_overcommits(|_| true),
        vec![cpu_overcommit(18, &["a", "b"])]
    );
}

#[test]
fn limit_block_reports_a_max_mem_per_node_violation() {
    let p = Partition {
        max_mem_per_node: Some(GIB),
        ..partition("m", &[])
    };
    let mut job = Job::new(1, spec_in("m", 1));
    job.spec.memory_per_node_mb = Some(GIB);
    assert_eq!(p.limit_block(&job.spec), None);
    job.spec.memory_per_node_mb = Some(GIB + 1);
    assert_eq!(
        p.limit_block(&job.spec),
        Some(PendingReason::PartitionConfig)
    );
}

/// One scheduling pass composed from the loop's own steps, minus agent RPCs:
/// every placement is reserved and activated as a confirmed dispatch would be.
fn run_pass(cm: &ClusterManager) -> Vec<JobId> {
    use spur_sched::backfill::BackfillScheduler;

    let mut trial = BackfillScheduler::new(100);
    let (classification, unclaimed) =
        crate::scheduler_loop::classify_without_phantom_claims(cm, |c| {
            crate::scheduler_loop::trial_placement(cm, &mut trial, c)
        });
    let (pending, _) = cm.apply_classification(classification);
    let nodes = cm.nodes_off_dispatch_cooldown(&pending);
    let partitions = cm.get_partitions();
    let reservations = cm.get_reservations();
    let busy_until = crate::scheduler_loop::running_jobs_busy_until(cm);
    let usage = crate::scheduler_loop::running_partition_usage(cm);
    let handoffs = cm.preemption_handoffs(&pending, &nodes, &reservations);
    let state = spur_sched::traits::ClusterState {
        nodes: &nodes,
        partitions: &partitions,
        reservations: &reservations,
        topology: None,
        fabric: None,
        busy_until: &busy_until,
        preemption_handoffs: &handoffs,
        partition_node_usage: &usage,
    };
    let mut scheduler = BackfillScheduler::new(100);
    let assignments =
        crate::scheduler_loop::claimed_assignments(&mut scheduler, &pending, &state, &unclaimed);
    let unscheduled: Vec<&Job> = pending
        .iter()
        .filter(|j| !assignments.iter().any(|a| a.job_id == j.job_id))
        .collect();
    cm.update_pending_reasons(&unscheduled, &state);
    cm.annotate_partition_cap_pending(&unscheduled, scheduler.partition_cap_blocks());
    for a in &assignments {
        let job = cm.get_job(a.job_id).unwrap();
        let attribution = RunAttribution::for_assignment(a, false);
        let resources =
            crate::scheduler_loop::compute_job_allocation(&job, &a.nodes, &a.per_node_alloc);
        activate(
            cm,
            a.job_id,
            &a.nodes,
            resources,
            &a.per_node_alloc,
            attribution,
        );
    }
    assignments.iter().map(|a| a.job_id).collect()
}

fn activate(
    cm: &ClusterManager,
    id: JobId,
    nodes: &[String],
    resources: ResourceAllocations,
    per_node_alloc: &HashMap<String, ResourceAllocations>,
    attribution: RunAttribution,
) {
    let run_attempt = cm
        .reserve_attributed_placement(
            id,
            nodes.to_vec(),
            resources.clone(),
            per_node_alloc.clone(),
            false,
            attribution,
        )
        .unwrap();
    cm.activate_job(
        id,
        run_attempt,
        nodes.to_vec(),
        resources,
        per_node_alloc.clone(),
        false,
        false,
    )
    .unwrap();
    settle(cm, id, JobState::Running);
}

/// Submit `spec` and run it on `node` holding `cpus`, charged to its partition.
fn run_on(cm: &ClusterManager, spec: JobSpec, node: &str, cpus: u32) -> JobId {
    let partition = spec.partition.clone();
    let id = submit_and_wait(cm, spec);
    let res = scalar_alloc(cpus, GIB);
    activate(
        cm,
        id,
        &[node.to_string()],
        res.clone(),
        &per_node_for(&[node], res),
        RunAttribution {
            effective_partition: partition,
            ..Default::default()
        },
    );
    id
}

/// CPUs each node holds against `partition`'s cap in the next pass's usage map.
fn cap_usage(cm: &ClusterManager, partition: &str) -> BTreeMap<String, u64> {
    crate::scheduler_loop::running_partition_usage(cm)
        .get(partition)
        .map(|by_node| {
            by_node
                .iter()
                .map(|(node, charges)| {
                    let cpus = charges.iter().map(|c| c.tres.get("cpu")).sum();
                    (node.clone(), cpus)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn reason(cm: &ClusterManager, id: JobId) -> (PendingReason, String) {
    let job = cm.get_job(id).unwrap();
    (job.pending_reason.clone(), job.state_reason_display())
}

fn in_account(spec: JobSpec, account: &str) -> JobSpec {
    JobSpec {
        account: Some(account.into()),
        ..spec
    }
}

fn limit_account_cpus(cm: &ClusterManager, cpus: u64) {
    let mut grp = TresRecord::new();
    grp.set(TresType::Cpu, cpus);
    cm.association_cache().insert_limits(
        "testuser",
        "research",
        spur_core::accounting::AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_association_limit_still_blocks_a_job_the_cap_admits() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    run_on(&cm, in_account(spec_in("default", 4), "research"), "n1", 4);
    limit_account_cpus(&cm, 8);
    let id = submit_and_wait(&cm, in_account(spec_in("cpu", 6), "research"));

    assert!(run_pass(&cm).is_empty());
    assert_eq!(reason(&cm, id).0, PendingReason::AssocGrpCpuLimit);

    limit_account_cpus(&cm, 16);
    assert_eq!(run_pass(&cm), [id], "the cap alone admits the job");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_qos_limit_still_blocks_a_job_the_cap_admits() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    let mut per_job = TresRecord::new();
    per_job.set(TresType::Cpu, 4);
    cm.qos_cache().insert(Qos {
        name: "small".into(),
        limits: spur_core::accounting::QosLimits {
            max_tres_per_job: Some(per_job),
            ..Default::default()
        },
        ..Default::default()
    });
    let id = submit_and_wait(
        &cm,
        JobSpec {
            qos: Some("small".into()),
            ..spec_in("cpu", 6)
        },
    );

    assert!(run_pass(&cm).is_empty());
    assert_eq!(reason(&cm, id).0, PendingReason::QosMaxCpuPerJobLimit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_failing_gate_names_the_pending_reason() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 32, 64 * GIB);
    run_on(&cm, spec_in("cpu", 6), "n1", 6);
    run_on(
        &cm,
        in_account(spec_in("default", 10), "research"),
        "n1",
        10,
    );
    limit_account_cpus(&cm, 12);
    let never = submit_and_wait(&cm, in_account(spec_in("cpu", 12), "research"));
    let full = submit_and_wait(&cm, in_account(spec_in("cpu", 4), "research"));

    assert!(run_pass(&cm).is_empty());
    let (code, desc) = reason(&cm, never);
    assert_eq!(code, PendingReason::PartitionTresLimit);
    assert!(desc.contains("can never run"), "{desc}");
    assert_eq!(
        reason(&cm, full).0,
        PendingReason::AssocGrpCpuLimit,
        "the association gate runs before placement checks the cap"
    );

    limit_account_cpus(&cm, 100);
    assert!(run_pass(&cm).is_empty());
    assert_eq!(
        reason(&cm, full),
        (
            PendingReason::PartitionTresLimit,
            "Partition cpu limit reached on n1: cpu 6 in use + 4 requested = 10 > max 8".into()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_blocked_only_by_a_full_cap_holds_no_grp_node_quota() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 4)])],
    )
    .await;
    for n in ["n1", "n2", "n3"] {
        register_node(&cm, n, 16, 64 * GIB);
    }
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, 2);
    cm.qos_cache().insert(Qos {
        name: "pool".into(),
        limits: spur_core::accounting::QosLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
        ..Default::default()
    });
    let pool = |cpus: u32| JobSpec {
        qos: Some("pool".into()),
        ..spec_in("cpu", cpus)
    };
    // Fills n1's cap and leaves it too little room for `fits` to be credited
    // reuse of n1, so `fits` needs a node of the group's quota.
    run_on(&cm, pool(4), "n1", 4);
    run_on(&cm, spec_in("default", 10), "n1", 10);
    let capped = submit_and_wait(
        &cm,
        JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            nodelist: Some("n1,n2".into()),
            ..pool(1)
        },
    );
    let fits = submit_and_wait(&cm, pool(4));

    let single: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
    assert_eq!(
        single,
        [capped],
        "one classification charges capped's claim"
    );

    assert_eq!(run_pass(&cm), [fits]);
    assert_eq!(
        reason(&cm, capped),
        (
            PendingReason::PartitionTresLimit,
            "Partition cpu limit reached on n1: cpu 4 in use + 1 requested = 5 > max 4".into()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preemption_hands_a_full_cap_to_the_preemptor_without_double_counting() {
    let dir = TempDir::new().unwrap();
    let mut config = test_config();
    config.scheduler.preempt_type = spur_core::partition::PreemptType::QosPriority;
    let cm = test_cluster_with_config(&dir, config).await;
    cm.create_partition(Partition {
        preempt_mode: Some(PreemptMode::Requeue),
        ..partition("cpu", &[("cpu", 8)])
    })
    .unwrap();
    register_node(&cm, "n1", 16, 64 * GIB);
    cm.qos_cache().insert(Qos {
        name: "low".into(),
        priority: 100,
        ..Default::default()
    });
    cm.qos_cache().insert(Qos {
        name: "high".into(),
        priority: 10_000,
        preempt: vec!["low".into()],
        ..Default::default()
    });
    let in_qos = |qos: &str| JobSpec {
        qos: Some(qos.into()),
        time_limit: Some(chrono::Duration::hours(1)),
        ..spec_in("cpu", 8)
    };
    let low = run_on(&cm, in_qos("low"), "n1", 8);
    let high = submit_and_wait(&cm, in_qos("high"));
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n1".into(), 8)]));

    assert!(
        run_pass(&cm).is_empty(),
        "8 CPUs are free, but the cap is full"
    );
    assert_eq!(reason(&cm, high).0, PendingReason::PartitionTresLimit);

    let high_job = cm.get_job(high).unwrap();
    crate::scheduler_loop::try_preempt(
        &cm,
        &cm.get_partitions(),
        &cm.get_nodes(),
        &[&high_job],
        &cm.config().scheduler,
    )
    .await;
    settle(&cm, low, JobState::Pending);
    assert!(
        cap_usage(&cm, "cpu").is_empty(),
        "the requeued victim holds nothing against the cap"
    );

    assert_eq!(run_pass(&cm), [high]);
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n1".into(), 8)]));
    assert_eq!(cm.get_job(low).unwrap().state, JobState::Pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn array_tasks_run_up_to_the_cap_and_start_one_per_freed_slot() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("array", &[("cpu", 4)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    let parent = cm
        .submit_job(JobSpec {
            array_spec: Some("0-9".into()),
            time_limit: Some(chrono::Duration::hours(1)),
            ..spec_in("array", 1)
        })
        .unwrap()
        .job_id;
    let tasks = || -> Vec<Job> {
        cm.get_jobs(&JobFilter::default())
            .into_iter()
            .filter(|j| j.spec.array_job_id == Some(parent))
            .collect()
    };
    wait_for("array tasks applied", || tasks().len() == 10);

    let mut running = run_pass(&cm);
    for finished_so_far in 0..10 {
        assert_eq!(running.len(), (10 - finished_so_far).min(4));
        assert_eq!(
            cap_usage(&cm, "array").get("n1").copied().unwrap_or(0),
            running.len() as u64
        );
        for task in tasks().iter().filter(|j| j.state == JobState::Pending) {
            assert_eq!(task.pending_reason, PendingReason::PartitionTresLimit);
        }
        let finished = running.remove(0);
        cm.complete_job(finished, 0, JobState::Completed).unwrap();
        settle(&cm, finished, JobState::Completed);
        running.extend(run_pass(&cm));
    }
    assert!(running.is_empty());
    assert!(tasks().iter().all(|j| j.state == JobState::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_submission_cannot_set_or_borrow_a_partition_cap() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![
            partition("low", &[("cpu", 8)]),
            partition("high", &[("cpu", 64)]),
        ],
    )
    .await;
    register_node(&cm, "n1", 64, 256 * GIB);
    let caps = || -> Vec<_> {
        cm.get_partitions()
            .into_iter()
            .map(|p| (p.name, p.max_tres_per_node, p.max_mem_per_node))
            .collect()
    };
    let configured = caps();
    run_on(&cm, spec_in("low", 4), "n1", 4);
    let id = submit_and_wait(
        &cm,
        JobSpec {
            comment: Some("max_tres_per_node=cpu=64".into()),
            environment: [("SPUR_MAX_CPUS_PER_NODE".to_string(), "64".to_string())].into(),
            ..spec_in("low", 6)
        },
    );

    assert!(run_pass(&cm).is_empty());
    assert_eq!(caps(), configured);
    assert_eq!(
        reason(&cm, id).1,
        "Partition low limit reached on n1: cpu 4 in use + 6 requested = 10 > max 8",
        "judged against its own partition's configured cap, never high's"
    );
    assert!(cap_usage(&cm, "high").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_node_leaves_no_partition_usage_behind() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    register_node(&cm, "n2", 16, 64 * GIB);
    let on_n1 = run_on(&cm, spec_in("cpu", 8), "n1", 8);
    run_on(&cm, spec_in("cpu", 4), "n2", 4);
    assert_eq!(
        cap_usage(&cm, "cpu"),
        BTreeMap::from([("n1".into(), 8), ("n2".into(), 4)])
    );

    cm.remove_node("n1", true, Some("decommissioned".into()))
        .unwrap();
    wait_for("n1 removed", || cm.get_node("n1").is_none());
    assert_eq!(cm.get_job(on_n1).unwrap().state, JobState::NodeFail);
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n2".into(), 4)]));

    let one_hour = |cpus| JobSpec {
        time_limit: Some(chrono::Duration::hours(1)),
        ..spec_in("cpu", cpus)
    };
    let too_big = submit_and_wait(&cm, one_hour(6));
    let fits = submit_and_wait(&cm, one_hour(4));
    assert_eq!(run_pass(&cm), [fits]);
    assert_eq!(
        reason(&cm, too_big).1,
        "Partition cpu limit reached on n2: cpu 4 in use + 6 requested = 10 > max 8"
    );
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n2".into(), 8)]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_job_keeps_charging_the_cap_it_still_holds() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    let suspended = run_on(&cm, spec_in("cpu", 8), "n1", 8);
    cm.suspend_job(suspended, "testuser").unwrap();
    settle(&cm, suspended, JobState::Suspended);
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n1".into(), 8)]));

    let next = submit_and_wait(&cm, spec_in("cpu", 8));
    assert!(run_pass(&cm).is_empty());
    assert_eq!(reason(&cm, next).0, PendingReason::PartitionTresLimit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_the_job_finished_on_no_longer_charges_the_cap() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    register_node(&cm, "n2", 16, 64 * GIB);
    let id = submit_and_wait(
        &cm,
        JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            ..spec_in("cpu", 8)
        },
    );
    let nodes = ["n1".to_string(), "n2".to_string()];
    activate(
        &cm,
        id,
        &nodes,
        scalar_alloc(16, 2 * GIB),
        &per_node_for(&["n1", "n2"], scalar_alloc(8, GIB)),
        RunAttribution {
            effective_partition: Some("cpu".into()),
            ..Default::default()
        },
    );
    cm.apply_operation(&WalOperation::JobNodeComplete {
        job_id: id,
        node_name: "n1".into(),
        exit_code: 0,
        signal: 0,
        at: None,
        run_attempt: cm.get_job(id).unwrap().run_attempt,
    });
    assert_eq!(cm.get_job(id).unwrap().state, JobState::Completing);
    assert_eq!(cap_usage(&cm, "cpu"), BTreeMap::from([("n2".into(), 8)]));

    let replacement = submit_and_wait(&cm, spec_in("cpu", 8));
    assert_eq!(run_pass(&cm), [replacement]);
}

/// `a` allows 2 GiB per CPU and `b` 1 GiB, so `-c2 --mem-per-cpu=3G` needs 4
/// CPUs in `a` and 6 in `b`. `b` has the higher tier, so placement prefers it.
async fn per_cpu_limited_pair(dir: &TempDir) -> Arc<ClusterManager> {
    let cm = cluster_with(
        dir,
        EnforcePartLimits::No,
        vec![
            Partition {
                max_mem_per_cpu: Some(2 * GIB),
                ..partition("a", &[])
            },
            Partition {
                max_mem_per_cpu: Some(GIB),
                priority_tier: 5,
                ..partition("b", &[])
            },
        ],
    )
    .await;
    register_node(&cm, "n1", 16, 64 * GIB);
    cm
}

fn three_gib_per_cpu(partitions: &str) -> JobSpec {
    JobSpec {
        memory_per_cpu_mb: Some(3 * GIB),
        ..spec_in(partitions, 2)
    }
}

fn qos_with_max_cpus_per_job(cm: &ClusterManager, cpus: u64, deny_on_limit: bool) -> JobSpec {
    let mut per_job = TresRecord::new();
    per_job.set(TresType::Cpu, cpus);
    cm.qos_cache().insert(Qos {
        name: "small".into(),
        deny_on_limit,
        limits: spur_core::accounting::QosLimits {
            max_tres_per_job: Some(per_job),
            ..Default::default()
        },
        ..Default::default()
    });
    JobSpec {
        qos: Some("small".into()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_association_limit_is_checked_per_candidate_partition() {
    let dir = TempDir::new().unwrap();
    let cm = per_cpu_limited_pair(&dir).await;
    limit_account_cpus(&cm, 4);
    let id = submit_and_wait(&cm, in_account(three_gib_per_cpu("a,b"), "research"));

    assert_eq!(run_pass(&cm), [id], "a's 4 CPUs fit GrpTRES=cpu=4");
    let job = cm.get_job(id).unwrap();
    assert_eq!(job.effective_partition.as_deref(), Some("a"));
    assert_eq!(job.spec.cpus_per_task, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_qos_limit_is_checked_per_candidate_partition() {
    let dir = TempDir::new().unwrap();
    let cm = per_cpu_limited_pair(&dir).await;
    let qos = qos_with_max_cpus_per_job(&cm, 4, false);
    let id = submit_and_wait(
        &cm,
        JobSpec {
            qos: qos.qos,
            ..three_gib_per_cpu("a,b")
        },
    );

    assert_eq!(run_pass(&cm), [id]);
    assert_eq!(
        cm.get_job(id).unwrap().effective_partition.as_deref(),
        Some("a")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_no_candidate_admits_reports_the_first_failing_gate() {
    let dir = TempDir::new().unwrap();
    let cm = per_cpu_limited_pair(&dir).await;
    let id = submit_and_wait(&cm, in_account(three_gib_per_cpu("a,b"), "research"));
    limit_account_cpus(&cm, 3);

    assert!(run_pass(&cm).is_empty());
    assert_eq!(reason(&cm, id).0, PendingReason::AssocGrpCpuLimit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_on_limit_sees_each_candidate_partitions_adjusted_request() {
    let dir = TempDir::new().unwrap();
    let cm = per_cpu_limited_pair(&dir).await;
    let qos = qos_with_max_cpus_per_job(&cm, 4, true);
    let in_qos = |partitions| JobSpec {
        qos: qos.qos.clone(),
        ..three_gib_per_cpu(partitions)
    };

    let msg = submit_err(&cm, in_qos("b"));
    assert!(msg.contains("(QOSMaxCpuPerJobLimit)"), "{msg}");
    assert!(cm.submit_job(in_qos("a,b")).is_ok());
    assert!(cm.submit_job(in_qos("a")).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_on_limit_sees_a_partitions_default_memory() {
    let dir = TempDir::new().unwrap();
    let cm = cluster_with(
        &dir,
        EnforcePartLimits::No,
        vec![
            Partition {
                def_mem_per_node: Some(8 * GIB),
                ..partition("big", &[])
            },
            partition("plain", &[]),
        ],
    )
    .await;
    let mut per_job = TresRecord::new();
    per_job.set(TresType::Memory, 4 * GIB);
    cm.qos_cache().insert(Qos {
        name: "small".into(),
        deny_on_limit: true,
        limits: spur_core::accounting::QosLimits {
            max_tres_per_job: Some(per_job),
            ..Default::default()
        },
        ..Default::default()
    });
    let in_qos = |partitions| JobSpec {
        qos: Some("small".into()),
        ..spec_in(partitions, 1)
    };

    let msg = submit_err(&cm, in_qos("big"));
    assert!(msg.contains("(QOSMaxMemoryPerJob)"), "{msg}");
    assert!(cm.submit_job(in_qos("big,plain")).is_ok());
}
