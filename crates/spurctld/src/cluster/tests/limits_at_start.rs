// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! QOS and account limits charged as a scheduling pass starts jobs.

use super::*;

fn shared_pool_job(name: &str, cpus: u32) -> JobSpec {
    let mut spec = pool_job(name, 1, None);
    spec.exclusive = false;
    spec.cpus_per_task = cpus;
    spec
}

/// A QOS whose only limit is a group cap of `cap` nodes.
fn grp_node_capped(name: &str, cap: u64) -> Qos {
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, cap);
    capped_qos(
        name,
        spur_core::accounting::QosLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_pass_of_many_starts_stops_at_the_qos_grp_node_cap() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for i in 1..=12 {
        register_node(&cm, &format!("n{i}"), 8, 128000);
    }
    cm.qos_cache().insert(grp_node_capped("pool", 5));
    let ids: Vec<JobId> = (0..12)
        .map(|i| submit_and_wait(&cm, pool_job(&format!("j{i}"), 1, None)))
        .collect();

    let pass = one_pass(&cm);
    assert_eq!(pass.started(), ids[..5]);
    let refused: Vec<_> = ids[5..]
        .iter()
        .map(|id| (*id, PendingReason::QosGrpNodeLimit, None))
        .collect();
    assert_eq!(pass.limits.refusals(), refused);
}

/// The QOS runs on n1-n5, its cap, on 2 of 8 CPUs each, and n6-n12 are idle. Each
/// 2-CPU job fits only on the QOS's nodes, so all twelve pack there and start in
/// one pass: the cap counts nodes, not jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_jobs_packing_in_one_pass_start_past_the_cap_count_but_not_its_nodes() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    let nodes: Vec<String> = (1..=12).map(|i| format!("n{i}")).collect();
    for node in &nodes {
        register_node(&cm, node, 8, 128000);
    }
    cm.qos_cache().insert(grp_node_capped("pool", 5));
    let running: Vec<JobId> = nodes[..5]
        .iter()
        .map(|node| run_on(&cm, shared_pool_job("running", 2), node))
        .collect();
    let ids: Vec<JobId> = (0..12)
        .map(|i| submit_and_wait(&cm, shared_pool_job(&format!("packs-{i}"), 2)))
        .collect();

    let pass = one_pass(&cm);
    assert_eq!(pass.started(), ids);
    assert!(pass.limits.refusals().is_empty());
    let used: HashSet<String> = running
        .iter()
        .flat_map(|id| cm.get_job(*id).unwrap().allocated_nodes)
        .chain(pass.assignments.iter().flat_map(|a| a.nodes.clone()))
        .collect();
    assert_eq!(used, nodes[..5].iter().cloned().collect());
}

/// Even a job that packs onto one of the QOS's nodes is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_qos_grp_node_cap_lowered_below_its_running_jobs_starts_none_and_keeps_them() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    let nodes = ["n1", "n2", "n3"];
    for node in nodes {
        register_node(&cm, node, 8, 128000);
    }
    cm.qos_cache().insert(grp_node_capped("pool", 3));
    let running: Vec<JobId> = nodes
        .iter()
        .map(|node| run_on(&cm, shared_pool_job("running", 2), node))
        .collect();
    let packer = submit_and_wait(&cm, shared_pool_job("packer", 2));
    assert_eq!(one_pass(&cm).started(), vec![packer]);

    cm.qos_cache().insert(grp_node_capped("pool", 2));
    assert!(one_pass(&cm).started().is_empty());
    assert_eq!(
        cm.get_job(packer).unwrap().pending_reason,
        PendingReason::QosGrpNodeLimit
    );
    for (id, node) in running.iter().zip(nodes) {
        let job = cm.get_job(*id).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.allocated_nodes, vec![node.to_string()]);
    }
}

