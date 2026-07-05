// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::hostlist;
use crate::job::{Job, JobState};

/// Optional reservation behavior flags (comma-separated in admin CLI).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReservationFlags {
    #[serde(default)]
    pub maint: bool,
    #[serde(default)]
    pub ignore_jobs: bool,
    #[serde(default)]
    pub no_hold_jobs: bool,
    #[serde(default)]
    pub overlap: bool,
}

impl ReservationFlags {
    pub fn parse_list(values: &[String]) -> Result<Self, String> {
        let mut flags = Self::default();
        for v in values {
            for part in v.split(',') {
                match part.trim().to_ascii_lowercase().as_str() {
                    "maint" => flags.maint = true,
                    "ignore_jobs" => flags.ignore_jobs = true,
                    "no_hold_jobs" => flags.no_hold_jobs = true,
                    "overlap" => flags.overlap = true,
                    "" => {}
                    other => return Err(format!("unknown reservation flag '{other}'")),
                }
            }
        }
        Ok(flags)
    }

    pub fn parse_csv(csv: &str) -> Result<Self, String> {
        if csv.is_empty() {
            return Ok(Self::default());
        }
        Self::parse_list(
            &csv.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>(),
        )
    }

    pub fn display_csv(&self) -> String {
        let mut parts = Vec::new();
        if self.maint {
            parts.push("MAINT");
        }
        if self.ignore_jobs {
            parts.push("IGNORE_JOBS");
        }
        if self.no_hold_jobs {
            parts.push("NO_HOLD_JOBS");
        }
        if self.overlap {
            parts.push("OVERLAP");
        }
        parts.join(",")
    }
}

/// A resource reservation that blocks nodes for specific users/accounts
/// during a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    pub name: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub nodes: Vec<String>,
    pub accounts: Vec<String>,
    pub users: Vec<String>,
    #[serde(default)]
    pub flags: ReservationFlags,
}

impl Reservation {
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.start_time && now < self.end_time
    }

    pub fn is_future(&self, now: DateTime<Utc>) -> bool {
        now < self.start_time
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.end_time
    }

    pub fn state_label(&self, now: DateTime<Utc>) -> &'static str {
        if self.is_active(now) {
            "ACTIVE"
        } else if self.is_future(now) {
            "INACTIVE"
        } else {
            "EXPIRED"
        }
    }

    pub fn covers_node(&self, node: &str) -> bool {
        self.nodes.iter().any(|n| n == node)
    }

    pub fn allows_user(&self, user: &str, account: Option<&str>) -> bool {
        if self.users.is_empty() && self.accounts.is_empty() {
            return true;
        }
        if self.users.iter().any(|u| u == user) {
            return true;
        }
        if let Some(acct) = account {
            if self.accounts.iter().any(|a| a == acct) {
                return true;
            }
        }
        false
    }
}

/// Whether two reservations share a node and overlap in time.
pub fn reservations_overlap(a: &Reservation, b: &Reservation) -> bool {
    if a.end_time <= b.start_time || b.end_time <= a.start_time {
        return false;
    }
    a.nodes.iter().any(|n| b.covers_node(n))
}

/// Whether overlap between `candidate` and `existing` is permitted by flags.
pub fn overlap_allowed(candidate: &Reservation, existing: &Reservation) -> bool {
    candidate.flags.overlap
        || candidate.flags.maint
        || existing.flags.overlap
        || existing.flags.maint
}

/// Running job is tied to an active reservation on its allocated nodes.
pub fn job_runs_in_active_reservation(
    job: &Job,
    reservations: &[Reservation],
    now: DateTime<Utc>,
) -> bool {
    let Some(res_name) = job.spec.reservation.as_deref() else {
        return false;
    };
    reservations.iter().any(|r| {
        r.name == res_name
            && r.is_active(now)
            && job.allocated_nodes.iter().any(|node| r.covers_node(node))
    })
}

/// Pending or running job has a matching active reservation (for priority ordering).
pub fn job_has_active_reservation(
    job: &Job,
    reservations: &[Reservation],
    now: DateTime<Utc>,
) -> bool {
    let Some(res_name) = job.spec.reservation.as_deref() else {
        return false;
    };
    reservations.iter().any(|r| {
        r.name == res_name
            && r.is_active(now)
            && r.allows_user(&job.spec.user, job.spec.account.as_deref())
    })
}

/// Expand hostlist patterns and verify each name exists in the cluster.
pub fn normalize_node_list(
    patterns: &[String],
    known_nodes: &std::collections::HashSet<String>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let expanded = hostlist::expand(trimmed).map_err(|e| e.to_string())?;
        for node in expanded {
            if !known_nodes.contains(&node) {
                return Err(format!("unknown node '{}'", node));
            }
            if !out.contains(&node) {
                out.push(node);
            }
        }
    }
    if out.is_empty() {
        return Err("reservation requires at least one node".into());
    }
    Ok(out)
}

/// Estimated end time for a running job (start + time limit).
pub fn running_job_end(job: &Job, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let _ = now;
    if !matches!(
        job.state,
        JobState::Running | JobState::Completing | JobState::Suspended
    ) {
        return None;
    }
    let start = job.start_time?;
    let limit = job.spec.time_limit?;
    Some(start + limit)
}

