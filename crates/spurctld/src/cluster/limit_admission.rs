// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! QOS, account and partition QOS limits charged when a job starts, on the nodes it got.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use spur_core::accounting::{Qos, TresRecord};
use spur_core::job::{Job, JobId, PendingReason};
use spur_core::partition::Partition;
use spur_sched::traits::{JobStart, StartAdmission};

use super::{
    account_of, job_tres, quota_gate, quota_probes, CapUsage, ClusterManager, EvictionHolds,
    PartitionQosView, Placement, QuotaLedger, QuotaVerdict, ReasonUpdate, ReuseView,
};
use crate::limits_cache::{consumed_minutes, GrpWallUsage};

/// What a pass's starts are checked against, as classification read it.
pub(super) struct PassUsage {
    pub(super) ledger: QuotaLedger,
    /// Each classified job's QOS, resolved once for the pass.
    pub(super) qos: HashMap<JobId, Qos>,
    pub(super) partition_qos: PartitionQosView,
    pub(super) partitions: Vec<Partition>,
    pub(super) cap_usage: CapUsage,
    pub(super) grp_wall: GrpWallUsage,
}

/// Room held for a preemptor whose victims are still stopping, so a sibling
/// can't take what the eviction frees before the preemptor starts.
#[derive(Clone)]
pub(super) struct Eviction {
    job_id: JobId,
    pub(super) user: String,
    pub(super) qos: Option<String>,
    pub(super) account: Option<String>,
    /// The occupied nodes the preemptor was hinted to pack onto.
    pub(super) hinted: Vec<String>,
    pub(super) tres: TresRecord,
    // New nodes it holds against the group node caps of its QOS, its account and
    // each partition QOS it may start under (by name).
    pub(super) qos_nodes: u64,
    pub(super) account_nodes: u64,
    pub(super) partition_qos_nodes: HashMap<String, u64>,
}

impl Eviction {
    pub(super) fn new(
        job: &Job,
        tres: TresRecord,
        qos_nodes: u64,
        account_nodes: u64,
        partition_qos_nodes: HashMap<String, u64>,
    ) -> Self {
        Self {
            job_id: job.job_id,
            user: job.spec.user.clone(),
            qos: job.spec.qos.clone().filter(|qos| !qos.is_empty()),
            account: account_of(job).map(str::to_owned),
            hinted: job.preferred_nodes.iter().cloned().collect(),
            tres,
            qos_nodes,
            account_nodes,
            partition_qos_nodes,
        }
    }
}

/// A scheduling pass's QOS, account and partition QOS limits, charged as each job
/// starts, on the nodes it got, in placement order. A waiting job holds nothing,
/// except a preemptor whose eviction is in flight.
pub(crate) struct LimitAdmission<'a> {
    cluster: &'a ClusterManager,
    usage: PassUsage,
    evictions: Vec<Eviction>,
    /// The room all of `evictions` hold.
    held: EvictionHolds,
    /// The nodes this pass's starts landed on.
    filled: HashSet<String>,
    refused: HashMap<JobId, PendingReason>,
}

impl<'a> LimitAdmission<'a> {
    pub(super) fn new(cluster: &'a ClusterManager, usage: PassUsage) -> Self {
        Self {
            cluster,
            usage,
            evictions: Vec::new(),
            held: EvictionHolds::default(),
            filled: HashSet::new(),
            refused: HashMap::new(),
        }
    }

    /// Whether `job` could start now, after this pass's starts and the evictions in flight,
    /// in any request its partitions give it. Reads the live nodes it could pack onto, so
    /// callers must not hold the nodes lock.
    pub(crate) fn fits(&self, job: &Job) -> bool {
        !self.fitting([job]).is_empty()
    }

