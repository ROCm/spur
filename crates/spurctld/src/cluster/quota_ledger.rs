// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! QOS, account and partition QOS usage summed once from the job table, so the
//! admission gates read a scope's usage instead of scanning every job on every check.
//! A scheduling pass charges its starts into the same ledger, so each start sees the
//! ones before it.

use std::collections::HashMap;

use spur_core::accounting::TresRecord;
use spur_core::job::{Job, JobId};

use super::{account_of, is_submitted, RunningTresAccumulator};

/// Usage per QOS, per account and per partition QOS, with each user's share in it.
/// Owns its names so it outlives the read of the job table it was summed from.
#[derive(Clone, Default)]
pub(super) struct QuotaLedger {
    qos: HashMap<String, ScopeTally>,
    accounts: HashMap<String, ScopeTally>,
    /// Counts no submitted jobs: a partition QOS applies no submit limit.
    partition_qos: HashMap<String, ScopeTally>,
}

#[derive(Clone, Default)]
struct ScopeTally {
    running: RunningTresAccumulator,
    users: HashMap<String, UserTally>,
}

#[derive(Clone, Default)]
struct UserTally {
    running_jobs: u32,
    running: RunningTresAccumulator,
    /// The user's pending and running jobs in the scope, ascending.
    submitted: Vec<JobId>,
}

impl QuotaLedger {
    pub(super) fn from_jobs<'a>(jobs: impl IntoIterator<Item = &'a Job>) -> Self {
        let mut ledger = Self::default();
        for job in jobs {
            ledger.add(job);
        }
        for scope in ledger.qos.values_mut().chain(ledger.accounts.values_mut()) {
            for user in scope.users.values_mut() {
                user.submitted.sort_unstable();
            }
        }
        ledger
    }

    pub(super) fn qos(&self, qos: &str, user: &str) -> QuotaView<'_> {
        QuotaView::of(self.qos.get(qos), user)
    }

    pub(super) fn account(&self, account: &str, user: &str) -> QuotaView<'_> {
        QuotaView::of(self.accounts.get(account), user)
    }

    pub(super) fn partition_qos(&self, qos: &str, user: &str) -> QuotaView<'_> {
        QuotaView::of(self.partition_qos.get(qos), user)
    }

    /// Adds each of `jobs` still holding its allocation to the partition QOS
    /// `charged_qos` names for it. Borrowed runs count too: a partition QOS bounds
    /// what a user holds, borrowed or not.
    pub(super) fn add_partition_qos<'a, 'q>(
        &mut self,
        jobs: impl IntoIterator<Item = &'a Job>,
        charged_qos: impl Fn(&Job) -> Option<&'q str>,
    ) {
        for job in jobs.into_iter().filter(|job| job.holds_allocation()) {
            if let Some(qos) = charged_qos(job) {
                update(&mut self.partition_qos, qos, |scope| {
                    scope.add(job, true, false)
                });
            }
        }
    }

    /// Charges `job` as running on `nodes`, under `partition_qos` if its partition
    /// has one. A borrowed start adds no node to its QOS, whose node cap it runs
    /// outside of, but counts in every other figure for the rest of the pass.
    pub(super) fn charge_start(
        &mut self,
        job: &Job,
        nodes: &[String],
        partition_qos: Option<&str>,
    ) {
        if let Some(qos) = job.spec.qos.as_deref() {
            let qos_nodes: &[String] = if job.idle_fill { &[] } else { nodes };
            update(&mut self.qos, qos, |scope| {
                scope.start(job, qos_nodes, nodes)
            });
        }
        for (tallies, name) in [
            (&mut self.accounts, account_of(job)),
            (&mut self.partition_qos, partition_qos),
        ] {
            if let Some(name) = name {
                update(tallies, name, |scope| scope.start(job, nodes, nodes));
            }
        }
    }

    /// The tallies the `starts` [charged](QuotaLedger::charge_start) could change,
    /// each a job and its partition QOS, for [`QuotaLedger::restore`].
    pub(super) fn snapshot<'a>(
        &self,
        starts: impl IntoIterator<Item = (&'a Job, Option<&'a str>)>,
    ) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for (job, partition_qos) in starts {
            for (taken, tallies, name) in [
                (&mut snapshot.qos, &self.qos, job.spec.qos.as_deref()),
                (&mut snapshot.accounts, &self.accounts, account_of(job)),
                (
                    &mut snapshot.partition_qos,
                    &self.partition_qos,
                    partition_qos,
                ),
            ] {
                if let Some(name) = name.filter(|n| !taken.iter().any(|(t, _)| t == n)) {
                    taken.push((name.to_owned(), tallies.get(name).cloned()));
                }
            }
        }
        snapshot
    }

    /// Puts back the tallies `snapshot` took, undoing every start charged since.
    pub(super) fn restore(&mut self, snapshot: Snapshot) {
        for (tallies, taken) in [
            (&mut self.qos, snapshot.qos),
            (&mut self.accounts, snapshot.accounts),
            (&mut self.partition_qos, snapshot.partition_qos),
        ] {
            for (name, tally) in taken {
                match tally {
                    Some(tally) => tallies.insert(name, tally),
                    None => tallies.remove(&name),
                };
            }
        }
    }

    fn add(&mut self, job: &Job) {
        // A borrowed run (`idle_fill` stamped) is outside every quota aggregate:
        // counted, it would block its team's legitimate jobs at a gate, where
        // reclaim never sees them (§7, D1). A job reclaimable only through
        // `idle_fill_preemptable` is inside its quota and counts in full (§4.1).
        if job.idle_fill {
            return;
        }
        let running = job.occupies_resources();
        let submitted = is_submitted(job);
        if !running && !submitted {
            return;
        }
        // As the gates look them up: an empty QOS name is its own scope, an empty account is none.
        for (scopes, name) in [
            (&mut self.qos, job.spec.qos.as_deref()),
            (&mut self.accounts, account_of(job)),
        ] {
            if let Some(name) = name {
                update(scopes, name, |scope| scope.add(job, running, submitted));
            }
        }
    }
}