/// The account runs on n1 under a cap of two nodes, so the job fits at
/// classification. The cap drops to one before the job is placed, and the start
/// is checked against the cap in force.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_grp_node_cap_lowered_after_classification_refuses_the_start() {
    let dir = TempDir::new().unwrap();
    let cm = grp_node_cluster_with(&dir, 2, GrpCap::Account, test_config()).await;
    let mut spec = pool_job("fits-at-classification", 1, None);
    spec.account = Some("research".into());
    let job = submit_and_wait(&cm, spec);
    let ClassifiedPass {
        jobs, mut limits, ..
    } = cm.apply_classification(cm.classify_pending_jobs());
    assert_eq!(jobs.iter().map(|j| j.job_id).collect::<Vec<_>>(), [job]);

    let mut grp = TresRecord::new();
    grp.set(TresType::Node, 1);
    cm.association_cache().insert_limits(
        "testuser",
        "research",
        spur_core::accounting::AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
    );
    let mut scheduler = spur_sched::backfill::BackfillScheduler::new(10);
    let started = crate::scheduler_loop::place(&cm, &mut scheduler, &jobs, &mut limits);
    assert!(started.is_empty());
    assert_eq!(
        limits.refusals(),
        vec![(job, PendingReason::AssocGrpNodeLimit, None)]
    );
}

/// An account capped at one node runs a shared job on n1, and another account's job in
/// the same QOS runs on n2. A higher-priority job fills n1 in the same pass, so the
/// account's second job, credited for packing onto n1, waits for n1 rather than start
/// on n2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_account_node_cap_holds_when_a_packing_job_loses_its_node_in_the_pass() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for n in ["n1", "n2"] {
        register_node(&cm, n, 8, 128000);
    }
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, 1);
    let limits = spur_core::accounting::AccountLimits {
        grp_tres: Some(grp),
        ..Default::default()
    };
    cm.association_cache()
        .insert_limits("testuser", "research", limits);
    cm.association_cache().insert_association("testuser", "lab");
    cm.qos_cache()
        .insert(capped_qos("open", Default::default()));
    let half_node = |name: &str, account: &str| {
        let mut spec = basic_spec(name);
        spec.qos = Some("open".into());
        spec.account = Some(account.into());
        spec.cpus_per_task = 4;
        spec
    };
    run_on(&cm, half_node("other-account", "lab"), "n2");
    run_on(&cm, half_node("account-first", "research"), "n1");
    let mut fill = half_node("fill", "lab");
    fill.nodelist = Some("n1".into());
    fill.priority = Some(1000);
    let fill = submit_and_wait(&cm, fill);
    let second = submit_and_wait(&cm, half_node("account-second", "research"));

    let pass = one_pass(&cm);
    assert_eq!(pass.admitted(), vec![fill, second]);
    assert_eq!(pass.started(), vec![fill]);
    assert!(pass.limits.refusals().is_empty());
    let hinted = &pass.pending[1].preferred_nodes;
    assert_eq!(hinted.iter().collect::<Vec<_>>(), ["n1"]);
}

/// n1 and n2, and QOS "burst" capped at grp_tres node=1.
async fn one_node_burst_cluster(dir: &TempDir) -> Arc<ClusterManager> {
    let cm = test_cluster(dir).await;
    for n in ["n1", "n2"] {
        register_node(&cm, n, 64, 128000);
    }
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, 1);
    cm.qos_cache().insert(Qos {
        name: "burst".into(),
        limits: spur_core::accounting::QosLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
        ..Default::default()
    });
    cm
}