    /// The `jobs` that [fit](Self::fits), read against one view of the nodes.
    pub(crate) fn fitting<'j>(&self, jobs: impl IntoIterator<Item = &'j Job>) -> Vec<&'j Job> {
        self.on_live_nodes(|reuse| {
            jobs.into_iter()
                .filter(|job| !self.refused(job.job_id) && self.fit(job, reuse).is_ok())
                .collect()
        })
    }

    fn on_live_nodes<R>(&self, read: impl FnOnce(&ReuseView<'_>) -> R) -> R {
        let nodes = self.cluster.nodes.read();
        read(&ReuseView {
            nodes: &nodes,
            partitions: &self.usage.partitions,
            cap_usage: &self.usage.cap_usage,
            filled: &self.filled,
        })
    }

    /// Whether `job` fits in any request its partitions give it, or else the limit
    /// its first request is over.
    fn fit(&self, job: &Job, reuse: &ReuseView<'_>) -> Result<(), PendingReason> {
        let mut over = None;
        for probe in quota_probes(job, &self.usage.partitions) {
            match self.check(&probe, Placement::Packing(reuse)) {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    over.get_or_insert(reason);
                }
            }
        }
        over.map_or(Ok(()), Err)
    }

    /// Holds the room an eviction frees for its preemptor until the preemptor starts.
    pub(super) fn hold(&mut self, eviction: Eviction) {
        match self
            .evictions
            .iter_mut()
            .find(|e| e.job_id == eviction.job_id)
        {
            Some(held) => {
                *held = eviction;
                self.held = held_by(&self.evictions, None);
            }
            None => {
                self.held.hold(&eviction);
                self.evictions.push(eviction);
            }
        }
    }

    /// Holds the room `job` evicts for, at its full node count in every scope since no
    /// node is known yet. In the evicting pass only federation reads it; from the next
    /// pass classification holds it.
    pub(crate) fn record_eviction(&mut self, job: &Job) {
        let nodes = u64::from(job.spec.num_nodes);
        let partition_qos_nodes = self
            .usage
            .partition_qos
            .qos_names(job)
            .into_iter()
            .map(|qos| (qos, nodes))
            .collect();
        self.hold(Eviction::new(
            job,
            job_tres(job),
            nodes,
            nodes,
            partition_qos_nodes,
        ));
    }

    /// Whether `job`, which can't start now, may stage in its burst buffer ahead of
    /// its start. Not when it [doesn't fit](Self::fits); it is refused with the limit
    /// it is over.
    pub(crate) fn may_stage_in(&mut self, job: &Job) -> bool {
        let fit = self.on_live_nodes(|reuse| self.fit(job, reuse));
        self.refuse_unless(job.job_id, fit)
    }

    fn refuse_unless(&mut self, id: JobId, fit: Result<(), PendingReason>) -> bool {
        match fit {
            Ok(()) => true,
            Err(reason) => {
                self.refused.insert(id, reason);
                false
            }
        }
    }

    pub(crate) fn refused(&self, id: JobId) -> bool {
        self.refused.contains_key(&id)
    }

    /// Each job refused a start, a future slot or a stage-in, with the limit it was
    /// over as its pending reason.
    pub(crate) fn refusals(&self) -> Vec<ReasonUpdate> {
        let mut refusals: Vec<_> = self
            .refused
            .iter()
            .map(|(id, reason)| (*id, reason.clone(), None))
            .collect();
        refusals.sort_unstable_by_key(|(id, _, _)| *id);
        refusals
    }

    fn check(&self, job: &Job, placement: Placement<'_>) -> Result<(), PendingReason> {
        let qos = match self.usage.qos.get(&job.job_id) {
            Some(qos) => Cow::Borrowed(qos),
            None => Cow::Owned(self.cluster.resolve_qos(job)),
        };
        let consumed_wall = job
            .spec
            .qos
            .as_deref()
            .and_then(|name| consumed_minutes(&self.usage.grp_wall, name));
        let packing = matches!(placement, Placement::Packing(_));
        match quota_gate(
            job,
            &qos,
            &self.usage.partition_qos,
            &self.cluster.association_cache,
            &self.usage.ledger,
            placement,
            &self.held_for(job.job_id),
            consumed_wall,
        ) {
            // A borrower gets no packing hint, so, as in classification, it fits only
            // if its account and partition QOS admit it without packing credit.
            QuotaVerdict::Admitted { account, .. } | QuotaVerdict::QosBlocked { account, .. }
                if job.idle_fill && packing && !account.admits_without_credit =>
            {
                Err(PendingReason::AssocGrpNodeLimit)
            }
            QuotaVerdict::Admitted { partition_qos, .. }
            | QuotaVerdict::QosBlocked { partition_qos, .. }
                if job.idle_fill && packing && !partition_qos.admits_without_credit() =>
            {
                Err(PendingReason::QosGrpNodeLimit)
            }
            QuotaVerdict::Admitted { .. } => Ok(()),
            QuotaVerdict::AccountBlocked(reason) | QuotaVerdict::PartitionQosBlocked(reason) => {
                Err(reason)
            }
            // A borrower may exceed its QOS node cap, but every other QOS limit
            // still bounds it.
            QuotaVerdict::QosBlocked { blocked, .. }
                if job.idle_fill && blocked.grp_node_sole_blocker =>
            {
                Ok(())
            }
            QuotaVerdict::QosBlocked { blocked, .. } => Err(blocked.reason),
        }
    }

    /// The room held by evictions in flight, less `id`'s own, which it is about to use.
    fn held_for(&self, id: JobId) -> Cow<'_, EvictionHolds> {
        if self.evictions.iter().any(|e| e.job_id == id) {
            Cow::Owned(held_by(&self.evictions, Some(id)))
        } else {
            Cow::Borrowed(&self.held)
        }
    }

    /// The partition `start` is charged in: the one backfill placed it in, else the
    /// one [`charged_partition`] picks.
    ///
    /// [`charged_partition`]: crate::scheduler_loop::charged_partition
    fn charged_partition<'s>(&'s self, start: &JobStart<'s>) -> Option<&'s str> {
        start.partition.or_else(|| {
            crate::scheduler_loop::charged_partition(start.job, &self.usage.partitions)
                .map(|p| p.name.as_str())
        })
    }

    fn release(&mut self, id: JobId) {
        let held = self.evictions.len();
        self.evictions.retain(|e| e.job_id != id);
        if self.evictions.len() != held {
            self.held = held_by(&self.evictions, None);
        }
    }
}

