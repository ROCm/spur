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
    /// Expanded, deduplicated `--nodelist`, whose cardinality determines
    /// whether the request is a candidate pool or an additive requirement.
    nodelist: Option<HashSet<String>>,
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
        let nodelist = job
            .spec
            .nodelist
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| HashSet::from_iter(expand_hostlist_or_split(s)));

        let exclude = job
            .spec
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| HashSet::from_iter(expand_hostlist_or_split(s)))
            .unwrap_or_default();

        let required_features = job
            .spec
            .constraint
            .as_deref()
            .map(|c| {
                c.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let partitions = job
            .spec
            .partition
            .as_deref()
            .map(|p| p.split(',').map(str::trim).collect())
            .unwrap_or_default();

        Self {
            job,
            nodelist,
            exclude,
            required_features,
            partitions,
        }
    }

    /// True if the job's `--nodelist`/`--exclude` permit this node by name.
    /// Name-only check: no partition, feature, capacity, or state involved.
    pub fn allows_name(&self, name: &str) -> bool {
        if let Some(ref allowed) = self.nodelist {
            if !self.nodelist_is_additive() && !allowed.contains(name) {
                return false;
            }
        }
        !self.exclude.contains(name)
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

    /// True if the node is in one of the job's requested partitions (or the job
    /// requested no partition).
    pub fn in_partition(&self, node: &Node) -> bool {
        if self.partitions.is_empty() {
            return true;
        }
        self.partitions
            .iter()
            .any(|rp| node.partitions.iter().any(|np| np == rp))
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
        };
        assert!(!p.matches(&node("node001"), std::slice::from_ref(&res), now));
        assert!(p.matches(&node("node002"), &[res], now));
    }
}
