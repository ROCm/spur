// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Secondary order among preemption victims of equal QOS priority.
//!
//! Left to node name alone, the lowest-named nodes lose every time. The keys of
//! `scheduler.preempt_victim_order` rank the tied nodes first, so the victim set
//! evicts few jobs and loses little work, and node name only breaks what is still
//! tied. The keys never decide who may be preempted: eligibility, QOS priority and
//! the exempt time are settled before any of this runs.
//!
//! Every key reads only what the controller already holds in memory.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use spur_core::job::{Job, JobId};
use spur_core::partition::VictimOrderKey;

/// What evicting one running job costs, read once per scheduling pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VictimFacts {
    /// Seconds the current run has lasted; 0 when no start time is recorded.
    pub elapsed_secs: f64,
    /// Nodes the job holds, so a wide job's lost work scales with its width.
    pub nodes: u32,
    /// Times the job has already been requeued by preemption. It lives on the job
    /// itself, so a requeue does not reset it.
    pub requeues: u32,
}

impl VictimFacts {
    const UNKNOWN: Self = Self {
        elapsed_secs: 0.0,
        nodes: 1,
        requeues: 0,
    };

    fn work(&self) -> f64 {
        self.elapsed_secs * f64::from(self.nodes)
    }
}

/// The facts of every job in `running`, keyed by job id.
pub(crate) fn victim_facts(running: &[Job], now: DateTime<Utc>) -> HashMap<JobId, VictimFacts> {
    running
        .iter()
        .map(|job| {
            let elapsed_secs = job
                .start_time
                .map_or(0.0, |started| (now - started).num_seconds().max(0) as f64);
            let facts = VictimFacts {
                elapsed_secs,
                nodes: job.allocated_nodes.len().max(1) as u32,
                requeues: job.preempt_requeue_count,
            };
            (job.job_id, facts)
        })
        .collect()
}

/// What evicting a node would add to the victim set built so far. Only the jobs the
/// set does not already hold count: a job already chosen costs nothing more.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NodeRank {
    new_jobs: u32,
    work: f64,
    requeues: u32,
    hash: u64,
}

/// How the nodes of one preemptor's victim search rank against each other.
pub(crate) struct VictimOrder<'a> {
    keys: &'a [VictimOrderKey],
    facts: &'a HashMap<JobId, VictimFacts>,
    /// Seeds the hash key: one preemptor sees one stable order, different
    /// preemptors spread over different nodes.
    preemptor: JobId,
}

impl<'a> VictimOrder<'a> {
    pub(crate) fn new(
        keys: &'a [VictimOrderKey],
        facts: &'a HashMap<JobId, VictimFacts>,
        preemptor: JobId,
    ) -> Self {
        Self {
            keys,
            facts,
            preemptor,
        }
    }

    /// What taking `node`, with its `jobs`, would add to `chosen`. A node is as
    /// protected as its most protected job, because emptying it evicts every one:
    /// for `requeue_count` the node takes the highest count among its new jobs.
    pub(crate) fn rank(&self, node: &str, jobs: &[JobId], chosen: &HashSet<JobId>) -> NodeRank {
        let mut rank = NodeRank {
            new_jobs: 0,
            work: 0.0,
            requeues: 0,
            hash: stable_hash(self.preemptor, node),
        };
        for id in jobs.iter().filter(|id| !chosen.contains(id)) {
            let facts = self.facts.get(id).unwrap_or(&VictimFacts::UNKNOWN);
            rank.new_jobs += 1;
            rank.work += facts.work();
            rank.requeues = rank.requeues.max(facts.requeues);
        }
        rank
    }

