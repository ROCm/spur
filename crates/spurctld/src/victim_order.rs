// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Secondary order among preemption victims of equal QOS priority, from the keys of
//! `scheduler.preempt_victim_order`. Eligibility is settled before this runs.

use std::cmp::Ordering;
use std::collections::HashMap;

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
}

impl VictimFacts {
    const UNKNOWN: Self = Self {
        elapsed_secs: 0.0,
        nodes: 1,
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
            };
            (job.job_id, facts)
        })
        .collect()
}

/// What taking a node would cost, and how many nodes it frees. Jobs already in the
/// victim set cost nothing more.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NodeRank {
    new_jobs: u32,
    work: f64,
    /// Nodes the pick empties and the preemptor still needs; at least 1.
    freed: u32,
    hash: u64,
}

/// How the nodes of one preemptor's victim search rank against each other.
pub(crate) struct VictimOrder<'a> {
    keys: &'a [VictimOrderKey],
    facts: &'a HashMap<JobId, VictimFacts>,
    /// Seeds the hash key: one preemptor sees one stable order, others spread out.
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

    /// The `hash` key's value for `node`.
    pub(crate) fn hash(&self, node: &str) -> u64 {
        stable_hash(self.preemptor, node)
    }

    /// What taking a node holding `new_jobs` would cost, when it empties `freed`
    /// nodes.
    pub(crate) fn rank(&self, hash: u64, new_jobs: &[JobId], freed: u32) -> NodeRank {
        let mut rank = NodeRank {
            new_jobs: new_jobs.len() as u32,
            work: 0.0,
            freed: freed.max(1),
            hash,
        };
        for id in new_jobs {
            let facts = self.facts.get(id).unwrap_or(&VictimFacts::UNKNOWN);
            rank.work += facts.work();
        }
        rank
    }

    /// `Less` when the node ranked `a` should be taken before `b`: the first key that
    /// tells them apart decides, and `Equal` leaves the tie to the caller. Jobs and
    /// work compare per node freed, cross-multiplied.
    pub(crate) fn compare(&self, a: &NodeRank, b: &NodeRank) -> Ordering {
        let (fa, fb) = (f64::from(a.freed), f64::from(b.freed));
        self.keys
            .iter()
            .map(|key| match key {
                VictimOrderKey::FewestJobs => (u64::from(a.new_jobs) * u64::from(b.freed))
                    .cmp(&(u64::from(b.new_jobs) * u64::from(a.freed))),
                VictimOrderKey::LeastWork => (a.work * fb).total_cmp(&(b.work * fa)),
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

    fn facts(elapsed_secs: f64, nodes: u32) -> VictimFacts {
        VictimFacts {
            elapsed_secs,
            nodes,
        }
    }

    fn table(entries: &[(JobId, VictimFacts)]) -> HashMap<JobId, VictimFacts> {
        entries.iter().copied().collect()
    }

    /// `Less` when the first node is taken before the second, each freeing `freed`
    /// nodes (1 unless given), given the jobs already chosen, under `keys`.
    fn order_of(
        keys: &[VictimOrderKey],
        table: &HashMap<JobId, VictimFacts>,
        chosen: &[JobId],
        a: (&str, &[JobId]),
        b: (&str, &[JobId]),
    ) -> Ordering {
        order_of_freed(keys, table, chosen, (a.0, a.1, 1), (b.0, b.1, 1))
    }

    fn order_of_freed(
        keys: &[VictimOrderKey],
        table: &HashMap<JobId, VictimFacts>,
        chosen: &[JobId],
        a: (&str, &[JobId], u32),
        b: (&str, &[JobId], u32),
    ) -> Ordering {
        let order = VictimOrder::new(keys, table, 1);
        let rank = |(node, jobs, freed): (&str, &[JobId], u32)| {
            let new: Vec<JobId> = jobs
                .iter()
                .copied()
                .filter(|id| !chosen.contains(id))
                .collect();
            order.rank(order.hash(node), &new, freed)
        };
        order.compare(&rank(a), &rank(b))
    }

    #[test]
    fn jobs_and_work_compare_per_node_freed() {
        let t = table(&[
            (10, facts(3600.0, 4)),
            (20, facts(3600.0, 1)),
            (21, facts(3600.0, 1)),
        ]);
        let wide = ("n1", &[10][..], 4);
        let narrow = ("n5", &[20][..], 1);
        assert_eq!(
            order_of_freed(&[VictimOrderKey::FewestJobs], &t, &[], wide, narrow),
            Ordering::Less,
            "1 job for 4 nodes beats 1 job for 1"
        );
        assert_eq!(
            order_of_freed(&[VictimOrderKey::LeastWork], &t, &[], wide, narrow),
            Ordering::Equal,
            "14400s for 4 nodes equals 3600s for 1"
        );
        assert_eq!(
            order_of_freed(
                &[VictimOrderKey::FewestJobs],
                &t,
                &[],
                wide,
                ("n6", &[20, 21], 1)
            ),
            Ordering::Less
        );
        // One node needed: the wide job frees one node for four times the work.
        assert_eq!(
            order_of_freed(
                &[VictimOrderKey::LeastWork],
                &t,
                &[],
                ("n1", &[10], 1),
                ("n6", &[20, 21], 1)
            ),
            Ordering::Greater
        );
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
            (10, facts(3600.0, 1)),
            (20, facts(1000.0, 1)),
            (30, facts(1000.0, 8)),
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
            order.compare(
                &order.rank(order.hash("n1"), &[], 1),
                &order.rank(order.hash("n2"), &[], 1),
            )
        };
        assert_eq!(first(1), Ordering::Greater, "preemptor 1 takes n2 first");
        assert_eq!(first(2), Ordering::Less, "preemptor 2 takes n1 first");
    }

    #[test]
    fn keys_apply_in_the_order_listed_and_later_keys_only_break_ties() {
        // n1 adds one job of old work; n2 adds two jobs of fresh work.
        let t = table(&[
            (10, facts(3600.0, 1)),
            (20, facts(10.0, 1)),
            (21, facts(10.0, 1)),
        ]);
        let n1 = ("n1", &[10][..]);
        let n2 = ("n2", &[20, 21][..]);
        let jobs_first = [VictimOrderKey::FewestJobs, VictimOrderKey::LeastWork];
        let work_first = [VictimOrderKey::LeastWork, VictimOrderKey::FewestJobs];
        assert_eq!(order_of(&jobs_first, &t, &[], n1, n2), Ordering::Less);
        assert_eq!(order_of(&work_first, &t, &[], n1, n2), Ordering::Greater);

        // A tie on the first key falls through to the next.
        let t = table(&[(10, facts(500.0, 1)), (20, facts(100.0, 1))]);
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
        let t = table(&[(10, facts(9.0, 4)), (20, facts(0.0, 1))]);
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
        let hit = running_job(1, &["n1", "n2"], Some(600), now);
        let unstarted = running_job(2, &[], None, now);
        let skewed = running_job(3, &["n3"], Some(-30), now);

        let f = victim_facts(&[hit, unstarted, skewed], now);
        assert_eq!(f[&1], facts(600.0, 2));
        assert_eq!(f[&2], facts(0.0, 1), "no start time and no nodes recorded");
        assert_eq!(
            f[&3].elapsed_secs, 0.0,
            "a start time in the future is not negative work"
        );
    }
}