fn held_by(evictions: &[Eviction], except: Option<JobId>) -> EvictionHolds {
    let mut held = EvictionHolds::default();
    for e in evictions.iter().filter(|e| Some(e.job_id) != except) {
        held.hold(e);
    }
    held
}

/// `start.job` with its partition's memory reshape applied, as dispatch runs it.
fn as_started<'j>(start: &JobStart<'j>) -> Cow<'j, Job> {
    match start.memory_adjustment {
        Some(adjustment) => {
            let mut job = start.job.clone();
            adjustment.apply_to(&mut job.spec);
            Cow::Owned(job)
        }
        None => Cow::Borrowed(start.job),
    }
}

impl StartAdmission for LimitAdmission<'_> {
    /// Each start is checked on its nodes, in the request it runs with, after the
    /// ones charged before it, so a heterogeneous job's components count against
    /// each other.
    fn admit(&mut self, starts: &[JobStart<'_>]) -> bool {
        // Each start's partition, and the partition QOS that charges it there.
        let charged: Vec<(Option<String>, Option<String>)> = starts
            .iter()
            .map(|start| {
                let partition = self.charged_partition(start);
                let qos = partition.and_then(|p| self.usage.partition_qos.qos_of(p));
                (partition.map(str::to_owned), qos.map(str::to_owned))
            })
            .collect();
        // A batch that fails part way must leave no partial charge behind.
        let saved = (starts.len() > 1).then(|| {
            let jobs = starts
                .iter()
                .zip(&charged)
                .map(|(start, (_, qos))| (start.job, qos.as_deref()));
            (self.usage.ledger.snapshot(jobs), self.evictions.clone())
        });
        for (start, (partition, partition_qos)) in starts.iter().zip(&charged) {
            let job = as_started(start);
            let placement = Placement::On {
                nodes: start.nodes,
                partition: partition.as_deref(),
            };
            if let Err(reason) = self.check(&job, placement) {
                if let Some((ledger, evictions)) = saved {
                    self.usage.ledger.restore(ledger);
                    self.evictions = evictions;
                    self.held = held_by(&self.evictions, None);
                }
                for refused in starts {
                    self.refused.insert(refused.job.job_id, reason.clone());
                }
                return false;
            }
            self.usage
                .ledger
                .charge_start(&job, start.nodes, partition_qos.as_deref());
            self.release(start.job.job_id);
        }
        for start in starts {
            self.filled.extend(start.nodes.iter().cloned());
        }
        true
    }

    /// A job over its limits on the slot's nodes after this pass's starts would hold
    /// the slot, and the start time it publishes, without being able to start in it.
    fn may_reserve(&mut self, slot: &JobStart<'_>) -> bool {
        let placement = Placement::On {
            nodes: slot.nodes,
            partition: self.charged_partition(slot),
        };
        let fit = self.check(&as_started(slot), placement);
        self.refuse_unless(slot.job.job_id, fit)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use spur_core::accounting::{AccountLimits, QosLimits, TresType};
    use spur_core::job::{JobSpec, JobState};
    use spur_core::node::{Node, NodeState};
    use spur_core::partition_limits::MemoryAdjustment;
    use spur_core::resource::ResourceSet;
    use tempfile::TempDir;

    use super::*;

    fn grp(limits: &[(TresType, u64)]) -> TresRecord {
        let mut grp = TresRecord::new();
        for (kind, cap) in limits {
            grp.set(*kind, *cap);
        }
        grp
    }

    /// Three idle nodes; `qos_cap` and `account_cap` are the group limits on the
    /// "pool" QOS and on alice's "research" association.
    fn cluster_capped(
        dir: &TempDir,
        qos_cap: &[(TresType, u64)],
        account_cap: &[(TresType, u64)],
    ) -> ClusterManager {
        let config = crate::server::test_support::test_slurm_config();
        let cm = ClusterManager::new(config, dir.path()).expect("cluster manager");
        for name in ["n1", "n2", "n3"] {
            let resources = ResourceSet {
                cpus: 8,
                ..Default::default()
            };
            let mut node = Node::new(name.into(), resources);
            node.state = NodeState::Idle;
            cm.nodes.write().insert(name.into(), node);
        }
        cm.qos_cache().insert(Qos {
            name: "pool".into(),
            limits: QosLimits {
                grp_tres: (!qos_cap.is_empty()).then(|| grp(qos_cap)),
                ..Default::default()
            },
            ..Default::default()
        });
        let limits = AccountLimits {
            grp_tres: (!account_cap.is_empty()).then(|| grp(account_cap)),
            ..Default::default()
        };
        cm.association_cache()
            .insert_limits("alice", "research", limits);
        cm
    }

    fn cluster(
        dir: &TempDir,
        qos_nodes: Option<u64>,
        account_nodes: Option<u64>,
    ) -> ClusterManager {
        let nodes = |cap: Option<u64>| cap.map(|c| vec![(TresType::Node, c)]).unwrap_or_default();
        cluster_capped(dir, &nodes(qos_nodes), &nodes(account_nodes))
    }

    /// A pending job of alice's in "pool" and "research".
    fn job(id: JobId, num_nodes: u32) -> Job {
        Job::new(
            id,
            JobSpec {
                name: format!("j{id}"),
                user: "alice".into(),
                qos: Some("pool".into()),
                account: Some("research".into()),
                num_nodes,
                num_tasks: num_nodes,
                cpus_per_task: 1,
                work_dir: "/tmp".into(),
                ..Default::default()
            },
        )
    }

    fn running(id: JobId, nodes: &[&str]) -> Job {
        let mut job = job(id, nodes.len() as u32);
        job.state = JobState::Running;
        job.allocated_nodes = names(nodes);
        job
    }

    fn names(nodes: &[&str]) -> Vec<String> {
        nodes.iter().map(|n| (*n).to_string()).collect()
    }

    fn on(nodes: &[String]) -> Placement<'_> {
        Placement::On {
            nodes,
            partition: None,
        }
    }

    fn start<'a>(job: &'a Job, nodes: &'a [String]) -> JobStart<'a> {
        let now = Utc::now();
        JobStart {
            job,
            nodes,
            partition: None,
            memory_adjustment: None,
            start: now,
            end: now,
        }
    }

    /// Admission over usage summed from `jobs`, as classification reads it.
    fn admission<'a>(cm: &'a ClusterManager, jobs: &[&Job]) -> LimitAdmission<'a> {
        admission_in(cm, jobs, Vec::new())
    }

    /// [`admission`] in `partitions`, under their partition QOSes.
    fn admission_in<'a>(
        cm: &'a ClusterManager,
        jobs: &[&Job],
        partitions: Vec<Partition>,
    ) -> LimitAdmission<'a> {
        let partition_qos = PartitionQosView::build(&partitions, cm.qos_cache(), true);
        let mut ledger = QuotaLedger::from_jobs(jobs.iter().copied());
        ledger.add_partition_qos(jobs.iter().copied(), |job| {
            partition_qos.charged_qos(job, &partitions)
        });
        let usage = PassUsage {
            ledger,
            qos: jobs.iter().map(|j| (j.job_id, cm.resolve_qos(j))).collect(),
            partition_qos,
            partitions,
            cap_usage: CapUsage::new(),
            grp_wall: None,
        };
        LimitAdmission::new(cm, usage)
    }

    /// The scope a test puts its group node cap of 1 on.
    #[derive(Clone, Copy)]
    enum Capped {
        Qos,
        Account,
    }

    impl Capped {
        fn cluster(self, dir: &TempDir) -> ClusterManager {
            match self {
                Self::Qos => cluster(dir, Some(1), None),
                Self::Account => cluster(dir, None, Some(1)),
            }
        }

        fn reason(self) -> PendingReason {
            match self {
                Self::Qos => PendingReason::QosGrpNodeLimit,
                Self::Account => PendingReason::AssocGrpNodeLimit,
            }
        }
    }

    fn assert_a_second_fresh_node_is_refused(capped: Capped) {
        let dir = TempDir::new().expect("tempdir");
        let cm = capped.cluster(&dir);
        let (first, second) = (job(1, 1), job(2, 1));
        let mut limits = admission(&cm, &[&first, &second]);

        assert!(limits.admit(&[start(&first, &names(&["n1"]))]));
        assert!(!limits.admit(&[start(&second, &names(&["n2"]))]));
        assert_eq!(limits.refusals(), vec![(2, capped.reason(), None)]);
        assert!(!limits.refused(1));
    }

    fn assert_a_second_job_on_the_same_node_is_admitted(capped: Capped) {
        let dir = TempDir::new().expect("tempdir");
        let cm = capped.cluster(&dir);
        let (first, second) = (job(1, 1), job(2, 1));
        let mut limits = admission(&cm, &[&first, &second]);

        assert!(limits.admit(&[start(&first, &names(&["n1"]))]));
        assert!(limits.admit(&[start(&second, &names(&["n1"]))]));
        assert!(limits.refusals().is_empty());
    }

    #[test]
    fn a_start_on_a_fresh_node_over_the_qos_node_cap_is_refused() {
        assert_a_second_fresh_node_is_refused(Capped::Qos);
    }

    #[test]
    fn a_start_on_a_node_its_qos_already_uses_is_admitted() {
        assert_a_second_job_on_the_same_node_is_admitted(Capped::Qos);
    }

    #[test]
    fn a_start_on_a_fresh_node_over_the_account_node_cap_is_refused() {
        assert_a_second_fresh_node_is_refused(Capped::Account);
    }

    #[test]
    fn a_start_on_a_node_its_account_already_uses_is_admitted() {
        assert_a_second_job_on_the_same_node_is_admitted(Capped::Account);
    }

    /// `start` with its partition's memory policy reshaping it to `cpus_per_task`.
    fn start_reshaped<'a>(job: &'a Job, nodes: &'a [String], cpus_per_task: u32) -> JobStart<'a> {
        JobStart {
            memory_adjustment: Some(MemoryAdjustment {
                cpus_per_task,
                memory_per_node_mb: None,
                memory_per_cpu_mb: None,
                memory_per_gpu_mb: None,
            }),
            ..start(job, nodes)
        }
    }

    #[test]
    fn a_start_is_checked_and_charged_in_the_request_its_partition_reshaped() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster_capped(&dir, &[(TresType::Cpu, 2)], &[]);
        let (oversized, reshaped, next) = (job(1, 1), job(2, 1), job(3, 1));
        let mut limits = admission(&cm, &[&oversized, &reshaped, &next]);

        let n1 = names(&["n1"]);
        assert!(
            !limits.admit(&[start_reshaped(&oversized, &n1, 3)]),
            "3 CPUs once reshaped, over the cap of 2"
        );
        assert!(limits.admit(&[start_reshaped(&reshaped, &n1, 2)]));
        assert!(
            !limits.admit(&[start(&next, &n1)]),
            "the start before counts its 2 reshaped CPUs, not the 1 it asked for"
        );
        let reason = PendingReason::QosGrpCpuLimit;
        assert_eq!(
            limits.refusals(),
            vec![(1, reason.clone(), None), (3, reason, None)]
        );
    }

    /// The two components of heterogeneous job 10.
    fn het_pair() -> (Job, Job) {
        let (mut leader, mut member) = (job(10, 1), job(11, 1));
        for (component, group) in [(&mut leader, 0), (&mut member, 1)] {
            component.het_job_id = Some(10);
            component.het_group = Some(group);
        }
        (leader, member)
    }

    #[test]
    fn a_heterogeneous_batch_over_the_cap_is_refused_whole_and_charges_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let (leader, member) = het_pair();
        let later = job(12, 1);
        let mut limits = admission(&cm, &[&leader, &member, &later]);

        let (n1, n2) = (names(&["n1"]), names(&["n2"]));
        assert!(!limits.admit(&[start(&leader, &n1), start(&member, &n2)]));
        let reason = PendingReason::QosGrpNodeLimit;
        assert_eq!(
            limits.refusals(),
            vec![(10, reason.clone(), None), (11, reason, None)]
        );

        // Had the leader's charge on n1 stayed, n3 would be a second node.
        assert!(limits.admit(&[start(&later, &names(&["n3"]))]));
    }

    #[test]
    fn a_borrower_skips_its_qos_node_cap_but_not_its_account_cap() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, Some(1), Some(2));
        let holder = running(1, &["n1"]);
        let (mut within, mut over) = (job(2, 1), job(3, 1));
        within.idle_fill = true;
        over.idle_fill = true;
        let in_quota = job(4, 1);
        let mut limits = admission(&cm, &[&holder, &within, &over, &in_quota]);

        let n2 = names(&["n2"]);
        assert_eq!(
            limits.check(&in_quota, on(&n2)),
            Err(PendingReason::QosGrpNodeLimit),
            "n2 is over the QOS node cap for a job that isn't borrowing"
        );
        assert!(limits.admit(&[start(&within, &n2)]));
        assert!(!limits.admit(&[start(&over, &names(&["n3"]))]));
        assert_eq!(
            limits.refusals(),
            vec![(3, PendingReason::AssocGrpNodeLimit, None)]
        );
    }

    #[test]
    fn a_borrower_is_refused_once_this_pass_fills_another_qos_limit() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster_capped(&dir, &[(TresType::Node, 1), (TresType::Cpu, 2)], &[]);
        let holder = running(1, &["n1"]);
        let mut borrower = job(2, 1);
        borrower.idle_fill = true;
        let in_quota = job(3, 1);
        let mut limits = admission(&cm, &[&holder, &borrower, &in_quota]);

        let n2 = names(&["n2"]);
        assert_eq!(
            limits.check(&borrower, on(&n2)),
            Ok(()),
            "only the node cap is in its way before the in-quota start"
        );
        assert!(limits.admit(&[start(&in_quota, &names(&["n1"]))]));
        assert!(!limits.admit(&[start(&borrower, &n2)]));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosGrpCpuLimit, None)]
        );
    }

    #[test]
    fn an_eviction_in_flight_holds_its_room_for_its_own_preemptor() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let (preemptor, sibling, packer) = (job(1, 1), job(2, 1), job(3, 1));
        let mut limits = admission(&cm, &[&preemptor, &sibling, &packer]);
        limits.hold(Eviction::new(
            &preemptor,
            job_tres(&preemptor),
            1,
            1,
            HashMap::new(),
        ));

        let n1 = names(&["n1"]);
        assert!(!limits.admit(&[start(&sibling, &names(&["n2"]))]));
        assert!(limits.admit(&[start(&preemptor, &n1)]));
        assert!(limits.evictions.is_empty());
        // Still held, the eviction would count on top of the node it now runs on.
        assert!(limits.admit(&[start(&packer, &n1)]));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosGrpNodeLimit, None)]
        );
    }

    #[test]
    fn a_refused_heterogeneous_batch_keeps_the_eviction_it_would_have_used() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let (leader, member) = het_pair();
        let sibling = job(12, 1);
        let mut limits = admission(&cm, &[&leader, &member, &sibling]);
        limits.hold(Eviction::new(
            &leader,
            job_tres(&leader),
            1,
            1,
            HashMap::new(),
        ));

        let (n1, n2) = (names(&["n1"]), names(&["n2"]));
        assert!(!limits.admit(&[start(&leader, &n1), start(&member, &n2)]));
        assert_eq!(
            limits.evictions.len(),
            1,
            "the leader still waits on its victims"
        );
        assert!(!limits.admit(&[start(&sibling, &n2)]));
    }

    #[test]
    fn fits_is_false_for_a_job_refused_this_pass() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let (leader, member) = het_pair();
        let twin = job(12, 1);
        let mut limits = admission(&cm, &[&leader, &member, &twin]);

        let (n1, n2) = (names(&["n1"]), names(&["n2"]));
        assert!(!limits.admit(&[start(&leader, &n1), start(&member, &n2)]));
        assert!(limits.fits(&twin), "one node alone is within the cap");
        assert!(!limits.fits(&leader));
        assert!(!limits.fits(&member));
    }

    #[test]
    fn a_recorded_eviction_keeps_a_sibling_from_fitting() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let (preemptor, sibling) = (job(1, 1), job(2, 1));
        let mut limits = admission(&cm, &[&preemptor, &sibling]);

        assert!(limits.fits(&sibling));
        limits.record_eviction(&preemptor);
        assert!(!limits.fits(&sibling));
        assert!(limits.fits(&preemptor), "its own eviction is not counted");
    }

    #[test]
    fn fits_credits_packing_onto_a_node_the_scope_already_occupies() {
        let dir = TempDir::new().expect("tempdir");
        let cm = Capped::Qos.cluster(&dir);
        let holder = running(1, &["n1"]);
        let packer = job(2, 1);
        let limits = admission(&cm, &[&holder, &packer]);

        assert!(limits.fits(&packer), "n1 has room for it at no new node");
        cm.nodes.write().remove("n1");
        assert!(!limits.fits(&packer), "without n1 it needs a second node");
    }

    #[test]
    fn a_borrower_gets_no_packing_credit_from_its_account() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, Some(1), Some(1));
        let holder = running(1, &["n1"]);
        let mut borrower = job(2, 1);
        borrower.idle_fill = true;
        let in_quota = job(3, 1);
        let limits = admission(&cm, &[&holder, &borrower, &in_quota]);

        assert!(limits.fits(&in_quota), "it may pack onto n1");
        assert!(
            !limits.fits(&borrower),
            "it would need a second node for its account"
        );
    }

    /// "pool" holds its one node, n1, which is gone, so a borrower runs outside the
    /// QOS cap. Its account fits it only by packing onto n2, which a borrower can't.
    #[test]
    fn a_borrower_outside_its_qos_cap_gets_no_packing_credit_from_its_account() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, Some(1), Some(2));
        let in_pool = running(1, &["n1"]);
        let mut outside_pool = running(2, &["n2"]);
        outside_pool.spec.qos = None;
        let mut borrower = job(3, 1);
        borrower.idle_fill = true;
        let limits = admission(&cm, &[&in_pool, &outside_pool, &borrower]);
        cm.nodes.write().remove("n1");

        assert!(!limits.fits(&borrower));
        let limits_with_room = AccountLimits {
            grp_tres: Some(grp(&[(TresType::Node, 3)])),
            ..Default::default()
        };
        cm.association_cache()
            .insert_limits("alice", "research", limits_with_room);
        assert!(
            limits.fits(&borrower),
            "a third node needs no packing credit"
        );
    }

    /// "gpu" carries the partition QOS "usercap" with `limits`, "cpu" none. Both hold
    /// every node.
    fn gpu_under_usercap(cm: &ClusterManager, limits: QosLimits) -> Vec<Partition> {
        cm.qos_cache().insert(Qos {
            name: "usercap".into(),
            limits,
            ..Default::default()
        });
        for node in cm.nodes.write().values_mut() {
            node.partitions = names(&["gpu", "cpu"]);
        }
        ["gpu", "cpu"]
            .into_iter()
            .map(|name| Partition {
                name: name.into(),
                nodes: "ALL".into(),
                qos: (name == "gpu").then(|| "usercap".to_string()),
                ..Default::default()
            })
            .collect()
    }

    fn one_node_per_user() -> QosLimits {
        QosLimits {
            max_tres_per_user: Some(grp(&[(TresType::Node, 1)])),
            ..Default::default()
        }
    }

    fn in_gpu(mut job: Job) -> Job {
        job.spec.partition = Some("gpu".into());
        job
    }

    fn start_in<'a>(job: &'a Job, nodes: &'a [String], partition: &'a str) -> JobStart<'a> {
        JobStart {
            partition: Some(partition),
            ..start(job, nodes)
        }
    }

    #[test]
    fn a_start_is_checked_and_charged_in_the_partition_it_starts_in() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let mut either = job(1, 1);
        either.spec.partition = Some("gpu,cpu".into());
        let in_gpu = in_gpu(job(2, 1));
        let mut later = job(3, 1);
        later.spec.partition = Some("gpu,cpu".into());
        let mut limits = admission_in(&cm, &[&either, &in_gpu, &later], partitions);

        assert!(limits.admit(&[start_in(&either, &names(&["n1"]), "cpu")]));
        assert!(limits.admit(&[start_in(&in_gpu, &names(&["n2"]), "gpu")]));
        assert!(
            !limits.admit(&[start_in(&later, &names(&["n3"]), "gpu")]),
            "cpu would admit it, but it starts in gpu"
        );
        assert_eq!(
            limits.refusals(),
            vec![(3, PendingReason::QosMaxNodePerUserLimit, None)]
        );
    }

    /// A start backfill placed in none of its partitions is charged where its run is:
    /// in its highest-tier partition.
    #[test]
    fn a_start_placed_in_no_partition_is_charged_where_its_run_is() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let (unplaced, next) = (in_gpu(job(1, 1)), in_gpu(job(2, 1)));
        let mut limits = admission_in(&cm, &[&unplaced, &next], partitions);

        assert!(limits.admit(&[start(&unplaced, &names(&["n1"]))]));
        assert!(!limits.admit(&[start_in(&next, &names(&["n2"]), "gpu")]));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosMaxNodePerUserLimit, None)]
        );
    }

    #[test]
    fn an_eviction_in_flight_holds_its_partition_qos_room() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let (preemptor, sibling) = (in_gpu(job(1, 1)), in_gpu(job(2, 1)));
        let mut limits = admission_in(&cm, &[&preemptor, &sibling], partitions);
        let held = HashMap::from([("usercap".to_string(), 1)]);
        limits.hold(Eviction::new(&preemptor, job_tres(&preemptor), 0, 0, held));

        assert!(!limits.admit(&[start_in(&sibling, &names(&["n2"]), "gpu")]));
        assert!(limits.admit(&[start_in(&preemptor, &names(&["n1"]), "gpu")]));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosMaxNodePerUserLimit, None)]
        );
    }

    #[test]
    fn a_recorded_eviction_keeps_a_sibling_under_its_partition_qos_from_fitting() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let (preemptor, sibling) = (in_gpu(job(1, 1)), in_gpu(job(2, 1)));
        let mut limits = admission_in(&cm, &[&preemptor, &sibling], partitions);

        assert!(limits.fits(&sibling));
        limits.record_eviction(&preemptor);
        assert!(!limits.fits(&sibling));
        assert!(limits.fits(&preemptor), "its own eviction is not counted");
    }

    #[test]
    fn a_heterogeneous_batch_over_its_partition_qos_is_refused_whole_and_charges_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let (leader, member) = het_pair();
        let (leader, member, later) = (in_gpu(leader), in_gpu(member), in_gpu(job(12, 1)));
        let mut limits = admission_in(&cm, &[&leader, &member, &later], partitions);

        let (n1, n2) = (names(&["n1"]), names(&["n2"]));
        assert!(!limits.admit(&[start_in(&leader, &n1, "gpu"), start_in(&member, &n2, "gpu")]));
        // Had the leader's charge on n1 stayed, n3 would be alice's second node.
        assert!(limits.admit(&[start_in(&later, &names(&["n3"]), "gpu")]));
    }

    fn one_group_node() -> QosLimits {
        QosLimits {
            grp_tres: Some(grp(&[(TresType::Node, 1)])),
            ..Default::default()
        }
    }

    #[test]
    fn a_borrower_gets_no_packing_credit_from_its_partition_qos() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_group_node());
        let holder = in_gpu(running(1, &["n1"]));
        let mut borrower = in_gpu(job(2, 1));
        borrower.idle_fill = true;
        let in_quota = in_gpu(job(3, 1));
        let limits = admission_in(&cm, &[&holder, &borrower, &in_quota], partitions);

        assert!(limits.fits(&in_quota), "it may pack onto n1");
        assert!(
            !limits.fits(&borrower),
            "it would need a second node for its partition QOS"
        );
    }

    /// A borrower runs outside its job QOS node cap only; its partition QOS counts its node.
    #[test]
    fn a_borrowed_start_counts_its_node_in_its_partition_qos() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_group_node());
        let mut borrower = in_gpu(job(1, 1));
        borrower.idle_fill = true;
        let in_quota = in_gpu(job(2, 1));
        let mut limits = admission_in(&cm, &[&borrower, &in_quota], partitions);

        assert!(limits.admit(&[start_in(&borrower, &names(&["n1"]), "gpu")]));
        assert!(!limits.admit(&[start_in(&in_quota, &names(&["n2"]), "gpu")]));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosGrpNodeLimit, None)]
        );
    }

    #[test]
    fn a_future_slot_over_its_partition_qos_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let cm = cluster(&dir, None, None);
        let partitions = gpu_under_usercap(&cm, one_node_per_user());
        let holder = in_gpu(running(1, &["n1"]));
        let waiting = in_gpu(job(2, 1));
        let mut limits = admission_in(&cm, &[&holder, &waiting], partitions);

        assert!(!limits.may_reserve(&start_in(&waiting, &names(&["n2"]), "gpu")));
        assert_eq!(
            limits.refusals(),
            vec![(2, PendingReason::QosMaxNodePerUserLimit, None)]
        );
    }
}