fn exclusive_burst_on(name: &str, node: &str) -> JobSpec {
    let mut spec = basic_spec(name);
    spec.qos = Some("burst".into());
    spec.exclusive = true;
    spec.nodelist = Some(node.into());
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_over_the_cap_after_this_passs_starts_reserves_no_future_slot() {
    // Two exclusive jobs share a QOS capped at one node. The first starts on n2; the
    // second waits for n1, busy with another job, where its slot would be a second node.
    let dir = TempDir::new().unwrap();
    let cm = one_node_burst_cluster(&dir).await;
    run_on(&cm, basic_spec("busy"), "n1");
    let first = submit_and_wait(&cm, exclusive_burst_on("first", "n2"));
    let second = submit_and_wait(&cm, exclusive_burst_on("second", "n1"));

    let pass = one_pass(&cm);
    assert_eq!(pass.started(), [first]);
    assert_eq!(
        pass.limits.refusals(),
        vec![(second, PendingReason::QosGrpNodeLimit, None)]
    );
    assert!(
        cm.with_planned_job_starts(|planned| !planned.contains_key(&second)),
        "a job over its limit publishes no start time"
    );
    assert_eq!(
        cm.get_job(second).unwrap().pending_reason,
        PendingReason::QosGrpNodeLimit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_behind_a_start_on_a_node_its_qos_uses_is_kept() {
    // As above, but both are pinned to n1: the second's slot follows the first
    // there, on the one node the QOS counts.
    let dir = TempDir::new().unwrap();
    let cm = one_node_burst_cluster(&dir).await;
    let first = submit_and_wait(&cm, exclusive_burst_on("first", "n1"));
    let second = submit_and_wait(&cm, exclusive_burst_on("second", "n1"));

    let pass = one_pass(&cm);
    assert_eq!(pass.started(), [first]);
    assert!(pass.limits.refusals().is_empty());
    assert!(cm.with_planned_job_starts(|planned| planned.contains_key(&second)));
}

/// One pass over an exclusive job and an exclusive burst-buffer job in a QOS
/// capped at `cap` nodes: the first starts, and the second, which fits the empty
/// running total, is up for stage-in. Returns the burst-buffer job.
async fn stage_in_after_a_start_under_a_grp_node_cap(cm: &ClusterManager, cap: u64) -> JobId {
    for n in ["n1", "n2"] {
        register_node(cm, n, 64, 128000);
    }
    *cm.burst_buffer_total_gb.write() = 100;
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, cap);
    cm.qos_cache().insert(Qos {
        name: "burst".into(),
        limits: spur_core::accounting::QosLimits {
            grp_tres: Some(grp),
            ..Default::default()
        },
        ..Default::default()
    });
    let spec = |name: &str| {
        let mut s = basic_spec(name);
        s.qos = Some("burst".into());
        s.exclusive = true;
        s
    };
    let first = submit_and_wait(cm, spec("first"));
    let staged = submit_and_wait(
        cm,
        JobSpec {
            burst_buffer: Some("capacity=40".into()),
            ..spec("staged")
        },
    );
    assert_eq!(one_pass(cm).started(), [first]);
    staged
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_buffer_job_over_the_cap_after_this_passs_starts_does_not_stage() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    let staged = stage_in_after_a_start_under_a_grp_node_cap(&cm, 1).await;

    let job = cm.get_job(staged).unwrap();
    assert_eq!(job.bb_stage_state, BbStageState::None);
    assert_eq!(job.pending_reason, PendingReason::QosGrpNodeLimit);
    assert_eq!(cm.available_bb(), 100, "it holds no burst-buffer capacity");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_buffer_job_within_the_cap_after_this_passs_starts_stages() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    let staged = stage_in_after_a_start_under_a_grp_node_cap(&cm, 2).await;

    let job = cm.get_job(staged).unwrap();
    assert_eq!(job.bb_stage_state, BbStageState::Staging);
    assert_eq!(job.pending_reason, PendingReason::BurstBufferStageIn);
    assert_eq!(cm.available_bb(), 60);
}

/// Classification gives the license to the second "pool" job, which is then refused at
/// start on a second node. The job waiting on the license runs the next pass, once
/// classification refuses the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_licensed_job_refused_at_start_holds_its_license_for_one_pass() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for n in ["n1", "n2", "n3"] {
        register_node(&cm, n, 8, 128000);
    }
    cm.qos_cache().insert(grp_node_capped("pool", 1));
    cm.license_pool.write().insert("fluent".into(), 1);
    let licensed = |mut spec: JobSpec| {
        spec.gres = vec!["license:fluent:1".into()];
        spec
    };
    let first = submit_and_wait(&cm, pool_job("first", 1, None));
    let refused = submit_and_wait(&cm, licensed(pool_job("refused", 1, None)));
    let waiting = submit_and_wait(&cm, licensed(basic_spec("waiting")));

    let pass = one_pass(&cm);
    assert_eq!(pass.admitted(), vec![first, refused]);
    assert_eq!(pass.started(), vec![first]);
    let reason = PendingReason::QosGrpNodeLimit;
    assert_eq!(
        pass.limits.refusals(),
        vec![(refused, reason.clone(), None)]
    );
    assert_eq!(
        cm.get_job(waiting).unwrap().pending_reason,
        PendingReason::Licenses
    );

    start_on(&cm, first, &pass.assignments[0].nodes[0]);
    assert_eq!(one_pass(&cm).started(), vec![waiting]);
    assert_eq!(cm.get_job(refused).unwrap().pending_reason, reason);
}

