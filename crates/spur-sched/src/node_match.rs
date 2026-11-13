// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared node-placement matching used by both the scheduler and the
//! pending-reason classifier so the two cannot drift. Capacity is not checked
//! here: callers apply their own (scheduler vs total, pending-reason vs free).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use tracing::warn;

use spur_core::job::Job;
use spur_core::node::Node;
use spur_core::reservation::Reservation;

/// Job placement constraints, parsed once and reused across all candidate nodes.
pub struct NodePlacement<'a> {
    job: &'a Job,
    /// Without an explicit node list, placement follows admission's reuse credits
    /// so the allocation honors the group-node charge.
    nodelist: Option<HashSet<String>>,
    use_reuse: bool,
    /// `--exclude` deny-list (expanded from hostlist).
    exclude: HashSet<String>,
    /// Requested `--constraint` features (all must be present on a node).
    required_features: Vec<&'a str>,
    /// Partitions the job may run in (`--partition` OR-list).
    partitions: Vec<&'a str>,
}

impl<'a> NodePlacement<'a> {
    /// Parse a job's placement constraints once.
    pub fn new(job: &'a Job) -> Self {
        Self::build(job, true, None)
    }

    /// Match the credits for the partition whose request placement is evaluating.
    pub fn new_in_partition(job: &'a Job, partition: &'a str) -> Self {
        Self::build(job, true, Some(partition))
    }

    /// Ignore reuse credits while account and QOS gates compute their independent credits.
    /// Actual placement uses [`new`](Self::new) to enforce the finished result.
    pub fn new_ignoring_preferred_nodes(job: &'a Job) -> Self {
        Self::build(job, false, None)
    }

    fn build(job: &'a Job, use_reuse: bool, partition: Option<&'a str>) -> Self {
        let nodelist = job
            .spec
            .nodelist
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| HashSet::from_iter(expand_hostlist_or_split(s)))
            .or_else(|| {
                if !use_reuse {
                    return None;
                }
                if job.node_reuse.is_empty() {
                    return (!job.preferred_nodes.is_empty()).then(|| job.preferred_nodes.clone());
                }
                let credits = job.node_reuse.get(partition?)?;
                let listed: HashSet<String> = credits.iter().flatten().cloned().collect();
                (!listed.is_empty()).then_some(listed)
            });

