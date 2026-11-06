// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use spur_core::job::{Job, JobId, JobState};
use spur_core::preemption_usage::{
    PreemptionInitiator as Initiator, PreemptionUsage, UnusedReason as Reason,
};
use spur_core::resource::ResourceAllocations;
use spur_core::wal::WalOperation;

use super::ClusterManager;
use crate::raft::ClientResponse;

type Allocations = BTreeMap<String, ResourceAllocations>;
type Outcome = Option<(Reason, Initiator)>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum LaunchOutcome {
    Used,
    Unused(Reason, Initiator),
}

impl From<Outcome> for LaunchOutcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            None => Self::Used,
            Some((reason, initiator)) => Self::Unused(reason, initiator),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct UsageLedger {
    pub cluster: PreemptionUsage,
    pub users: BTreeMap<String, PreemptionUsage>,
    requests: BTreeMap<JobId, BTreeMap<u32, Request>>,
    victims: BTreeMap<JobId, BTreeMap<u32, Victim>>,
    #[serde(skip)]
    at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Request {
    user: String,
    victims: BTreeSet<(JobId, u32)>,
    activated: Option<Allocations>,
    outcome: Option<LaunchOutcome>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Victim {
    run_attempt: u32,
    owner: JobId,
    owner_attempt: u32,
    allocations: Allocations,
    freed: BTreeSet<String>,
    opened: bool,
    settled: bool,
}

fn allocations(job: &Job) -> Allocations {
    job.allocated_nodes
        .iter()
        .map(|node| {
            let alloc = job.per_node_alloc.get(node).cloned().unwrap_or_else(|| {
                let n = job.allocated_nodes.len().max(1) as u32;
                job.allocated_resources
                    .as_ref()
                    .map(|a| {
                        ResourceAllocations::with_scalar(a.cpus / n, a.memory_mb / u64::from(n))
                    })
                    .unwrap_or_default()
            });
            (node.clone(), alloc)
        })
        .collect()
}

// Scalar units have no identity. Consume a bounded budget in victim-id order;
// device-backed victims require matching device identities instead of CPU overlap.
fn consume(freed: &Allocations, available: &mut Allocations) -> bool {
    let mut used = false;
    for (node, victim) in freed {
        let Some(owner) = available.get_mut(node) else {
            continue;
        };
        if victim.has_devices() {
            if victim.generation != owner.generation {
                continue;
            }
            for (kind, devices) in &victim.devices {
                let Some(owner_devices) = owner.devices.get_mut(kind) else {
                    continue;
                };
                for device in devices {
                    if let Some(other) = owner_devices
                        .iter_mut()
                        .find(|d| d.device_id == device.device_id)
                    {
                        let n = device.count.min(other.count);
                        other.count -= n;
                        used |= n > 0;
                    }
                }
            }
        } else {
            let cpus = victim.cpus.min(owner.cpus);
            let memory = victim.memory_mb.min(owner.memory_mb);
            owner.cpus -= cpus;
            owner.memory_mb -= memory;
            used |= cpus > 0 || memory > 0;
        }
    }
    used
}

#[derive(Clone)]
struct ObservedJob {
    job_id: JobId,
    state: JobState,
    run_attempt: u32,
    user: String,
    cancelled_by: Option<spur_core::end_reason::CancelProvenance>,
    allocations: Allocations,
    node_completions: BTreeSet<String>,
}

impl From<&Job> for ObservedJob {
    fn from(job: &Job) -> Self {
        Self {
            job_id: job.job_id,
            state: job.state,
            run_attempt: job.run_attempt,
            user: job.spec.user.clone(),
            cancelled_by: job.cancelled_by.clone(),
            allocations: allocations(job),
            node_completions: job.node_completions.keys().cloned().collect(),
        }
    }
}

impl UsageLedger {
    pub(super) fn tracks_request(&self, id: JobId) -> bool {
        self.requests.contains_key(&id)
    }

    fn take_victim(&mut self, id: JobId, run_attempt: u32) -> Option<Victim> {
        let runs = self.victims.get_mut(&id)?;
        let victim = runs.remove(&run_attempt);
        if runs.is_empty() {
            self.victims.remove(&id);
        }
        victim
    }

    fn request_mut(&mut self, victim: &Victim) -> Option<&mut Request> {
        self.requests
            .get_mut(&victim.owner)?
            .get_mut(&victim.owner_attempt)
    }

    fn count(&mut self, victim: &Victim, jobs: &mut HashMap<JobId, Job>, outcome: Option<Outcome>) {
        let Some(request) = self.request_mut(victim) else {
            return;
        };
        let user = request.user.clone();
        for counts in [&mut self.cluster, self.users.entry(user).or_default()] {
            match outcome {
                None => counts.open_one(),
                Some(outcome) => counts.settle(outcome),
            }
        }
        if let Some(job) = jobs.get_mut(&victim.owner) {
            if self.at.is_some() {
                job.preemption_usage_updated_at = self.at;
            }
            match outcome {
                None => job.preemption_usage.open_one(),
                Some(outcome) => job.preemption_usage.settle(outcome),
            }
        }
    }

    fn register(&mut self, victim: &ObservedJob, owner: &ObservedJob, attempt: u32) {
        if self
            .victims
            .get(&victim.job_id)
            .is_some_and(|runs| runs.contains_key(&victim.run_attempt))
        {
            return;
        }
        let request = self
            .requests
            .entry(owner.job_id)
            .or_default()
            .entry(attempt)
            .or_insert_with(|| Request {
                user: owner.user.clone(),
                outcome: if owner.state == JobState::Cancelled {
                    Some(LaunchOutcome::Unused(
                        Reason::Cancelled,
                        Initiator::from_cancel(owner.cancelled_by.as_ref()),
                    ))
                } else if owner.state.is_finalized() || owner.run_attempt > attempt {
                    Some(LaunchOutcome::Unused(
                        Reason::EndedOther,
                        Initiator::Unknown,
                    ))
                } else {
                    None
                },
                ..Default::default()
            });
        request.victims.insert((victim.job_id, victim.run_attempt));
        self.victims.entry(victim.job_id).or_default().insert(
            victim.run_attempt,
            Victim {
                run_attempt: victim.run_attempt,
                owner: owner.job_id,
                owner_attempt: attempt,
                allocations: victim.allocations.clone(),
                ..Default::default()
            },
        );
    }

    fn release(
        &mut self,
        before: &ObservedJob,
        after: Option<&ObservedJob>,
        jobs: &mut HashMap<JobId, Job>,
    ) {
        let Some(mut victim) = self.take_victim(before.job_id, before.run_attempt) else {
            return;
        };
        let finished =
            after.is_none_or(|job| job.run_attempt != victim.run_attempt || !job.state.is_active());
        for node in victim.allocations.keys() {
            if finished || after.is_some_and(|job| job.node_completions.contains(node)) {
                victim.freed.insert(node.clone());
            }
        }
        if !victim.opened && !victim.freed.is_empty() {
            victim.opened = true;
            self.count(&victim, jobs, None);
            if let Some(outcome) = self.request_mut(&victim).and_then(|r| r.outcome) {
                // Capacity freed after the launch confirmation could not have served it.
                self.count(
                    &victim,
                    jobs,
                    Some(Some(match outcome {
                        LaunchOutcome::Used => (Reason::StartedElsewhere, Initiator::System),
                        LaunchOutcome::Unused(reason, initiator) => (reason, initiator),
                    })),
                );
                victim.settled = true;
            }
        }
        if finished && (!victim.opened || victim.settled) {
            self.remove_victim(before.job_id, &victim);
        } else {
            self.victims
                .entry(before.job_id)
                .or_default()
                .insert(victim.run_attempt, victim);
        }
    }

    fn remove_victim(&mut self, id: JobId, victim: &Victim) {
        if let Some(attempts) = self.requests.get_mut(&victim.owner) {
            if let Some(request) = attempts.get_mut(&victim.owner_attempt) {
                request.victims.remove(&(id, victim.run_attempt));
                if request.victims.is_empty() {
                    attempts.remove(&victim.owner_attempt);
                }
            }
            if attempts.is_empty() {
                self.requests.remove(&victim.owner);
            }
        }
    }

    fn settle(
        &mut self,
        owner: JobId,
        attempt: u32,
        outcome: Outcome,
        jobs: &mut HashMap<JobId, Job>,
    ) {
        let Some(request) = self
            .requests
            .get_mut(&owner)
            .and_then(|r| r.get_mut(&attempt))
        else {
            return;
        };
        if request.outcome.is_some() {
            return;
        }
        request.outcome = Some(outcome.into());
        let mut available = request.activated.clone().unwrap_or_default();
        let ids: Vec<_> = request.victims.iter().copied().collect();
        for (id, run_attempt) in ids {
            let Some(mut victim) = self.take_victim(id, run_attempt) else {
                continue;
            };
            if victim.opened && !victim.settled {
                let freed = victim
                    .allocations
                    .iter()
                    .filter(|(node, _)| victim.freed.contains(*node))
                    .map(|(node, alloc)| (node.clone(), alloc.clone()))
                    .collect();
                let verdict = outcome.or_else(|| {
                    (!consume(&freed, &mut available))
                        .then_some((Reason::StartedElsewhere, Initiator::System))
                });
                self.count(&victim, jobs, Some(verdict));
                victim.settled = true;
            }
            if victim.settled && victim.freed.len() == victim.allocations.len() {
                self.remove_victim(id, &victim);
            } else {
                self.victims
                    .entry(id)
                    .or_default()
                    .insert(victim.run_attempt, victim);
            }
        }
    }

    fn activate(&mut self, job: &ObservedJob) {
        if let Some(request) = self
            .requests
            .get_mut(&job.job_id)
            .and_then(|r| r.get_mut(&job.run_attempt))
        {
            request.activated = Some(job.allocations.clone());
        }
    }

    fn end_owner(
        &mut self,
        id: JobId,
        outcome: (Reason, Initiator),
        defer_activated: bool,
        jobs: &mut HashMap<JobId, Job>,
    ) {
        let attempts: Vec<_> = self
            .requests
            .get(&id)
            .into_iter()
            .flat_map(|a| a.iter())
            .filter(|(_, r)| !defer_activated || r.activated.is_none())
            .map(|(&a, _)| a)
            .collect();
        for attempt in attempts {
            self.settle(id, attempt, Some(outcome), jobs);
        }
    }
}

struct Observation {
    before: BTreeMap<JobId, ObservedJob>,
    registration: Option<(ObservedJob, ObservedJob, u32)>,
}

impl ClusterManager {
    pub fn preemption_usage(&self) -> PreemptionUsage {
        self.preemption_usage.read().cluster.clone()
    }

    pub fn user_preemption_usage(
        &self,
        permits: impl Fn(&str) -> bool,
    ) -> Vec<(String, PreemptionUsage)> {
        self.preemption_usage
            .read()
            .users
            .iter()
            .filter(|(user, _)| permits(user))
            .take(1001)
            .map(|(user, usage)| (user.clone(), usage.clone()))
            .collect()
    }

    pub(crate) fn confirm_preemption_release(
        &self,
        job_id: JobId,
        attempt: u32,
        released: bool,
    ) -> anyhow::Result<()> {
        let pending = self
            .preemption_usage
            .read()
            .requests
            .get(&job_id)
            .and_then(|a| a.get(&attempt))
            .is_some_and(|r| r.outcome.is_none());
        if !pending {
            return Ok(());
        }
        let op = WalOperation::preemption_release(job_id, attempt, released);
        self.propose(op)?;
        Ok(())
    }

    fn observe_preemption_before(&self, op: &WalOperation) -> Observation {
        let jobs = self.jobs.read();
        let usage = self.preemption_usage.read();
        let registration = match op {
            WalOperation::JobPreemptRequeue {
                job_id,
                handoff: true,
                preempted_by: Some(owner),
                preemptor_run_attempt,
                ..
            }
            | WalOperation::JobPreemptCancel {
                job_id,
                preempted_by: Some(owner),
                preemptor_run_attempt,
                ..
            } => jobs.get(job_id).zip(jobs.get(owner)).map(|(v, o)| {
                (
                    v.into(),
                    o.into(),
                    preemptor_run_attempt.unwrap_or_else(|| o.run_attempt.saturating_add(1)),
                )
            }),
            WalOperation::JobPreemptSignaled { job_id, pending } => jobs
                .get(job_id)
                .zip(jobs.get(&pending.preempted_by))
                .map(|(v, o)| {
                    (
                        v.into(),
                        o.into(),
                        pending
                            .preemptor_run_attempt
                            .unwrap_or_else(|| o.run_attempt.saturating_add(1)),
                    )
                }),
            _ => None,
        };
        let ids: Vec<JobId> = match op {
            WalOperation::JobStateChange { job_id, .. }
            | WalOperation::JobStart { job_id, .. }
            | WalOperation::JobComplete { job_id, .. }
            | WalOperation::JobNodeComplete { job_id, .. }
            | WalOperation::JobCancelSignaled { job_id, .. }
            | WalOperation::JobTimeLimitExpired { job_id, .. }
            | WalOperation::JobDispatchBackoff { job_id, .. }
            | WalOperation::JobPriorityChange { job_id, .. }
            | WalOperation::JobPreemptCancel { job_id, .. }
            | WalOperation::JobPreemptRequeue { job_id, .. }
            | WalOperation::JobPreemptSignaled { job_id, .. }
            | WalOperation::JobUserRequeue { job_id, .. }
            | WalOperation::JobEvict { job_id, .. } => vec![*job_id],
            WalOperation::EvictTerminalJobs { job_ids, .. } => job_ids.clone(),
            WalOperation::NodeStateChange { name, .. } | WalOperation::NodeRemove { name, .. } => {
                usage
                    .victims
                    .keys()
                    .chain(usage.requests.keys())
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter(|id| jobs.get(id).is_some_and(|j| j.is_held_on(name)))
                    .collect()
            }
            _ => Vec::new(),
        };
        let before = ids
            .into_iter()
            .filter(|id| {
                usage.victims.contains_key(id)
                    || usage.requests.contains_key(id)
                    || registration.is_some()
            })
            .filter_map(|id| jobs.get(&id).map(|job| (id, job.into())))
            .collect();
        Observation {
            before,
            registration,
        }
    }

    pub(super) fn apply_operation(&self, op: &WalOperation) -> ClientResponse {
        if let WalOperation::EvictTerminalJobs {
            job_ids,
            preemption_release: Some(release),
            ..
        } = op
        {
            if !job_ids.is_empty() {
                return self.apply_job_operation(op);
            }
            let job_id = &release.job_id;
            let attempt = &release.run_attempt;
            let released = &release.released;
            let mut jobs = self.jobs.write();
            let mut usage = self.preemption_usage.write();
            usage.at = op.occurred_at();
            {
                let outcome = if *released {
                    None
                } else {
                    jobs.get(job_id)
                        .filter(|j| j.run_attempt <= *attempt && j.cancelled_by.is_some())
                        .map(|j| {
                            (
                                Reason::Cancelled,
                                Initiator::from_cancel(j.cancelled_by.as_ref()),
                            )
                        })
                        .or(Some((Reason::LaunchFailed, Initiator::System)))
                };
                usage.settle(*job_id, *attempt, outcome, &mut jobs);
            }
            return ClientResponse::default();
        }
        if let WalOperation::EvictTerminalJobs {
            job_ids, before, ..
        } = op
        {
            let mut jobs = self.jobs.write();
            let mut usage = self.preemption_usage.write();
            usage.at = op.occurred_at();
            for id in job_ids {
                if jobs.get(id).is_some_and(|j| {
                    j.state.is_finalized()
                        && before.is_none_or(|cutoff| {
                            j.end_time.is_some_and(|end| end < cutoff)
                                && j.preemption_usage_updated_at
                                    .is_none_or(|updated| updated < cutoff)
                        })
                }) {
                    usage.end_owner(
                        *id,
                        (Reason::EndedOther, Initiator::Unknown),
                        false,
                        &mut jobs,
                    );
                }
            }
        }
        let observation = self.observe_preemption_before(op);
        if observation.before.is_empty() && observation.registration.is_none() {
            return self.apply_job_operation(op);
        }
        let response = self.apply_job_operation(op);
        let mut jobs = self.jobs.write();
        let mut usage = self.preemption_usage.write();
        usage.at = op.occurred_at();
        if response.preempt_applied || response.preempt_signal_applied {
            if let Some((victim, owner, attempt)) = &observation.registration {
                usage.register(victim, owner, *attempt);
            }
        }
        for (id, before) in observation.before {
            let after = jobs.get(&id).map(ObservedJob::from);
            usage.release(&before, after.as_ref(), &mut jobs);
            let Some(after) = after else {
                usage.end_owner(
                    id,
                    (Reason::EndedOther, Initiator::Unknown),
                    false,
                    &mut jobs,
                );
                continue;
            };
            if response.state_change_applied && after.state == JobState::Running {
                usage.activate(&after);
            }
            if after.run_attempt > before.run_attempt {
                let previous: Vec<_> = usage
                    .requests
                    .get(&id)
                    .into_iter()
                    .flat_map(|r| r.keys())
                    .copied()
                    .filter(|a| *a < after.run_attempt)
                    .collect();
                for attempt in previous {
                    usage.settle(
                        id,
                        attempt,
                        Some((Reason::EndedOther, Initiator::Unknown)),
                        &mut jobs,
                    );
                }
            }
            if after.state == JobState::Cancelled || after.cancelled_by.is_some() {
                usage.end_owner(
                    id,
                    (
                        Reason::Cancelled,
                        Initiator::from_cancel(after.cancelled_by.as_ref()),
                    ),
                    true,
                    &mut jobs,
                );
            } else if after.state.is_finalized()
                || (before.state.is_active() && after.state == JobState::Pending)
            {
                usage.end_owner(
                    id,
                    (Reason::EndedOther, Initiator::Unknown),
                    true,
                    &mut jobs,
                );
            }
            match op {
                WalOperation::JobDispatchBackoff { run_attempt, .. }
                    if before.state == JobState::Pending
                        && after.state == JobState::Pending
                        && (*run_attempt == 0 || *run_attempt >= before.run_attempt) =>
                {
                    usage.settle(
                        id,
                        *run_attempt,
                        Some((Reason::LaunchFailed, Initiator::System)),
                        &mut jobs,
                    )
                }
                WalOperation::JobPriorityChange {
                    run_attempt: Some(attempt),
                    ..
                } if before.state == JobState::Pending
                    && after.state == JobState::Pending
                    && (*attempt == 0 || *attempt >= before.run_attempt) =>
                {
                    usage.settle(
                        id,
                        *attempt,
                        Some((Reason::LaunchFailed, Initiator::System)),
                        &mut jobs,
                    )
                }
                _ => {}
            }
        }
        response
    }
}

#[cfg(test)]
#[path = "preemption_usage_tests.rs"]
mod tests;