/// Submits `components` as one heterogeneous job, in group order. Each later
/// component links to the first by name and user.
fn submit_het(cm: &ClusterManager, components: Vec<JobSpec>) -> Vec<JobId> {
    let ids: Vec<JobId> = components
        .into_iter()
        .zip(0..)
        .map(|(mut spec, group)| {
            spec.het_group = Some(group);
            submit_and_wait(cm, spec)
        })
        .collect();
    for id in &ids[1..] {
        assert_eq!(cm.get_job(*id).unwrap().het_job_id, Some(ids[0]));
    }
    ids
}

/// Only the second component is in "pool", which is at its one-node cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_het_job_with_one_component_over_its_qos_limit_starts_no_component() {
    let dir = TempDir::new().unwrap();
    let cm = grp_node_cluster(&dir, 1).await;
    let het = submit_het(&cm, vec![basic_spec("het"), pool_job("het", 1, None)]);

    let pass = one_pass(&cm);
    assert!(pass.admitted().is_empty());
    assert!(pass.started().is_empty());
    for id in het {
        assert_eq!(
            cm.get_job(id).unwrap().pending_reason,
            PendingReason::QosGrpNodeLimit
        );
    }
}

/// Each component alone fits the one-node cap, but they are placed on two nodes.
/// The job behind them starts on a third, so the refused components left no
/// charge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_het_job_over_the_grp_node_cap_once_placed_is_refused_whole() {
    let dir = TempDir::new().unwrap();
    let cm = test_cluster(&dir).await;
    for n in ["n1", "n2", "n3"] {
        register_node(&cm, n, 8, 128000);
    }
    cm.qos_cache().insert(grp_node_capped("pool", 1));
    let het = submit_het(&cm, vec![pool_job("het", 1, Some("n1,n2")); 2]);
    let later = submit_and_wait(&cm, pool_job("later", 1, Some("n3")));

    let pass = one_pass(&cm);
    assert_eq!(pass.admitted(), vec![het[0], het[1], later]);
    assert_eq!(pass.started(), vec![later]);
    let reason = PendingReason::QosGrpNodeLimit;
    let refused: Vec<_> = het.iter().map(|id| (*id, reason.clone(), None)).collect();
    assert_eq!(pass.limits.refusals(), refused);
    for id in het {
        assert_eq!(cm.get_job(id).unwrap().pending_reason, reason);
    }
}

/// Beside the sibling with a free node, a second one is pinned behind a burst job it
/// may evict, so only the room held for the preemptor stops that eviction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preemptors_siblings_neither_start_nor_evict_while_its_victim_drains() {
    let dir = TempDir::new().unwrap();
    let (cm, victim, preemptor, _) = preemptor_with_draining_victim(&dir).await;
    register_node(&cm, "n4", 8, 128000);
    let bystander = run_burst_job(&cm, "n4");
    submit_and_wait(&cm, pool_job("evictor", 1, Some("n4")));

    let mut pass = one_pass(&cm);
    assert!(pass.started().is_empty());
    crate::scheduler_loop::preempt_unstarted(&cm, &mut pass).await;
    assert_eq!(cm.get_job(bystander).unwrap().state, JobState::Running);

    report_nodes_released(&cm, victim);
    settle(&cm, victim, JobState::Cancelled);
    cm.tick_preempt_freed_earmarks();
    assert_eq!(one_pass(&cm).started(), vec![preemptor]);
    assert_eq!(cm.get_job(bystander).unwrap().state, JobState::Running);
}