        let exclude = job
            .spec
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| HashSet::from_iter(expand_hostlist_or_split(s)))
            .unwrap_or_default();

        let required_features =
            spur_core::constraints::Constraints::parse(job.spec.constraint.as_deref())
                .map(|constraints| constraints.features)
                // Invalid requests must not become unconstrained candidates.
                .unwrap_or_else(|_| vec![job.spec.constraint.as_deref().unwrap_or_default()]);

        let partitions = if let Some(partition) = partition {
            vec![partition]
        } else {
            let requested: Vec<&str> =
                spur_core::partition::requested_partition_names(job.spec.partition.as_deref())
                    .collect();
            if requested.is_empty() && use_reuse && !job.node_reuse.is_empty() {
                job.node_reuse.keys().map(String::as_str).collect()
            } else {
                requested
            }
        };

        Self {
            job,
            nodelist,
            use_reuse,
            exclude,
            required_features,
            partitions,
        }
    }

    /// True if the job's node lists and complete reuse credit permit this node by name.
    /// Name-only check: no partition, feature, capacity, or state involved.
    pub fn allows_name(&self, name: &str) -> bool {
        if let Some(ref allowed) = self.nodelist {
            if !self.nodelist_is_additive() && !allowed.contains(name) {
                return false;
            }
        }
        !self.exclude.contains(name)
            && (self.partitions.is_empty()
                || self
                    .partitions
                    .iter()
                    .any(|partition| self.reuse_allows(name, partition)))
            && (!self.job.node_reuse.is_empty()
                || self.credit_allows(name, &self.job.preferred_nodes))
    }

    fn reuse_allows(&self, name: &str, partition: &str) -> bool {
        !self.use_reuse
            || self.job.node_reuse.get(partition).is_none_or(|credits| {
                credits
                    .iter()
                    .all(|credit| self.credit_allows(name, credit))
            })
    }

    fn credit_allows(&self, name: &str, credit: &HashSet<String>) -> bool {
        if !self.use_reuse || credit.is_empty() {
            return true;
        }
        let mandatory = self
            .nodelist
            .as_ref()
            .filter(|_| self.nodelist_is_additive());
        let covered =
            credit.len() + mandatory.map_or(0, |listed| listed.difference(credit).count());
        covered < (self.job.spec.num_nodes as usize).max(1)
            || credit.contains(name)
            || mandatory.is_some_and(|listed| listed.contains(name))
    }

    pub fn nodelist_is_additive(&self) -> bool {
        let Some(ref nodelist) = self.nodelist else {
            return false;
        };
        (self.job.spec.num_nodes as usize).max(1) > nodelist.len()
    }

    pub fn is_listed(&self, name: &str) -> bool {
        self.nodelist
            .as_ref()
            .is_some_and(|nodelist| nodelist.contains(name))
    }

    pub fn listed_count(&self) -> usize {
        self.nodelist.as_ref().map_or(0, HashSet::len)
    }

    /// True if the node is in one of the job's requested partitions (or the job
    /// requested no partition).
    pub fn in_partition(&self, node: &Node) -> bool {
        if self.partitions.is_empty() {
            return true;
        }
        self.partitions.iter().any(|rp| {
            node.partitions.iter().any(|np| np == rp) && self.reuse_allows(&node.name, rp)
        })
    }

    /// True if the node carries every requested `--constraint` feature.
    pub fn has_features(&self, node: &Node) -> bool {
        self.required_features
            .iter()
            .all(|f| node.features.iter().any(|nf| nf == f))
    }

    /// Load-independent placement identity (nodelist/exclude, partition,
    /// features, reservation), ignoring node state and free capacity.
    pub fn eligible(&self, node: &Node, reservations: &[Reservation], now: DateTime<Utc>) -> bool {
        self.allows_name(&node.name)
            && self.in_partition(node)
            && self.has_features(node)
            && self.reservation_ok(node, reservations, now)
    }

    /// [`eligible`](Self::eligible) plus runtime state (schedulable,
    /// exclusive-idle), still ignoring free capacity.
    pub fn matches(&self, node: &Node, reservations: &[Reservation], now: DateTime<Utc>) -> bool {
        // A node claimed by the managed k0s cluster is owned by the k8s
        // scheduler; Spur must not also place jobs on it (no GPU double-booking).
        !node.is_k0s_reserved() && self.matches_ignoring_k0s(node, reservations, now)
    }

    /// [`matches`](Self::matches) without the k0s-reservation gate, so the pending-reason
    /// classifier can ask "would this node match if it weren't reserved for k0s?".
    pub fn matches_ignoring_k0s(
        &self,
        node: &Node,
        reservations: &[Reservation],
        now: DateTime<Utc>,
    ) -> bool {
        if !self.eligible(node, reservations, now) {
            return false;
        }
        if !node.is_schedulable() {
            return false;
        }
        // Exclusive jobs need an idle node.
        if self.job.spec.exclusive
            && (node.alloc_resources.cpus > 0 || node.alloc_resources.has_devices())
        {
            return false;
        }
        true
    }

    /// Like [`matches`](Self::matches), but admits a busy-yet-up node as a
    /// candidate for a *future* reservation instead of excluding it outright.
    pub fn matches_for_reservation(
        &self,
        node: &Node,
        reservations: &[Reservation],
        now: DateTime<Utc>,
    ) -> bool {
        !node.is_k0s_reserved()
            && self.eligible(node, reservations, now)
            && node.state.is_up()
            && node.layout_block_reason().is_none()
    }

    /// True when a listed node can't satisfy the request in an additive
    /// nodelist — real placement rejects the whole job regardless of other idle capacity.
    pub fn additive_listed_node_unavailable<'n>(
        &self,
        nodes: impl IntoIterator<Item = &'n Node>,
        reservations: &[Reservation],
        now: DateTime<Utc>,
        required: &spur_core::resource::ResourceSet,
    ) -> bool {
        self.nodelist_is_additive()
            && nodes.into_iter().any(|n| {
                self.is_listed(&n.name)
                    && self.eligible(n, reservations, now)
                    && n.total_resources.can_satisfy(required)
                    && !self.matches_for_reservation(n, reservations, now)
            })
    }

    fn reservation_ok(
        &self,
        node: &Node,
        reservations: &[Reservation],
        now: DateTime<Utc>,
    ) -> bool {
        let job_reservation = self
            .job
            .spec
            .reservation
            .as_deref()
            .filter(|s| !s.is_empty());

        let active: Vec<&Reservation> = reservations
            .iter()
            .filter(|r| r.is_active(now) && r.covers_node(&node.name))
            .collect();

        match job_reservation {
            Some(res_name) => {
                let user = &self.job.spec.user;
                let account = self.job.spec.account.as_deref();
                active
                    .iter()
                    .any(|r| r.name == res_name && r.allows_user(user, account))
            }
            // Jobs without a reservation may not use reserved nodes.
            None => active.is_empty(),
        }
    }
}