    /// `Less` when the node ranked `a` should be taken before `b`: the first key that
    /// tells them apart decides, and `Equal` leaves the tie to the caller.
    pub(crate) fn compare(&self, a: &NodeRank, b: &NodeRank) -> Ordering {
        self.keys
            .iter()
            .map(|key| match key {
                VictimOrderKey::FewestJobs => a.new_jobs.cmp(&b.new_jobs),
                VictimOrderKey::LeastWork => a.work.total_cmp(&b.work),
                VictimOrderKey::RequeueCount => a.requeues.cmp(&b.requeues),
                VictimOrderKey::Hash => a.hash.cmp(&b.hash),
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

/// A 64-bit hash of the preemptor and node that never changes between runs or
/// toolchains, unlike a std hasher: FNV-1a with fixed constants, then a finalizer
/// so names that differ in one trailing byte still land far apart.
fn stable_hash(preemptor: JobId, node: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in preemptor.to_le_bytes().iter().chain(node.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;

    fn facts(elapsed_secs: f64, nodes: u32, requeues: u32) -> VictimFacts {
        VictimFacts {
            elapsed_secs,
            nodes,
            requeues,
        }
    }

    fn table(entries: &[(JobId, VictimFacts)]) -> HashMap<JobId, VictimFacts> {
        entries.iter().copied().collect()
    }

    /// `Less` when the first node is taken before the second, given the jobs already
    /// chosen, under `keys`.
    fn order_of(
        keys: &[VictimOrderKey],
        table: &HashMap<JobId, VictimFacts>,
        chosen: &[JobId],
        a: (&str, &[JobId]),
        b: (&str, &[JobId]),
    ) -> Ordering {
        let order = VictimOrder::new(keys, table, 1);
        let chosen: HashSet<JobId> = chosen.iter().copied().collect();
        order.compare(
            &order.rank(a.0, a.1, &chosen),
            &order.rank(b.0, b.1, &chosen),
        )
    }

    #[test]
    fn fewest_jobs_takes_the_node_that_adds_fewer_jobs() {
        let keys = [VictimOrderKey::FewestJobs];
        let t = table(&[]);
        assert_eq!(
            order_of(&keys, &t, &[], ("n1", &[10, 11]), ("n2", &[20])),
            Ordering::Greater,
            "two new jobs rank after one"
        );
        assert_eq!(
            order_of(&keys, &t, &[], ("n2", &[20]), ("n1", &[10, 11])),
            Ordering::Less
        );
    }

    #[test]
    fn a_job_already_chosen_adds_nothing() {
        let keys = [VictimOrderKey::FewestJobs];
        let t = table(&[]);
        // n1 holds 10 and 11, but 10 is already in the set, so it adds one job: the
        // same as n2, and the tie falls through to the caller.
        assert_eq!(
            order_of(&keys, &t, &[10], ("n1", &[10, 11]), ("n2", &[20])),
            Ordering::Equal
        );
        // A node whose jobs are all chosen adds none and ranks first.
        assert_eq!(
            order_of(&keys, &t, &[10, 11], ("n1", &[10, 11]), ("n2", &[20])),
            Ordering::Less
        );
    }

    #[test]
    fn least_work_takes_the_cheaper_work_first_and_scales_with_width() {
        let keys = [VictimOrderKey::LeastWork];
        let t = table(&[
            (10, facts(3600.0, 1, 0)),
            (20, facts(1000.0, 1, 0)),
            (30, facts(1000.0, 8, 0)),
        ]);
        assert_eq!(
            order_of(&keys, &t, &[], ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "the longer-running job is the more costly to lose"
        );
        assert_eq!(
            order_of(&keys, &t, &[], ("n2", &[20]), ("n3", &[30])),
            Ordering::Less,
            "the same run time on eight nodes is eight times the work"
        );
        // Work already committed to the set is not counted again.
        assert_eq!(
            order_of(&keys, &t, &[10], ("n1", &[10]), ("n2", &[20])),
            Ordering::Less
        );
    }

    #[test]
    fn requeue_count_ranks_a_job_that_keeps_being_hit_later() {
        let keys = [VictimOrderKey::RequeueCount];
        let t = table(&[
            (10, facts(0.0, 1, 3)),
            (20, facts(0.0, 1, 0)),
            (21, facts(0.0, 1, 5)),
        ]);
        assert_eq!(
            order_of(&keys, &t, &[], ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater
        );
        assert_eq!(
            order_of(&keys, &t, &[], ("n1", &[10]), ("n2", &[20, 21])),
            Ordering::Less,
            "a node is as protected as its most requeued job"
        );
    }

    #[test]
    fn hash_is_stable_and_spreads_over_preemptors() {
        // Pinned values: the order must not change between toolchains or runs.
        assert_eq!(stable_hash(1, "n1"), 0xb023_3076_789d_b4ff);
        assert_eq!(stable_hash(1, "n2"), 0x0967_b5d5_e13c_6966);
        assert_eq!(stable_hash(2, "n1"), 0x090e_6d81_b765_6ce6);
        assert_eq!(stable_hash(2, "n2"), 0x27ce_7728_24ea_f7c2);

        let t = table(&[]);
        let keys = [VictimOrderKey::Hash];
        let first = |preemptor: JobId| {
            let order = VictimOrder::new(&keys, &t, preemptor);
            let none = HashSet::new();
            order.compare(&order.rank("n1", &[], &none), &order.rank("n2", &[], &none))
        };
        assert_eq!(first(1), Ordering::Greater, "preemptor 1 takes n2 first");
        assert_eq!(first(2), Ordering::Less, "preemptor 2 takes n1 first");
    }

    #[test]
    fn keys_apply_in_the_order_listed_and_later_keys_only_break_ties() {
        // n1 adds one job of old, much-requeued work; n2 adds two jobs of fresh work.
        let t = table(&[
            (10, facts(3600.0, 1, 4)),
            (20, facts(10.0, 1, 0)),
            (21, facts(10.0, 1, 0)),
        ]);
        let n1 = ("n1", &[10][..]);
        let n2 = ("n2", &[20, 21][..]);
        let jobs_first = [VictimOrderKey::FewestJobs, VictimOrderKey::LeastWork];
        let work_first = [VictimOrderKey::LeastWork, VictimOrderKey::FewestJobs];
        assert_eq!(order_of(&jobs_first, &t, &[], n1, n2), Ordering::Less);
        assert_eq!(order_of(&work_first, &t, &[], n1, n2), Ordering::Greater);

        // A tie on the first key falls through to the next.
        let t = table(&[(10, facts(500.0, 1, 0)), (20, facts(100.0, 1, 0))]);
        assert_eq!(
            order_of(&jobs_first, &t, &[], ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "equal job counts, so the cheaper work goes first"
        );

        // Equal on every listed key leaves the tie to the caller.
        let t = table(&[]);
        assert_eq!(
            order_of(&jobs_first, &t, &[], ("n1", &[10]), ("n2", &[20])),
            Ordering::Equal
        );
    }

    #[test]
    fn an_empty_key_list_never_separates_nodes() {
        let t = table(&[(10, facts(9.0, 4, 9)), (20, facts(0.0, 1, 0))]);
        assert_eq!(
            order_of(&[], &t, &[], ("n1", &[10]), ("n2", &[20])),
            Ordering::Equal
        );
    }

    fn running_job(
        id: JobId,
        nodes: &[&str],
        started_secs_ago: Option<i64>,
        now: DateTime<Utc>,
    ) -> Job {
        let mut job = Job::new(
            id,
            JobSpec {
                name: format!("job-{id}"),
                user: "u".into(),
                num_nodes: nodes.len().max(1) as u32,
                num_tasks: 1,
                cpus_per_task: 1,
                ..Default::default()
            },
        );
        job.allocated_nodes = nodes.iter().map(|n| (*n).to_owned()).collect();
        job.start_time = started_secs_ago.map(|secs| now - chrono::Duration::seconds(secs));
        job
    }

    #[test]
    fn facts_come_from_the_running_job_itself() {
        let now = Utc::now();
        let mut hit = running_job(1, &["n1", "n2"], Some(600), now);
        hit.preempt_requeue_count = 2;
        let unstarted = running_job(2, &[], None, now);
        let skewed = running_job(3, &["n3"], Some(-30), now);

        let f = victim_facts(&[hit, unstarted, skewed], now);
        assert_eq!(f[&1], facts(600.0, 2, 2));
        assert_eq!(
            f[&2],
            facts(0.0, 1, 0),
            "no start time and no nodes recorded"
        );
        assert_eq!(
            f[&3].elapsed_secs, 0.0,
            "a start time in the future is not negative work"
        );
    }
}