/// Two jobs with room for one, each pinned behind its own burst job. The pass
/// evicts for the first, and the room that eviction holds keeps the second from
/// being forwarded to a federated peer in the same pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_eviction_made_in_a_pass_keeps_its_sibling_from_being_forwarded_in_it() {
    let dir = TempDir::new().unwrap();
    let cm = pool_over_burst_cluster(&dir).await;
    let victims = [run_burst_job(&cm, "n2"), run_burst_job(&cm, "n3")];
    let first = submit_and_wait(&cm, pool_job("first", 1, Some("n2")));
    let second = submit_and_wait(&cm, pool_job("second", 1, Some("n3")));
    let forwarded = |pass: &crate::scheduler_loop::TestPass<'_>| -> Vec<JobId> {
        crate::scheduler_loop::federation_candidates(&pass.limits, &pass.unstarted())
            .iter()
            .map(|job| job.job_id)
            .collect()
    };

    let mut pass = one_pass(&cm);
    assert_eq!(
        forwarded(&pass),
        [first, second],
        "alone, each fits the room"
    );
    crate::scheduler_loop::preempt_unstarted(&cm, &mut pass).await;
    settle(&cm, victims[0], JobState::Completing);
    assert_eq!(cm.get_job(victims[0]).unwrap().preempted_by, Some(first));
    assert_eq!(forwarded(&pass), [first]);
}

/// Two shared jobs that each fill an 8-CPU node, with room under the cap for one
/// more node and a burst job on n3. The first starts on n2, so the second could
/// only pack onto the node the first just filled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_fits_only_on_a_node_a_start_just_filled_neither_evicts_nor_is_forwarded() {
    let dir = TempDir::new().unwrap();
    let cm = pool_over_burst_cluster(&dir).await;
    let mut burst = basic_spec("burst");
    burst.qos = Some("burst".into());
    burst.cpus_per_task = 8;
    let victim = run_on(&cm, burst, "n3");
    let first = submit_and_wait(&cm, shared_pool_job("first", 8));
    let second = submit_and_wait(&cm, shared_pool_job("second", 8));

    let mut pass = one_pass(&cm);
    assert_eq!(pass.started(), vec![first]);
    assert!(!pass.limits.fits(&cm.get_job(second).unwrap()));
    let forwarded = crate::scheduler_loop::federation_candidates(&pass.limits, &pass.unstarted());
    assert!(forwarded.iter().all(|job| job.job_id != second));
    crate::scheduler_loop::preempt_unstarted(&cm, &mut pass).await;
    assert_eq!(cm.get_job(victim).unwrap().state, JobState::Running);
}

/// Three idle 8-CPU nodes with idle-fill on and `pool` known.
async fn idle_fill_cluster(dir: &TempDir, pool: Qos) -> Arc<ClusterManager> {
    let mut config = test_config();
    config.scheduler.idle_fill_enabled = true;
    let cm = test_cluster_with_config(dir, config).await;
    for n in ["n1", "n2", "n3"] {
        register_node(&cm, n, 8, 128000);
    }
    cm.qos_cache().insert(pool);
    cm
}

/// A one-node "pool" job with the time limit a borrower must have.
fn borrowing_pool_job(name: &str) -> JobSpec {
    let mut spec = pool_job(name, 1, None);
    spec.time_limit = Some(chrono::Duration::hours(1));
    spec
}

fn in_research(mut spec: JobSpec) -> JobSpec {
    spec.account = Some("research".into());
    spec
}

/// A job of "pool" and "research" fills n1: "pool" is at its one-node cap and
/// "research" has one node left under its cap of two.
async fn borrowing_account_cluster(dir: &TempDir) -> Arc<ClusterManager> {
    let cm = idle_fill_cluster(dir, grp_node_capped("pool", 1)).await;
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, 2);
    let limits = spur_core::accounting::AccountLimits {
        grp_tres: Some(grp),
        ..Default::default()
    };
    cm.association_cache()
        .insert_limits("testuser", "research", limits);
    run_on(&cm, in_research(shared_pool_job("holder", 8)), "n1");
    cm
}