/// Expand a hostlist pattern (e.g. `node[001-003]`) into individual names.
/// Falls back to a plain comma-split if the pattern is malformed, so existing
/// behaviour is preserved for simple comma-separated lists.
pub fn expand_hostlist_or_split(pattern: &str) -> Vec<String> {
    let names = match spur_core::hostlist::expand(pattern) {
        Ok(names) => names,
        Err(e) => {
            warn!(
                pattern,
                error = %e,
                "hostlist expansion failed, falling back to comma-split"
            );
            pattern.split(',').map(|s| s.trim().to_string()).collect()
        }
    };
    names.into_iter().filter(|name| !name.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use spur_core::job::JobSpec;
    use spur_core::node::NodeState;
    use spur_core::resource::ResourceSet;

    fn node(name: &str) -> Node {
        let mut n = Node::new(
            name.into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        n.state = NodeState::Idle;
        n.partitions = vec!["default".into()];
        n
    }

    fn job_with(spec: JobSpec) -> Job {
        Job::new(1, spec)
    }

    fn base_spec() -> JobSpec {
        JobSpec {
            name: "j".into(),
            partition: Some("default".into()),
            user: "test".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            ..Default::default()
        }
    }

    #[test]
    fn k0s_reserved_node_is_not_a_scheduling_match() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();

        let mut n = node("n1");
        assert!(p.matches(&n, &[], now), "idle node should match");

        n.k0s_role = Some(spur_core::k0s::K0sRole::Worker);
        assert!(!p.matches(&n, &[], now), "k0s worker must not match");
        // Placement identity is unchanged; only the runtime gate rejects it.
        assert!(p.eligible(&n, &[], now));

        n.k0s_role = None;
        assert!(
            p.matches(&n, &[], now),
            "cleared role reverts to schedulable"
        );
    }

    #[test]
    fn k0s_controller_and_single_are_also_excluded() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();
        for role in [
            spur_core::k0s::K0sRole::Controller,
            spur_core::k0s::K0sRole::Single,
        ] {
            let mut n = node("n1");
            n.k0s_role = Some(role);
            assert!(!p.matches(&n, &[], now), "{role:?} must be excluded");
        }
    }

    #[test]
    fn matches_for_reservation_admits_a_fully_busy_but_healthy_node() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();

        let mut n = node("n1");
        n.state = NodeState::Allocated;
        n.alloc_resources = spur_core::resource::ResourceAllocations::with_scalar(64, 256_000);

        assert!(!p.matches(&n, &[], now), "matches() still excludes it");
        assert!(
            p.matches_for_reservation(&n, &[], now),
            "a fully-busy Allocated node is still a valid future-reservation candidate"
        );
    }

    #[test]
    fn matches_for_reservation_still_excludes_down_and_k0s_nodes() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();

        let mut down = node("n1");
        down.state = NodeState::Down;
        assert!(!p.matches_for_reservation(&down, &[], now));

        let mut k0s = node("n2");
        k0s.k0s_role = Some(spur_core::k0s::K0sRole::Worker);
        assert!(!p.matches_for_reservation(&k0s, &[], now));
    }

    #[test]
    fn matches_for_reservation_excludes_a_node_blocked_by_its_core_layout() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();

        let mut bad_layout = node("n1");
        bad_layout.layout_error = Some("no CPU topology".into());
        assert!(!p.matches_for_reservation(&bad_layout, &[], now));

        let mut not_enforced = node("n2");
        not_enforced.layout_enforce_error = Some("root write failed".into());
        assert!(!p.matches_for_reservation(&not_enforced, &[], now));
    }

    #[test]
    fn malformed_hostlist_falls_back_to_comma_split() {
        // Reversed range is invalid; fallback returns the literal as one token.
        assert_eq!(
            expand_hostlist_or_split("node[003-001]"),
            vec!["node[003-001]"]
        );
        // Plain comma list is not a hostlist pattern; fallback splits it.
        assert_eq!(expand_hostlist_or_split("a,b,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_hostlist_entries_are_ignored() {
        assert_eq!(
            expand_hostlist_or_split("node001,,node002,"),
            vec!["node001", "node002"]
        );

        let job = job_with(JobSpec {
            nodelist: Some("node001,".into()),
            num_nodes: 2,
            ..base_spec()
        });
        assert!(NodePlacement::new(&job).nodelist_is_additive());
    }

    #[test]
    fn nodelist_expands_brackets() {
        let job = job_with(JobSpec {
            nodelist: Some("node[001-003]".into()),
            ..base_spec()
        });
        let p = NodePlacement::new(&job);
        assert!(p.allows_name("node001"));
        assert!(p.allows_name("node003"));
        assert!(!p.allows_name("node004"));
    }

    #[test]
    fn preferred_nodes_restricts_when_it_fully_covers_num_nodes() {
        let mut job = job_with(JobSpec {
            num_nodes: 1,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".to_string(), "node002".to_string()]);
        let p = NodePlacement::new(&job);
        assert!(!p.nodelist_is_additive());
        assert!(p.allows_name("node001"));
        assert!(p.allows_name("node002"));
        assert!(!p.allows_name("node003"));
    }

    #[test]
    fn preferred_nodes_is_additive_when_it_falls_short_of_num_nodes() {
        let mut job = job_with(JobSpec {
            num_nodes: 3,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".to_string()]);
        let p = NodePlacement::new(&job);
        assert!(p.nodelist_is_additive());
        assert!(p.allows_name("node001"));
        assert!(
            p.allows_name("node002"),
            "additive: nodes outside the credited set are still allowed"
        );
        assert!(p.is_listed("node001"));
        assert!(!p.is_listed("node002"));
    }

    #[test]
    fn explicit_nodelist_and_full_reuse_credit_both_restrict_placement() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node001".into()),
            num_nodes: 1,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node002".to_string()]);
        let p = NodePlacement::new(&job);
        assert!(!p.allows_name("node001"));
        assert!(!p.allows_name("node002"));
        assert!(!p.allows_name("node003"));
    }

    #[test]
    fn full_reuse_credit_restricts_an_explicit_candidate_pool() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-003]".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".to_string(), "node002".to_string()]);
        let p = NodePlacement::new(&job);
        assert!(!p.nodelist_is_additive());
        assert!(p.allows_name("node001"));
        assert!(p.allows_name("node002"));
        assert!(!p.allows_name("node003"));
        assert!(!p.allows_name("node004"));

        let admission = NodePlacement::new_ignoring_preferred_nodes(&job);
        assert!(admission.allows_name("node003"));
        assert!(!admission.allows_name("node004"));
    }

    #[test]
    fn partial_reuse_credit_keeps_an_explicit_candidate_pool() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-003]".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".to_string()]);
        let p = NodePlacement::new(&job);
        assert!(!p.nodelist_is_additive());
        assert!(p.allows_name("node001"));
        assert!(p.allows_name("node002"));
        assert!(p.allows_name("node003"));
        assert!(!p.allows_name("node004"));
    }

    #[test]
    fn additive_nodelist_stays_mandatory_when_credits_cover_the_fillers() {
        for credit in [
            HashSet::from(["node002".to_string()]),
            HashSet::from(["node002".to_string(), "node003".to_string()]),
        ] {
            let mut job = job_with(JobSpec {
                nodelist: Some("node001".into()),
                num_nodes: 2,
                ..base_spec()
            });
            job.preferred_nodes = credit.clone();
            let p = NodePlacement::new(&job);
            assert!(p.nodelist_is_additive());
            assert!(p.is_listed("node001"));
            assert!(p.allows_name("node001"));
            assert!(p.allows_name("node002"));
            assert_eq!(p.allows_name("node003"), credit.contains("node003"));
            assert!(!p.allows_name("node004"));

            job.node_reuse.insert("default".into(), vec![credit]);
            let scoped = NodePlacement::new_in_partition(&job, "default");
            assert!(scoped.nodelist_is_additive());
            assert!(scoped.is_listed("node001"));
            assert!(scoped.allows_name("node001"));
            assert!(scoped.allows_name("node002"));
            assert!(!scoped.allows_name("node004"));
        }
    }

    #[test]
    fn partial_credits_in_alternative_partitions_never_become_full() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-004]".into()),
            partition: Some("a,b".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".into(), "node003".into()]);
        job.node_reuse
            .insert("a".into(), vec![HashSet::from(["node001".into()])]);
        job.node_reuse
            .insert("b".into(), vec![HashSet::from(["node003".into()])]);
        let generic = NodePlacement::new(&job);
        let now = Utc::now();
        for (partition, names) in [("a", ["node001", "node002"]), ("b", ["node003", "node004"])] {
            let scoped = NodePlacement::new_in_partition(&job, partition);
            for name in names {
                let mut candidate = node(name);
                candidate.partitions = vec![partition.into()];
                assert!(generic.matches_for_reservation(&candidate, &[], now));
                assert!(scoped.matches_for_reservation(&candidate, &[], now));
            }
        }
    }

    #[test]
    fn independent_partial_quota_credits_never_become_full() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-003]".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".into(), "node002".into()]);
        job.node_reuse.insert(
            "default".into(),
            vec![
                HashSet::from(["node001".into()]),
                HashSet::from(["node002".into()]),
            ],
        );
        for p in [
            NodePlacement::new(&job),
            NodePlacement::new_in_partition(&job, "default"),
        ] {
            assert!(p.allows_name("node001"));
            assert!(p.allows_name("node002"));
            assert!(p.allows_name("node003"));
        }
    }

    #[test]
    fn full_credits_must_hold_for_each_quota_scope() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-004]".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.node_reuse.insert(
            "default".into(),
            vec![
                HashSet::from(["node001".into(), "node002".into()]),
                HashSet::from(["node002".into(), "node003".into()]),
            ],
        );
        let p = NodePlacement::new_in_partition(&job, "default");
        assert!(!p.allows_name("node001"));
        assert!(p.allows_name("node002"));
        assert!(!p.allows_name("node003"));
        assert!(!p.allows_name("node004"));
    }

    #[test]
    fn reuse_requires_membership_in_the_partition_that_credited_the_node() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node[001-003]".into()),
            partition: Some("a,b".into()),
            ..base_spec()
        });
        job.node_reuse
            .insert("a".into(), vec![HashSet::from(["node001".into()])]);
        job.node_reuse
            .insert("b".into(), vec![HashSet::from(["node002".into()])]);
        let p = NodePlacement::new(&job);
        let now = Utc::now();
        let mut candidate = node("node001");
        candidate.partitions = vec!["b".into()];
        assert!(p.allows_name("node001"));
        assert!(!p.matches_for_reservation(&candidate, &[], now));
        candidate.partitions = vec!["a".into()];
        assert!(p.matches_for_reservation(&candidate, &[], now));

        job.spec.partition = Some("a".into());
        assert!(!NodePlacement::new(&job).allows_name("node002"));
    }

    #[test]
    fn scoped_partial_credit_supplies_only_its_own_additive_fallback() {
        let mut job = job_with(JobSpec {
            partition: Some("a,b".into()),
            num_nodes: 2,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".into(), "node002".into()]);
        job.node_reuse
            .insert("a".into(), vec![HashSet::from(["node001".into()])]);
        job.node_reuse
            .insert("b".into(), vec![HashSet::from(["node002".into()])]);
        let scoped = NodePlacement::new_in_partition(&job, "a");
        assert!(scoped.nodelist_is_additive());
        assert!(scoped.is_listed("node001"));
        assert!(!scoped.is_listed("node002"));
        assert!(scoped.allows_name("node003"));
        let generic = NodePlacement::new(&job);
        assert!(!generic.nodelist_is_additive());
        assert!(generic.allows_name("node003"));
    }

    #[test]
    fn admission_ignores_scoped_credits() {
        let mut job = job_with(base_spec());
        job.preferred_nodes.insert("node001".into());
        job.node_reuse
            .insert("default".into(), vec![HashSet::from(["node001".into()])]);
        let p = NodePlacement::new_ignoring_preferred_nodes(&job);
        assert!(p.allows_name("node002"));
        assert!(!p.is_listed("node001"));
    }

    #[test]
    fn new_ignoring_preferred_nodes_never_falls_back_to_the_field() {
        // Admission calls this while it is still incrementally computing
        // job.preferred_nodes across two independent gates (account, then
        // QOS) for the same job. If the second gate's own placement view
        // fell back to a value the first gate already wrote, that credit
        // would leak in as a spurious restriction on the second gate's own,
        // independent reuse computation.
        let mut job = job_with(JobSpec {
            num_nodes: 1,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node001".to_string()]);
        let p = NodePlacement::new_ignoring_preferred_nodes(&job);
        assert!(
            !p.nodelist_is_additive(),
            "no nodelist at all means additive() must report false (no restriction), \
             just like a job with no --nodelist and no preferred_nodes"
        );
        assert!(
            p.allows_name("node001"),
            "not restricted to the credited set"
        );
        assert!(
            p.allows_name("node999"),
            "preferred_nodes must be completely ignored: any node is allowed"
        );
    }

    #[test]
    fn new_ignoring_preferred_nodes_still_honors_an_explicit_nodelist() {
        let mut job = job_with(JobSpec {
            nodelist: Some("node001".into()),
            num_nodes: 1,
            ..base_spec()
        });
        job.preferred_nodes = HashSet::from(["node002".to_string()]);
        let p = NodePlacement::new_ignoring_preferred_nodes(&job);
        assert!(
            p.allows_name("node001"),
            "the user's own nodelist still applies"
        );
        assert!(!p.allows_name("node002"));
        assert!(!p.allows_name("node003"));
    }

    #[test]
    fn exclude_expands_brackets() {
        let job = job_with(JobSpec {
            exclude: Some("node[001-002]".into()),
            ..base_spec()
        });
        let p = NodePlacement::new(&job);
        assert!(!p.allows_name("node001"));
        assert!(!p.allows_name("node002"));
        assert!(p.allows_name("node003"));
    }

    #[test]
    fn matches_respects_nodelist() {
        let job = job_with(JobSpec {
            nodelist: Some("node001".into()),
            ..base_spec()
        });
        let p = NodePlacement::new(&job);
        let now = Utc::now();
        assert!(p.matches(&node("node001"), &[], now));
        assert!(!p.matches(&node("node002"), &[], now));
    }

    #[test]
    fn matches_requires_all_features() {
        let job = job_with(JobSpec {
            constraint: Some("mi300x,nvlink".into()),
            ..base_spec()
        });
        let p = NodePlacement::new(&job);
        let now = Utc::now();

        let mut both = node("n1");
        both.features = vec!["mi300x".into(), "nvlink".into()];
        assert!(p.matches(&both, &[], now));

        let mut one = node("n2");
        one.features = vec!["mi300x".into()];
        assert!(!p.matches(&one, &[], now));
    }

    #[test]
    fn matches_blocks_reserved_node_for_unreserved_job() {
        let job = job_with(base_spec());
        let p = NodePlacement::new(&job);
        let now = Utc::now();
        let res = Reservation {
            name: "r1".into(),
            start_time: now - Duration::hours(1),
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        assert!(!p.matches(&node("node001"), std::slice::from_ref(&res), now));
        assert!(p.matches(&node("node002"), &[res], now));
    }
}
