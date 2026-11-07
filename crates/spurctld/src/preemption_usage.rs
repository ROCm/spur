// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use spur_core::job::{Job, JobId, JobState};
use spur_core::preemption_usage::{
    PreemptionClaim, PreemptionInitiator as Initiator, PreemptionUsage, UnusedReason as Reason,
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
    requests: BTreeMap<JobId, BTreeMap<u64, Request>>,
    high_water: BTreeMap<JobId, u64>,
    current: BTreeMap<JobId, u64>,
    victims: BTreeMap<JobId, BTreeMap<u32, Victim>>,
    #[serde(skip)]
    at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Request {
    closed: bool,
    closure_cause: Option<(Reason, Initiator)>,
    run_attempt: Option<u32>,
    reserved: Option<Allocations>,
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
    owner_attempt: u64,
    allocations: Allocations,
    freed: BTreeSet<String>,
    opened: bool,
    settled: bool,
}

fn allocations(job: &Job) -> Allocations {
    allocation_slices(
        &job.allocated_nodes,
        job.allocated_resources.as_ref(),
        &job.per_node_alloc,
    )
}

fn allocation_slices(
    nodes: &[String],
    total: Option<&ResourceAllocations>,
    per_node: &HashMap<String, ResourceAllocations>,
) -> Allocations {
    let n = nodes.len().max(1) as u32;
    nodes
        .iter()
        .map(|node| {
            let alloc = per_node.get(node).cloned().unwrap_or_else(|| {
                // Aggregate device IDs cannot identify a node-local slice.
                total
                    .filter(|a| !a.has_devices())
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
    cancelled_by: Option<spur_core::end_reason::CancelProvenance>,
}

impl From<&Job> for ObservedJob {
    fn from(job: &Job) -> Self {
        Self {
            job_id: job.job_id,
            state: job.state,
            run_attempt: job.run_attempt,
            cancelled_by: job.cancelled_by.clone(),
        }
    }
}

struct Registration {
    victim: ObservedJob,
    owner: ObservedJob,
    user: String,
    claim: PreemptionClaim,
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

    fn register(&mut self, registration: Registration) {
        let Registration {
            victim,
            owner,
            user,
            claim,
        } = registration;
        let attempt = claim.observation_id;
        if self
            .victims
            .get(&victim.job_id)
            .is_some_and(|runs| runs.contains_key(&victim.run_attempt))
        {
            return;
        }
        let high_water = self.high_water.entry(owner.job_id).or_default();
        let fresh = attempt > *high_water;
        *high_water = (*high_water).max(attempt);
        if fresh && !owner.state.is_finalized() {
            self.current.insert(owner.job_id, attempt);
        }
        let request = self
            .requests
            .entry(owner.job_id)
            .or_default()
            .entry(attempt)
            .or_insert_with(|| Request {
                user,
                outcome: if owner.state == JobState::Cancelled {
                    Some(LaunchOutcome::Unused(
                        Reason::Cancelled,
                        Initiator::from_cancel(owner.cancelled_by.as_ref()),
                    ))
                } else if owner.state.is_finalized() || !fresh {
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
                allocations: claim.allocations,
                ..Default::default()
            },
        );
    }

    fn release(
        &mut self,
        before: &ObservedJob,
        after: Option<&ObservedJob>,
        completed_node: Option<&str>,
        jobs: &mut HashMap<JobId, Job>,
    ) {
        let Some(mut victim) = self.take_victim(before.job_id, before.run_attempt) else {
            return;
        };
        let finished =
            after.is_none_or(|job| job.run_attempt != victim.run_attempt || !job.state.is_active());
        if finished {
            victim.freed.extend(victim.allocations.keys().cloned());
        } else if let Some(node) =
            completed_node.filter(|node| victim.allocations.contains_key(*node))
        {
            victim.freed.insert(node.to_owned());
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
                    if self.current.get(&victim.owner) == Some(&victim.owner_attempt) {
                        self.current.remove(&victim.owner);
                    }
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
        attempt: u64,
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
        request.closed = true;
        if self.current.get(&owner) == Some(&attempt) {
            self.current.remove(&owner);
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

    fn reserve(&mut self, op: &WalOperation) {
        let WalOperation::JobStart {
            job_id,
            run_attempt,
            nodes,
            resources,
            per_node_alloc,
            preemption_observation,
            ..
        } = op
        else {
            return;
        };
        let observation = preemption_observation.unwrap_or(u64::from(*run_attempt));
        if let Some(request) = self
            .requests
            .get_mut(job_id)
            .and_then(|r| r.get_mut(&observation))
        {
            if request.outcome.is_none() {
                request.run_attempt = Some(*run_attempt);
                request.reserved = Some(allocation_slices(nodes, Some(resources), per_node_alloc));
            }
        }
    }

    fn activate(&mut self, job: &ObservedJob) {
        if let Some(requests) = self.requests.get_mut(&job.job_id) {
            for request in requests.values_mut() {
                if request.run_attempt == Some(job.run_attempt) && request.outcome.is_none() {
                    if let Some(reserved) = request.reserved.take() {
                        request.activated = Some(reserved);
                    }
                }
            }
        }
    }

    fn fail_run(&mut self, job_id: JobId, run_attempt: u32, jobs: &mut HashMap<JobId, Job>) {
        let observations: Vec<_> = self
            .requests
            .get(&job_id)
            .into_iter()
            .flat_map(|requests| requests.iter())
            .filter(|(_, request)| request.run_attempt == Some(run_attempt))
            .map(|(&id, _)| id)
            .collect();
        for id in observations {
            self.settle(
                job_id,
                id,
                Some((Reason::LaunchFailed, Initiator::System)),
                jobs,
            );
        }
    }

    fn end_owner(
        &mut self,
        id: JobId,
        outcome: (Reason, Initiator),
        defer_activated: bool,
        jobs: &mut HashMap<JobId, Job>,
    ) {
        self.current.remove(&id);
        if let Some(requests) = self.requests.get_mut(&id) {
            for request in requests.values_mut() {
                if !request.closed {
                    request.closed = true;
                    request.closure_cause = Some(outcome);
                }
            }
        }
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
    registration: Option<Registration>,
}

impl ClusterManager {
    pub(crate) fn preemption_observation(&self, job_id: JobId) -> Option<u64> {
        let usage = self.preemption_usage.read();
        usage.current.get(&job_id).copied().or_else(|| {
            usage
                .requests
                .get(&job_id)?
                .iter()
                .rev()
                .find(|(_, request)| !request.closed && request.outcome.is_none())
                .map(|(&id, _)| id)
        })
    }

    pub(crate) fn next_preemption_observation(&self, job_id: JobId) -> u64 {
        if let Some(current) = self.preemption_observation(job_id) {
            return current;
        }
        let usage = self.preemption_usage.read();
        usage.current.get(&job_id).copied().unwrap_or_else(|| {
            usage
                .high_water
                .get(&job_id)
                .copied()
                .unwrap_or(0)
                .max(
                    usage
                        .requests
                        .get(&job_id)
                        .and_then(|r| r.keys().next_back())
                        .copied()
                        .unwrap_or(0),
                )
                .saturating_add(1)
        })
    }

    pub(crate) fn preemption_claim(&self, victim: &Job, observation_id: u64) -> PreemptionClaim {
        PreemptionClaim {
            observation_id,
            allocations: allocations(victim),
        }
    }

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

    pub(crate) fn confirm_preemption_observation(
        &self,
        job_id: JobId,
        attempt: u32,
        observation_id: Option<u64>,
        released: bool,
    ) -> anyhow::Result<()> {
        let Some(observation) = observation_id else {
            return Ok(());
        };
        let pending = self
            .preemption_usage
            .read()
            .requests
            .get(&job_id)
            .and_then(|a| a.get(&observation))
            .is_some_and(|r| r.outcome.is_none());
        if !pending {
            return Ok(());
        }
        let mut op = WalOperation::preemption_release(job_id, attempt, released);
        if let WalOperation::EvictTerminalJobs {
            preemption_release: Some(release),
            ..
        } = &mut op
        {
            release.observation_id = Some(observation);
        }
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
                preemption_claim,
                ..
            }
            | WalOperation::JobPreemptCancel {
                job_id,
                preempted_by: Some(owner),
                preemptor_run_attempt,
                preemption_claim,
                ..
            } => jobs
                .get(job_id)
                .zip(jobs.get(owner))
                .map(|(v, o)| Registration {
                    victim: v.into(),
                    owner: o.into(),
                    user: o.spec.user.clone(),
                    claim: preemption_claim.clone().unwrap_or_else(|| PreemptionClaim {
                        observation_id: u64::from(
                            preemptor_run_attempt
                                .unwrap_or_else(|| o.run_attempt.saturating_add(1)),
                        ),
                        allocations: allocation_slices(
                            &v.allocated_nodes,
                            v.allocated_resources.as_ref(),
                            &HashMap::new(),
                        ),
                    }),
                }),
            WalOperation::JobPreemptSignaled { job_id, pending } => {
                jobs.get(job_id)
                    .zip(jobs.get(&pending.preempted_by))
                    .map(|(v, o)| Registration {
                        victim: v.into(),
                        owner: o.into(),
                        user: o.spec.user.clone(),
                        claim: pending.preemption_claim.clone().unwrap_or_else(|| {
                            PreemptionClaim {
                                observation_id: u64::from(
                                    pending
                                        .preemptor_run_attempt
                                        .unwrap_or_else(|| o.run_attempt.saturating_add(1)),
                                ),
                                allocations: allocation_slices(
                                    &v.allocated_nodes,
                                    v.allocated_resources.as_ref(),
                                    &HashMap::new(),
                                ),
                            }
                        }),
                    })
            }
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
                    || (matches!(op, WalOperation::EvictTerminalJobs { .. })
                        && usage.high_water.contains_key(id))
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
            let observation = release
                .observation_id
                .unwrap_or(u64::from(release.run_attempt));
            let mut jobs = self.jobs.write();
            let mut usage = self.preemption_usage.write();
            usage.at = op.occurred_at();
            // Requeue can clear or replace the job's cancellation provenance
            // before this observation's delayed launch acknowledgment arrives.
            let outcome = (!release.released).then(|| {
                usage
                    .requests
                    .get(&release.job_id)
                    .and_then(|requests| requests.get(&observation))
                    .and_then(|request| request.closure_cause)
                    .filter(|(reason, _)| *reason == Reason::Cancelled)
                    .unwrap_or((Reason::LaunchFailed, Initiator::System))
            });
            usage.settle(release.job_id, observation, outcome, &mut jobs);
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
            if let Some(registration) = observation.registration {
                usage.register(registration);
            }
        }
        if response.job_started {
            usage.reserve(op);
        }
        for (id, before) in observation.before {
            let after = jobs.get(&id).map(ObservedJob::from);
            let completed_node = match op {
                WalOperation::JobNodeComplete { node_name, .. }
                    if jobs
                        .get(&id)
                        .is_some_and(|job| job.node_completions.contains_key(node_name)) =>
                {
                    Some(node_name.as_str())
                }
                _ => None,
            };
            usage.release(&before, after.as_ref(), completed_node, &mut jobs);
            let Some(after) = after else {
                usage.high_water.remove(&id);
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
                    .flat_map(|r| r.iter())
                    .filter(|(_, request)| {
                        request.activated.is_none()
                            && request.run_attempt.is_some_and(|a| a < after.run_attempt)
                    })
                    .map(|(&id, _)| id)
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
                    usage.fail_run(id, *run_attempt, &mut jobs)
                }
                WalOperation::JobPriorityChange {
                    run_attempt: Some(attempt),
                    ..
                } if before.state == JobState::Pending
                    && after.state == JobState::Pending
                    && (*attempt == 0 || *attempt >= before.run_attempt) =>
                {
                    usage.fail_run(id, *attempt, &mut jobs)
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