/// Each borrower fits the account's last node at classification; the first to
/// start takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_borrower_over_its_account_node_cap_is_refused_at_start() {
    let dir = TempDir::new().unwrap();
    let cm = borrowing_account_cluster(&dir).await;
    let first = submit_and_wait(&cm, in_research(borrowing_pool_job("first")));
    let second = submit_and_wait(&cm, in_research(borrowing_pool_job("second")));

    let pass = one_pass(&cm);
    assert_eq!(pass.borrowed(), vec![first, second]);
    assert_eq!(pass.started(), vec![first]);
    let reason = PendingReason::AssocGrpNodeLimit;
    assert_eq!(pass.limits.refusals(), vec![(second, reason.clone(), None)]);
    assert_eq!(cm.get_job(second).unwrap().pending_reason, reason);
}

/// The borrower outranks the in-quota job, but borrowers are placed after every
/// in-quota job, so the account's last node goes to the in-quota one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_borrower_cannot_take_account_room_from_an_in_quota_job_in_the_same_pass() {
    let dir = TempDir::new().unwrap();
    let cm = borrowing_account_cluster(&dir).await;
    let mut borrower = in_research(borrowing_pool_job("borrower"));
    borrower.priority = Some(1000);
    let borrower = submit_and_wait(&cm, borrower);
    let mut in_quota = in_research(basic_spec("in-quota"));
    in_quota.exclusive = true;
    in_quota.priority = Some(1);
    let in_quota = submit_and_wait(&cm, in_quota);

    let pass = one_pass(&cm);
    assert_eq!(pass.admitted(), vec![in_quota]);
    assert_eq!(pass.borrowed(), vec![borrower]);
    assert_eq!(pass.started(), vec![in_quota]);
    assert_eq!(
        pass.limits.refusals(),
        vec![(borrower, PendingReason::AssocGrpNodeLimit, None)]
    );
}

/// "research", capped at `account_cap` nodes, runs on n1 and starts a job on n3. Only
/// its limits decide whether reclaim frees n2, held by a preemptable "burst" run, for
/// its borrower pinned there. Returns the run and the nodes reclaim freed.
async fn reclaim_for_a_borrower_beside_a_start(
    dir: &TempDir,
    account_cap: u64,
) -> (Arc<ClusterManager>, JobId, Vec<String>) {
    let mut pool = grp_node_capped("pool", 1);
    pool.priority = 10;
    let cm = idle_fill_cluster(dir, pool).await;
    let mut grp = TresRecord::new();
    grp.set(TresType::Node, account_cap);
    let limits = spur_core::accounting::AccountLimits {
        grp_tres: Some(grp),
        ..Default::default()
    };
    cm.association_cache()
        .insert_limits("testuser", "research", limits);
    cm.qos_cache().insert(Qos {
        name: "burst".into(),
        priority: 1,
        idle_fill_preemptable: true,
        ..Default::default()
    });
    run_on(&cm, in_research(shared_pool_job("holder", 8)), "n1");
    let run = run_burst_job(&cm, "n2");
    let mut in_quota = in_research(basic_spec("in-quota"));
    in_quota.exclusive = true;
    in_quota.nodelist = Some("n3".into());
    let in_quota = submit_and_wait(&cm, in_quota);
    let mut borrower = in_research(borrowing_pool_job("borrower"));
    borrower.nodelist = Some("n2".into());
    let borrower = submit_and_wait(&cm, borrower);

    let pass = one_pass(&cm);
    assert_eq!(pass.borrowed(), vec![borrower]);
    assert_eq!(pass.started(), vec![in_quota]);
    assert!(!pass.limits.refused(borrower));
    let freed = crate::scheduler_loop::reclaim_unstarted(&cm, &pass).await;
    (cm, run, freed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaim_frees_no_node_for_a_borrower_over_its_account_cap_after_this_passs_starts() {
    let dir = TempDir::new().unwrap();
    let (cm, run, freed) = reclaim_for_a_borrower_beside_a_start(&dir, 2).await;
    assert!(freed.is_empty());
    assert_eq!(cm.get_job(run).unwrap().state, JobState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaim_frees_a_node_for_a_borrower_within_its_account_cap_after_this_passs_starts() {
    let dir = TempDir::new().unwrap();
    let (cm, run, freed) = reclaim_for_a_borrower_beside_a_start(&dir, 3).await;
    assert_eq!(freed, ["n2"]);
    settle(&cm, run, JobState::Pending);
}