/// Some scopes' tallies as they stood, `None` for a scope that had none.
#[derive(Default)]
pub(super) struct Snapshot {
    qos: Vec<(String, Option<ScopeTally>)>,
    accounts: Vec<(String, Option<ScopeTally>)>,
    partition_qos: Vec<(String, Option<ScopeTally>)>,
}

/// Runs `f` on the tally under `name`, allocating the name only when it is new.
fn update<T: Default>(tallies: &mut HashMap<String, T>, name: &str, f: impl FnOnce(&mut T)) {
    match tallies.get_mut(name) {
        Some(tally) => f(tally),
        None => f(tallies.entry(name.to_owned()).or_default()),
    }
}

impl ScopeTally {
    fn add(&mut self, job: &Job, running: bool, submitted: bool) {
        if running {
            self.running.add(job);
        }
        update(&mut self.users, &job.spec.user, |user| {
            if running {
                user.running.add(job);
                user.running_jobs += 1;
            }
            if submitted {
                user.submitted.push(job.job_id);
            }
        });
    }

    fn start(&mut self, job: &Job, scope_nodes: &[String], user_nodes: &[String]) {
        self.running.add_on(job, scope_nodes);
        update(&mut self.users, &job.spec.user, |user| {
            user.running.add_on(job, user_nodes);
            user.running_jobs += 1;
        });
    }
}

/// What one user and their whole scope already use, for a gate checking a job.
pub(super) struct QuotaView<'a> {
    scope: Option<&'a ScopeTally>,
    user: Option<&'a UserTally>,
}

impl<'a> QuotaView<'a> {
    fn of(scope: Option<&'a ScopeTally>, user: &str) -> Self {
        Self {
            scope,
            user: scope.and_then(|s| s.users.get(user)),
        }
    }

    pub(super) fn user_running_jobs(&self) -> u32 {
        self.user.map_or(0, |u| u.running_jobs)
    }