/// Returns the first running job that would still occupy `nodes` at `start_time`.
pub fn running_jobs_overlap_start(
    jobs: &std::collections::HashMap<crate::job::JobId, Job>,
    nodes: &[String],
    start_time: DateTime<Utc>,
    except_reservation: Option<&str>,
) -> Option<(crate::job::JobId, String)> {
    let node_set: std::collections::HashSet<&str> = nodes.iter().map(String::as_str).collect();
    for job in jobs.values() {
        if !matches!(
            job.state,
            JobState::Running | JobState::Completing | JobState::Suspended
        ) {
            continue;
        }
        if let Some(ex) = except_reservation {
            if job.spec.reservation.as_deref() == Some(ex) {
                continue;
            }
        }
        let overlaps_node = job
            .allocated_nodes
            .iter()
            .any(|n| node_set.contains(n.as_str()));
        if !overlaps_node {
            continue;
        }
        let end = running_job_end(job, Utc::now()).unwrap_or(start_time);
        if end > start_time {
            let node = job
                .allocated_nodes
                .iter()
                .find(|n| node_set.contains(n.as_str()))
                .cloned()
                .unwrap_or_default();
            return Some((job.job_id, node));
        }
    }
    None
}

/// Whether a job without reservation access would overlap this reservation window.
pub fn prospective_overlap(
    job: &Job,
    reservation: &Reservation,
    node: &str,
    now: DateTime<Utc>,
    job_duration: Duration,
) -> bool {
    if !reservation.covers_node(node) {
        return false;
    }
    let res_name = job.spec.reservation.as_deref().filter(|s| !s.is_empty());
    if res_name == Some(reservation.name.as_str())
        && reservation.allows_user(&job.spec.user, job.spec.account.as_deref())
    {
        return false;
    }
    let job_end = now + job_duration;
    if reservation.is_active(now) {
        return job_end > reservation.end_time || now < reservation.start_time;
    }
    if reservation.is_future(now) {
        return job_end > reservation.start_time;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    fn make_reservation() -> Reservation {
        let now = Utc::now();
        Reservation {
            name: "maint".into(),
            start_time: now - Duration::hours(1),
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into(), "node002".into()],
            accounts: vec!["research".into()],
            users: vec!["alice".into()],
            flags: ReservationFlags::default(),
        }
    }

    #[test]
    fn test_is_active() {
        let res = make_reservation();
        assert!(res.is_active(Utc::now()));
        assert!(!res.is_active(res.end_time + Duration::seconds(1)));
        assert!(!res.is_active(res.start_time - Duration::seconds(1)));
    }

    #[test]
    fn test_covers_node() {
        let res = make_reservation();
        assert!(res.covers_node("node001"));
        assert!(res.covers_node("node002"));
        assert!(!res.covers_node("node003"));
    }

    #[test]
    fn test_allows_user() {
        let res = make_reservation();
        assert!(res.allows_user("alice", None));
        assert!(res.allows_user("bob", Some("research")));
        assert!(!res.allows_user("bob", Some("other")));
        assert!(!res.allows_user("bob", None));
    }

    #[test]
    fn test_unrestricted_reservation() {
        let now = Utc::now();
        let res = Reservation {
            name: "open".into(),
            start_time: now - Duration::hours(1),
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags::default(),
        };
        assert!(res.allows_user("anyone", None));
    }

    #[test]
    fn flags_parse_csv() {
        let f = ReservationFlags::parse_csv("maint,ignore_jobs,overlap").unwrap();
        assert!(f.maint);
        assert!(f.ignore_jobs);
        assert!(!f.no_hold_jobs);
        assert!(f.overlap);
    }

    #[test]
    fn flags_reject_unknown() {
        assert!(ReservationFlags::parse_csv("not_a_flag").is_err());
    }

    #[test]
    fn reservations_overlap_detects_shared_node_and_time() {
        let now = Utc::now();
        let a = Reservation {
            name: "a".into(),
            start_time: now,
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags::default(),
        };
        let b = Reservation {
            name: "b".into(),
            start_time: now + Duration::minutes(30),
            end_time: now + Duration::hours(2),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags::default(),
        };
        assert!(reservations_overlap(&a, &b));
    }

    #[test]
    fn overlap_allowed_with_overlap_flag() {
        let now = Utc::now();
        let mut a = Reservation {
            name: "a".into(),
            start_time: now,
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags {
                overlap: true,
                ..Default::default()
            },
        };
        let b = Reservation {
            name: "b".into(),
            start_time: now,
            end_time: now + Duration::hours(1),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: ReservationFlags::default(),
        };
        assert!(overlap_allowed(&a, &b));
        a.flags.overlap = false;
        assert!(!overlap_allowed(&a, &b));
    }

    #[test]
    fn prospective_overlap_blocks_long_job_before_reservation() {
        let now = Utc::now();
        let res = Reservation {
            name: "r1".into(),
            start_time: now + Duration::hours(1),
            end_time: now + Duration::hours(2),
            nodes: vec!["node001".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: ReservationFlags::default(),
        };
        let job = Job::new(1, JobSpec::default());
        assert!(prospective_overlap(
            &job,
            &res,
            "node001",
            now,
            Duration::hours(3)
        ));
    }
}