    /// Only earlier-submitted jobs (lower id) count, so a later job never makes an
    /// earlier, within-limit job retroactively blocked.
    pub(super) fn user_submitted_before(&self, job_id: JobId) -> u32 {
        self.user
            .map_or(0, |u| u.submitted.partition_point(|id| *id < job_id) as u32)
    }

    pub(super) fn user_running_tres(&self) -> TresRecord {
        self.user.map_or_else(
            || RunningTresAccumulator::default().tres(),
            |u| u.running.tres(),
        )
    }

    pub(super) fn scope_running_tres(&self) -> TresRecord {
        self.scope.map_or_else(
            || RunningTresAccumulator::default().tres(),
            |s| s.running.tres(),
        )
    }

    /// Nodes the scope's running jobs occupy.
    pub(super) fn occupied(&self) -> impl Iterator<Item = &'a str> {
        self.scope.into_iter().flat_map(|s| s.running.nodes())
    }

    pub(super) fn occupies(&self, node: &str) -> bool {
        self.scope.is_some_and(|s| s.running.occupies(node))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{occupied_nodes, sum_running_tres};
    use super::*;
    use spur_core::accounting::TresType;
    use spur_core::job::{JobSpec, JobState};

    fn job(id: JobId, user: &str, qos: &str, state: JobState, nodes: &[&str]) -> Job {
        let mut job = Job::new(
            id,
            JobSpec {
                name: "j".into(),
                user: user.into(),
                qos: Some(qos.into()),
                account: Some("acct".into()),
                num_nodes: nodes.len().max(1) as u32,
                num_tasks: 1,
                cpus_per_task: 2,
                ..Default::default()
            },
        );
        job.state = state;
        job.allocated_nodes = nodes.iter().map(|n| (*n).to_string()).collect();
        job
    }

    #[test]
    fn counts_running_and_earlier_submitted_jobs_per_user() {
        let jobs = [
            job(1, "alice", "team", JobState::Running, &["n1"]),
            job(2, "alice", "team", JobState::Running, &["n1", "n2"]),
            job(3, "bob", "team", JobState::Running, &["n3"]),
            job(4, "alice", "team", JobState::Pending, &[]),
            job(5, "alice", "team", JobState::Completed, &["n4"]),
        ];
        let ledger = QuotaLedger::from_jobs(&jobs);
        let alice = ledger.qos("team", "alice");
        assert_eq!(alice.user_running_jobs(), 2);
        assert_eq!(alice.user_submitted_before(2), 1);
        assert_eq!(alice.user_submitted_before(4), 2);
        assert_eq!(alice.user_submitted_before(9), 3);
        assert_eq!(alice.user_running_tres().get(TresType::Node), 2);
        assert_eq!(alice.user_running_tres().get(TresType::Cpu), 4);
        assert_eq!(alice.scope_running_tres().get(TresType::Node), 3);
        let mut occupied: Vec<_> = alice.occupied().collect();
        occupied.sort();
        assert_eq!(occupied, ["n1", "n2", "n3"]);
        assert_eq!(ledger.account("acct", "bob").user_running_jobs(), 1);
    }

    #[test]
    fn borrowed_runs_are_outside_every_aggregate() {
        let mut borrowed = job(1, "alice", "team", JobState::Running, &["n1"]);
        borrowed.idle_fill = true;
        let sibling = job(2, "alice", "team", JobState::Running, &["n2"]);
        let ledger = QuotaLedger::from_jobs([&borrowed, &sibling]);
        for usage in [ledger.qos("team", "alice"), ledger.account("acct", "alice")] {
            assert_eq!(usage.user_running_jobs(), 1);
            assert_eq!(usage.user_submitted_before(3), 1);
            for tres in [usage.user_running_tres(), usage.scope_running_tres()] {
                assert_eq!(tres.get(TresType::Node), 1);
                assert_eq!(tres.get(TresType::Cpu), 2);
            }
            assert_eq!(usage.occupied().collect::<Vec<_>>(), ["n2"]);
        }
    }

    /// A pending job in "team" and "acct" without a placement, and the nodes it starts on.
    fn starting(id: JobId, user: &str, nodes: &[&str]) -> (Job, Vec<String>) {
        let mut job = job(id, user, "team", JobState::Pending, nodes);
        let placed = std::mem::take(&mut job.allocated_nodes);
        (job, placed)
    }

    #[test]
    fn a_start_charges_its_qos_and_account() {
        let running = job(1, "alice", "team", JobState::Running, &["n1"]);
        let (start, placed) = starting(2, "alice", &["n2", "n3"]);
        let mut ledger = QuotaLedger::from_jobs([&running, &start]);
        ledger.charge_start(&start, &placed, None);
        for usage in [ledger.qos("team", "alice"), ledger.account("acct", "alice")] {
            assert_eq!(usage.user_running_jobs(), 2);
            assert_eq!(usage.user_submitted_before(3), 2);
            for tres in [usage.user_running_tres(), usage.scope_running_tres()] {
                assert_eq!(tres.get(TresType::Node), 3);
                assert_eq!(tres.get(TresType::Cpu), 4);
            }
            let mut occupied: Vec<_> = usage.occupied().collect();
            occupied.sort();
            assert_eq!(occupied, ["n1", "n2", "n3"]);
        }
    }

    #[test]
    fn a_start_counts_a_node_its_scope_already_occupies_once() {
        let running = job(1, "alice", "team", JobState::Running, &["n1"]);
        let mut ledger = QuotaLedger::from_jobs([&running]);
        for (start, placed) in [
            starting(2, "alice", &["n1", "n2"]),
            starting(3, "bob", &["n2"]),
        ] {
            ledger.charge_start(&start, &placed, None);
        }
        for (alice, bob) in [
            (ledger.qos("team", "alice"), ledger.qos("team", "bob")),
            (
                ledger.account("acct", "alice"),
                ledger.account("acct", "bob"),
            ),
        ] {
            assert_eq!(alice.scope_running_tres().get(TresType::Node), 2);
            assert_eq!(alice.scope_running_tres().get(TresType::Cpu), 6);
            assert_eq!(alice.user_running_tres().get(TresType::Node), 2);
            assert_eq!(alice.user_running_jobs(), 2);
            assert_eq!(bob.user_running_tres().get(TresType::Node), 1);
            assert_eq!(bob.user_running_jobs(), 1);
        }
    }

    #[test]
    fn a_borrowed_start_adds_no_node_to_its_qos() {
        let (mut borrowed, placed) = starting(1, "alice", &["n1"]);
        borrowed.idle_fill = true;
        let mut ledger = QuotaLedger::from_jobs([&borrowed]);
        ledger.charge_start(&borrowed, &placed, None);

        let qos = ledger.qos("team", "alice");
        assert!(!qos.occupies("n1"));
        assert_eq!(qos.occupied().count(), 0);
        assert_eq!(qos.scope_running_tres().get(TresType::Node), 0);
        assert_eq!(qos.scope_running_tres().get(TresType::Cpu), 2);
        assert_eq!(qos.user_running_jobs(), 1);
        assert_eq!(qos.user_running_tres().get(TresType::Node), 1);
        assert_eq!(qos.user_running_tres().get(TresType::Cpu), 2);

        let account = ledger.account("acct", "alice");
        assert!(account.occupies("n1"));
        assert_eq!(account.user_running_jobs(), 1);
        for tres in [account.user_running_tres(), account.scope_running_tres()] {
            assert_eq!(tres.get(TresType::Node), 1);
            assert_eq!(tres.get(TresType::Cpu), 2);
        }
    }

    /// What a gate reads for `user` in `scope`, by scanning the whole job table.
    struct Scanned {
        running_jobs: u32,
        submitted_before: u32,
        user_tres: TresRecord,
        scope_tres: TresRecord,
        occupied: Vec<String>,
    }

    fn scan(
        jobs: &HashMap<JobId, Job>,
        scope_of: impl Fn(&Job) -> Option<&str>,
        scope: &str,
        user: &str,
        before: JobId,
    ) -> Scanned {
        let in_scope = |j: &Job| !j.idle_fill && scope_of(j) == Some(scope);
        let mine = |j: &Job| in_scope(j) && j.spec.user == user;
        let mut occupied: Vec<String> = occupied_nodes(jobs, in_scope).into_iter().collect();
        occupied.sort();
        Scanned {
            running_jobs: jobs
                .values()
                .filter(|j| j.occupies_resources() && mine(j))
                .count() as u32,
            submitted_before: jobs
                .values()
                .filter(|j| {
                    j.job_id < before
                        && matches!(j.state, JobState::Pending | JobState::Running)
                        && mine(j)
                })
                .count() as u32,
            user_tres: sum_running_tres(jobs, mine),
            scope_tres: sum_running_tres(jobs, in_scope),
            occupied,
        }
    }

    fn assert_matches(usage: &QuotaView<'_>, want: &Scanned, before: JobId, what: &str) {
        let mut occupied: Vec<&str> = usage.occupied().collect();
        occupied.sort();
        assert_eq!(
            usage.user_running_jobs(),
            want.running_jobs,
            "{what}: running"
        );
        assert_eq!(
            usage.user_submitted_before(before),
            want.submitted_before,
            "{what}: submitted"
        );
        assert_eq!(
            usage.user_running_tres(),
            want.user_tres,
            "{what}: user tres"
        );
        assert_eq!(
            usage.scope_running_tres(),
            want.scope_tres,
            "{what}: scope tres"
        );
        assert_eq!(occupied, want.occupied, "{what}: occupied");
    }

    #[test]
    fn matches_a_scan_of_the_job_table() {
        let mut seed: u64 = 0x5eed;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let qoses = [None, Some(""), Some("a"), Some("b"), Some("c")];
        let accounts = [None, Some(""), Some("x"), Some("y")];
        let users = ["u0", "u1", "u2", "u3"];
        let states = [
            JobState::Pending,
            JobState::Running,
            JobState::Running,
            JobState::Completing,
            JobState::Completed,
        ];
        let mut jobs: HashMap<JobId, Job> = HashMap::new();
        for id in 1..=400 {
            let mut j = job(
                id,
                users[next(4) as usize],
                "a",
                states[next(5) as usize],
                &[],
            );
            j.spec.qos = qoses[next(5) as usize].map(str::to_owned);
            j.spec.account = accounts[next(4) as usize].map(str::to_owned);
            j.spec.num_nodes = 1 + next(3) as u32;
            j.spec.cpus_per_task = 1 + next(8) as u32;
            j.idle_fill = next(10) == 0;
            // Some running jobs have no recorded placement, and some pending ones
            // already hold the placement they are being dispatched to.
            let placed = match j.state {
                JobState::Running | JobState::Completing => next(5) != 0,
                JobState::Pending => next(4) == 0,
                _ => false,
            };
            if placed {
                j.allocated_nodes = (0..j.spec.num_nodes)
                    .map(|_| format!("n{}", next(64)))
                    .collect();
            }
            jobs.insert(id, j);
        }

        let ledger = QuotaLedger::from_jobs(jobs.values());
        for before in [1, 137, 401] {
            // "u9" holds no jobs, so scopes that exist are also read for a user they lack.
            for user in users.into_iter().chain(["u9"]) {
                for scope in ["", "a", "b", "c", "z"] {
                    let want = scan(&jobs, |j| j.spec.qos.as_deref(), scope, user, before);
                    let what = format!("qos {scope:?} {user} before {before}");
                    assert_matches(&ledger.qos(scope, user), &want, before, &what);
                }
                for scope in ["x", "y", "z"] {
                    let want = scan(&jobs, |j| j.spec.account.as_deref(), scope, user, before);
                    let what = format!("account {scope} {user} before {before}");
                    assert_matches(&ledger.account(scope, user), &want, before, &what);
                }
            }
        }
    }
}
