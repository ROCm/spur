// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Confirms a node is really gone before the leader marks it Down. Heartbeats
//! often reach the leader through a follower, so silence alone cannot tell a
//! dead node from a broken path or a struggling leader.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use spur_core::node::NodeSource;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::cluster::{ClusterManager, MarkDownPolicy, StaleNodes};
use crate::raft::JobFinalized;

/// Ceiling on one probe, connect included.
const NODE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds a pass that has to probe many nodes at once.
const MAX_CONCURRENT_PROBES: usize = 64;
/// Consecutive passes a node must fail to answer before it is marked Down.
const FAILED_PROBES_TO_MARK_DOWN: u32 = 2;
/// Tolerance on the gap between a node's two failed probes, so a pass that lands
/// a hair under one tick still counts.
const STRIKE_SPACING_SLACK: Duration = Duration::from_millis(500);
/// How often the leader repeats its warning about a silent node whose agent
/// still answers, and its error about a tripped silent-node limit.
const SILENT_NODE_WARN_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Debug)]
pub(crate) enum ProbeOutcome {
    Alive,
    Unreachable(String),
}

impl ProbeOutcome {
    fn is_alive(&self) -> bool {
        matches!(self, Self::Alive)
    }

    fn reason(&self) -> &str {
        match self {
            Self::Alive => "",
            Self::Unreachable(reason) => reason,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeTarget {
    pub(crate) node: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) last_heartbeat: Option<DateTime<Utc>>,
}

/// How a pass reaches a silent node, so the decision can be tested without one.
pub(crate) trait Prober: Clone + Send + Sync + 'static {
    fn probe(&self, target: ProbeTarget) -> impl Future<Output = ProbeOutcome> + Send;
}

/// Asks the node's own agent, at the address the leader uses to launch and
/// cancel its jobs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentProbe;

impl Prober for AgentProbe {
    async fn probe(&self, target: ProbeTarget) -> ProbeOutcome {
        probe_agent(target).await
    }
}

pub(crate) async fn probe_agent(target: ProbeTarget) -> ProbeOutcome {
    let Some(endpoint) = target.endpoint else {
        return ProbeOutcome::Unreachable("node has no address to probe".into());
    };
    let ping = crate::agent_client::ping_hostname(endpoint);
    match tokio::time::timeout(NODE_PROBE_TIMEOUT, ping).await {
        Ok(Ok(hostname)) if hostname == target.node => ProbeOutcome::Alive,
        Ok(Ok(hostname)) => ProbeOutcome::Unreachable(format!(
            "the agent at the node's address answered as {hostname}"
        )),
        Ok(Err(e)) => ProbeOutcome::Unreachable(e.to_string()),
        Err(_) => ProbeOutcome::Unreachable(format!("no answer within {NODE_PROBE_TIMEOUT:?}")),
    }
}

/// Whether so many watched nodes are suspect in one pass that the leader should
/// suspect its own connectivity. Never trips on a single node.
pub(crate) fn breaker_tripped(suspects: usize, watched: usize, max_fraction: f64) -> bool {
    let allowed = (watched as f64 * max_fraction).floor() as usize;
    suspects > allowed.max(1)
}

/// The nodes one pass counts against the silent-node limit. A node whose agent
/// answered is never one, and neither is a Suspended node.
#[derive(Debug)]
struct Suspects {
    failed_probes: usize,
    operator_managed: usize,
    recently_silent: usize,
}

impl Suspects {
    fn count(results: &[ProbeResult], operator_managed: &[String], stale: &StaleNodes) -> Self {
        let counted = |node: &String| !stale.suspended.contains(node);
        let failed_probes = results
            .iter()
            .filter(|r| !r.outcome.is_alive() && counted(&r.target.node))
            .count();
        let operator_managed = operator_managed.iter().filter(|n| counted(n)).count();
        // With no node to mark Down, a heartbeat-path outage would trip on the
        // nodes that have only missed a beat, raising a false alarm.
        let recently_silent = if failed_probes + operator_managed > 0 {
            stale.recently_silent
        } else {
            0
        };
        Self {
            failed_probes,
            operator_managed,
            recently_silent,
        }
    }

    fn total(&self) -> usize {
        self.failed_probes + self.operator_managed + self.recently_silent
    }
}

/// A pass whose suspects exceeded the silent-node limit.
#[derive(Debug)]
struct Trip {
    suspects: Suspects,
    watched: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The agent answered; `warn` is true on the first pass of this silence and
    /// then once per [`SILENT_NODE_WARN_INTERVAL`].
    Keep { warn: bool },
    /// The probe failed; the node gets one more pass.
    Wait,
    /// The probe failed on consecutive passes.
    MarkDown,
}

/// A node's run of consecutive failed probes. `first` and `last_heartbeat` are
/// taken at the run's first failure, to check later failures against.
#[derive(Debug)]
struct FailureRun {
    count: u32,
    first: Instant,
    last_heartbeat: Option<DateTime<Utc>>,
}

impl FailureRun {
    fn starting(now: Instant, last_heartbeat: Option<DateTime<Utc>>) -> Self {
        Self {
            count: 0,
            first: now,
            last_heartbeat,
        }
    }
}

/// Per-node probe history across passes. Only consecutive failures at least
/// `min_gap` apart count, so one outage seen by back-to-back probes counts once.
#[derive(Debug, Default)]
pub(crate) struct StrikeTracker {
    failures: HashMap<String, FailureRun>,
    /// Answering nodes, mapped to when the leader last warned about them.
    answering: HashMap<String, Instant>,
    min_gap: Duration,
}

impl StrikeTracker {
    fn new(min_gap: Duration) -> Self {
        Self {
            min_gap,
            ..Default::default()
        }
    }

    pub(crate) fn record(
        &mut self,
        node: &str,
        last_heartbeat: Option<DateTime<Utc>>,
        outcome: &ProbeOutcome,
        now: Instant,
    ) -> Verdict {
        match outcome {
            ProbeOutcome::Alive => {
                self.failures.remove(node);
                Verdict::Keep {
                    warn: self.warn_due(node, now),
                }
            }
            ProbeOutcome::Unreachable(_) => {
                self.answering.remove(node);
                let run = self
                    .failures
                    .entry(node.to_string())
                    .or_insert_with(|| FailureRun::starting(now, last_heartbeat));
                // With a timeout shorter than a tick, a node can heartbeat between
                // passes and be stale again by the next; it was up in between.
                if run.last_heartbeat != last_heartbeat {
                    *run = FailureRun::starting(now, last_heartbeat);
                }
                run.count += 1;
                // An overrun sweep lets the next start at once, which would
                // strike a node twice in moments.
                let spaced =
                    now.saturating_duration_since(run.first) + STRIKE_SPACING_SLACK >= self.min_gap;
                if run.count >= FAILED_PROBES_TO_MARK_DOWN && spaced {
                    Verdict::MarkDown
                } else {
                    Verdict::Wait
                }
            }
        }
    }

    /// Whether an answering node is due a warning, starting its interval if so.
    fn warn_due(&mut self, node: &str, now: Instant) -> bool {
        let Some(last) = self.answering.get_mut(node) else {
            self.answering.insert(node.to_string(), now);
            return true;
        };
        if now.saturating_duration_since(*last) < SILENT_NODE_WARN_INTERVAL {
            return false;
        }
        *last = now;
        true
    }

    /// Forgets every node that is no longer stale.
    pub(crate) fn retain(&mut self, stale: &HashSet<String>) {
        self.failures.retain(|node, _| stale.contains(node));
        self.answering.retain(|node, _| stale.contains(node));
    }

    pub(crate) fn clear(&mut self) {
        self.failures.clear();
        self.answering.clear();
    }

    pub(crate) fn clear_failures(&mut self) {
        self.failures.clear();
    }
}

/// When a pass began. Suspects are read at `wall`; the apply step adds the time
/// the probes took, so a node that crossed the timeout meanwhile is flagged
/// not-responding and not recovered.
#[derive(Debug, Clone, Copy)]
struct PassStart {
    wall: DateTime<Utc>,
    at: Instant,
}

impl PassStart {
    fn now() -> Self {
        Self {
            wall: Utc::now(),
            at: Instant::now(),
        }
    }

    fn wall_now(&self) -> DateTime<Utc> {
        self.wall + chrono::Duration::from_std(self.at.elapsed()).unwrap_or_default()
    }
}

/// The health loop's liveness state. Leader-local: see [`Self::observe_term`].
#[derive(Debug)]
pub(crate) struct NodeLiveness {
    strikes: StrikeTracker,
    max_stale_fraction: f64,
    health_tick: Duration,
    term: Option<u64>,
    /// When a tripped silent-node limit was last logged; `None` while it is not tripped.
    breaker_logged: Option<Instant>,
}

impl NodeLiveness {
    pub(crate) fn new(max_stale_fraction: f64, health_tick: Duration) -> Self {
        Self {
            strikes: StrikeTracker::new(health_tick),
            max_stale_fraction,
            health_tick,
            term: None,
            breaker_logged: None,
        }
    }

    /// Whether a tripped pass is due its error: on the trip, then once per
    /// [`SILENT_NODE_WARN_INTERVAL`] while it stays tripped.
    fn breaker_log_due(&mut self, now: Instant) -> bool {
        if self
            .breaker_logged
            .is_some_and(|last| now.saturating_duration_since(last) < SILENT_NODE_WARN_INTERVAL)
        {
            return false;
        }
        self.breaker_logged = Some(now);
        true
    }

    /// Whether a pass that did not trip ends a trip, which is then forgotten.
    fn breaker_recovered(&mut self) -> bool {
        self.breaker_logged.take().is_some()
    }

    /// Called every health tick with the term this controller leads, or `None`.
    /// Leader-local state belongs to the term that built it, so a new term or a
    /// lost leadership discards strikes, not-responding flags, and the gauges.
    pub(crate) fn observe_term(&mut self, cluster: &ClusterManager, leading_term: Option<u64>) {
        if self.term == leading_term {
            return;
        }
        self.strikes.clear();
        self.breaker_logged = None;
        cluster.clear_not_responding();
        cluster.set_liveness_breaker_tripped(false);
        cluster.set_silent_answering_nodes(0);
        self.term = leading_term;
    }

    /// One health pass. `still_leading` is re-checked after the probes, which can
    /// take seconds, so a pass judged under a lost leadership is dropped.
    pub(crate) async fn sweep<P: Prober, F: Fn() -> bool>(
        &mut self,
        cluster: &ClusterManager,
        timeout_secs: u64,
        mark_down: MarkDownPolicy,
        prober: &P,
        still_leading: F,
    ) -> Vec<JobFinalized> {
        self.sweep_at(
            cluster,
            timeout_secs,
            mark_down,
            prober,
            still_leading,
            PassStart::now(),
        )
        .await
    }

    async fn sweep_at<P: Prober, F: Fn() -> bool>(
        &mut self,
        cluster: &ClusterManager,
        timeout_secs: u64,
        mark_down: MarkDownPolicy,
        prober: &P,
        still_leading: F,
        start: PassStart,
    ) -> Vec<JobFinalized> {
        let (mark_down, trip) = match mark_down {
            MarkDownPolicy::Allowed => {
                let stale = cluster.stale_nodes(timeout_secs, self.health_tick, start.wall);
                let (confirmed, trip) = self.confirm(cluster, stale, prober).await;
                (MarkDownPolicy::Only(confirmed), Some(trip))
            }
            other => (other, None),
        };
        if !still_leading() {
            self.strikes.clear();
            cluster.set_liveness_breaker_tripped(false);
            cluster.set_silent_answering_nodes(0);
            return Vec::new();
        }
        if let Some(trip) = trip {
            self.report_breaker(cluster, trip.as_ref());
        }
        cluster.set_silent_answering_nodes(self.strikes.answering.len());
        cluster.check_node_health_at(timeout_secs, mark_down, start.wall_now())
    }

    /// Judges the silent-node limit after probing, by the nodes that did not
    /// answer; returns the nodes confirmed gone and the trip, if any. Probing a
    /// mass silence can overrun the tick, which delays the next pass.
    async fn confirm<P: Prober>(
        &mut self,
        cluster: &ClusterManager,
        stale: StaleNodes,
        prober: &P,
    ) -> (HashSet<String>, Option<Trip>) {
        let stale_names: HashSet<String> = stale.names.iter().cloned().collect();
        self.strikes.retain(&stale_names);
        if stale_names.is_empty() {
            return (HashSet::new(), None);
        }
        let (operator_managed, targets) = split_by_source(cluster, &stale.names);
        let results = probe_all(targets, prober).await;

        let suspects = Suspects::count(&results, &operator_managed, &stale);
        if breaker_tripped(suspects.total(), stale.watched, self.max_stale_fraction) {
            self.record_answers(results);
            self.strikes.clear_failures();
            let trip = Trip {
                suspects,
                watched: stale.watched,
            };
            return (HashSet::new(), Some(trip));
        }

        let mut confirmed: HashSet<String> = operator_managed.into_iter().collect();
        confirmed.extend(self.judge_probes(results));
        (confirmed, None)
    }

    /// Publishes a pass's breaker state, logging a trip or a recovery.
    fn report_breaker(&mut self, cluster: &ClusterManager, trip: Option<&Trip>) {
        cluster.set_liveness_breaker_tripped(trip.is_some());
        let Some(Trip { suspects, watched }) = trip else {
            if self.breaker_recovered() {
                info!(
                    "the silent-node limit is no longer exceeded; marking unreachable nodes Down again"
                );
            }
            return;
        };
        if self.breaker_log_due(Instant::now()) {
            error!(
                failed_probes = suspects.failed_probes,
                operator_managed = suspects.operator_managed,
                recently_silent = suspects.recently_silent,
                watched,
                "too many nodes went silent in one health pass; suspecting the control plane and marking none of them Down"
            );
        }
    }

    /// Records each probe's outcome and returns the nodes now confirmed gone.
    fn judge_probes(&mut self, results: Vec<ProbeResult>) -> HashSet<String> {
        results
            .into_iter()
            .filter_map(|result| self.judge(result))
            .collect()
    }

    /// Records only the nodes that answered, so a node that keeps answering
    /// through a tripped pass is not reported as newly silent afterwards.
    fn record_answers(&mut self, results: Vec<ProbeResult>) {
        for result in results.into_iter().filter(|r| r.outcome.is_alive()) {
            self.judge(result);
        }
    }

    /// Records one probe's outcome; returns the node if it is now confirmed gone.
    fn judge(&mut self, result: ProbeResult) -> Option<String> {
        let ProbeResult {
            target,
            outcome,
            at,
        } = result;
        let node = target.node;
        match self
            .strikes
            .record(&node, target.last_heartbeat, &outcome, at)
        {
            Verdict::Keep { warn: true } => warn!(
                node = %node,
                silent_for = %silent_for(target.last_heartbeat, Utc::now()),
                "heartbeats from this node are not reaching the leader, but its agent still answers; not marking it Down, so check its jobs"
            ),
            Verdict::Keep { warn: false } => {
                debug!(node = %node, "silent node's agent still answers")
            }
            Verdict::Wait => warn!(
                node = %node,
                reason = %outcome.reason(),
                "silent node did not answer a liveness probe; waiting one more sweep before marking it Down"
            ),
            Verdict::MarkDown => {
                warn!(
                    node = %node,
                    reason = %outcome.reason(),
                    "silent node failed liveness probes on consecutive sweeps"
                );
                return Some(node);
            }
        }
        None
    }
}

fn silent_for(last_heartbeat: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match last_heartbeat {
        Some(hb) => format!("{}s", (now - hb).num_seconds().max(0)),
        None => "no heartbeat on record".into(),
    }
}

/// Splits stale nodes into operator-managed ones and probe targets for the
/// rest. The operator answers for its nodes, so there is no agent of their own
/// to ask: they keep the plain timeout rule.
fn split_by_source(cluster: &ClusterManager, names: &[String]) -> (Vec<String>, Vec<ProbeTarget>) {
    let mut operator_managed = Vec::new();
    let mut targets = Vec::new();
    for name in names {
        let Some(node) = cluster.get_node(name) else {
            continue;
        };
        if node.source != NodeSource::NativeHost {
            operator_managed.push(name.clone());
            continue;
        }
        targets.push(ProbeTarget {
            endpoint: crate::scheduler_loop::node_comm_http_url(&node),
            node: name.clone(),
            last_heartbeat: node.last_heartbeat,
        });
    }
    (operator_managed, targets)
}

/// One probe's outcome and when it ran.
struct ProbeResult {
    target: ProbeTarget,
    outcome: ProbeOutcome,
    at: Instant,
}

/// Probes every target, at most [`MAX_CONCURRENT_PROBES`] at a time. A probe
/// task that dies yields no outcome, so its node waits for the next pass.
async fn probe_all<P: Prober>(targets: Vec<ProbeTarget>, prober: &P) -> Vec<ProbeResult> {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
    let mut probes = JoinSet::new();
    for target in targets {
        let prober = prober.clone();
        let permits = permits.clone();
        probes.spawn(async move {
            let _permit = permits.acquire_owned().await;
            // Stamped once the probe has a slot, not when the pass began: a probe
            // queued behind others runs later, and strike spacing must see that.
            let at = Instant::now();
            let outcome = prober.probe(target.clone()).await;
            ProbeResult {
                target,
                outcome,
                at,
            }
        });
    }
    let mut outcomes = Vec::new();
    while let Some(joined) = probes.join_next().await {
        match joined {
            Ok(result) => outcomes.push(result),
            Err(e) => warn!(error = %e, "node liveness probe task failed"),
        }
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use parking_lot::Mutex;
    use spur_core::node::{NodeSource, NodeState};
    use spur_core::resource::ResourceSet;
    use tempfile::TempDir;

    use super::*;
    use crate::cluster::tests::{
        register_node, set_last_heartbeat, set_node_state, test_cluster, wait_for,
    };
    use crate::cluster::{ClusterManager, MarkDownPolicy};
    use crate::scheduler_loop::tests::dispatch_trigger_tests::spawn_mock_agent_answering_ping;

    fn unreachable() -> ProbeOutcome {
        ProbeOutcome::Unreachable("no answer".into())
    }

    #[test]
    fn the_breaker_never_trips_on_a_single_stale_node() {
        assert!(!breaker_tripped(1, 1, 0.5));
        assert!(!breaker_tripped(1, 4, 0.5));
    }

    #[test]
    fn the_breaker_trips_when_more_than_the_fraction_goes_stale() {
        assert!(!breaker_tripped(2, 4, 0.5));
        assert!(breaker_tripped(3, 4, 0.5));
        assert!(breaker_tripped(2, 2, 0.5));
        assert!(breaker_tripped(2, 3, 0.5));
    }

    #[test]
    fn a_fraction_of_one_turns_the_breaker_off() {
        assert!(!breaker_tripped(4, 4, 1.0));
        assert!(!breaker_tripped(1000, 1000, 1.0));
    }

    #[test]
    fn a_node_is_marked_down_only_after_two_consecutive_failed_probes() {
        let mut strikes = StrikeTracker::default();
        let now = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::Wait
        );
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::MarkDown
        );
    }

    #[test]
    fn an_answer_between_failures_starts_the_count_over() {
        let mut strikes = StrikeTracker::default();
        let now = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::Wait
        );
        assert_eq!(
            strikes.record("n1", None, &ProbeOutcome::Alive, now),
            Verdict::Keep { warn: true }
        );
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::Wait
        );
    }

    #[test]
    fn an_answering_node_is_reported_new_once_per_silence() {
        let mut strikes = StrikeTracker::default();
        let now = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &ProbeOutcome::Alive, now),
            Verdict::Keep { warn: true }
        );
        assert_eq!(
            strikes.record("n1", None, &ProbeOutcome::Alive, now),
            Verdict::Keep { warn: false }
        );
        strikes.retain(&HashSet::new());
        assert_eq!(
            strikes.record("n1", None, &ProbeOutcome::Alive, now),
            Verdict::Keep { warn: true }
        );
    }

    #[test]
    fn an_answering_silent_node_is_warned_about_again_only_after_the_interval() {
        let mut strikes = StrikeTracker::default();
        let t0 = Instant::now();
        let keep =
            |strikes: &mut StrikeTracker, at| strikes.record("n1", None, &ProbeOutcome::Alive, at);

        assert_eq!(keep(&mut strikes, t0), Verdict::Keep { warn: true });
        assert_eq!(
            keep(&mut strikes, t0 + Duration::from_secs(30)),
            Verdict::Keep { warn: false }
        );
        assert_eq!(
            keep(
                &mut strikes,
                t0 + SILENT_NODE_WARN_INTERVAL - Duration::from_secs(1)
            ),
            Verdict::Keep { warn: false }
        );
        assert_eq!(
            keep(&mut strikes, t0 + SILENT_NODE_WARN_INTERVAL),
            Verdict::Keep { warn: true }
        );
        assert_eq!(
            keep(
                &mut strikes,
                t0 + SILENT_NODE_WARN_INTERVAL + Duration::from_secs(30)
            ),
            Verdict::Keep { warn: false },
            "the interval restarts at the last warning"
        );
    }

    #[test]
    fn a_node_that_stops_answering_or_being_stale_is_warned_about_afresh() {
        let mut strikes = StrikeTracker::default();
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let alive = ProbeOutcome::Alive;

        assert_eq!(
            strikes.record("n1", None, &alive, at(0)),
            Verdict::Keep { warn: true }
        );
        strikes.record("n1", None, &unreachable(), at(1));
        assert_eq!(
            strikes.record("n1", None, &alive, at(2)),
            Verdict::Keep { warn: true }
        );
        strikes.retain(&HashSet::new());
        assert_eq!(
            strikes.record("n1", None, &alive, at(3)),
            Verdict::Keep { warn: true }
        );
        strikes.clear();
        assert_eq!(
            strikes.record("n1", None, &alive, at(4)),
            Verdict::Keep { warn: true }
        );
    }

    #[test]
    fn clearing_failures_keeps_the_answering_nodes() {
        let mut strikes = StrikeTracker::default();
        let now = Instant::now();
        strikes.record("up", None, &ProbeOutcome::Alive, now);
        assert_eq!(
            strikes.record("down", None, &unreachable(), now),
            Verdict::Wait
        );

        strikes.clear_failures();

        assert_eq!(
            strikes.record("down", None, &unreachable(), now),
            Verdict::Wait
        );
        assert_eq!(
            strikes.record("up", None, &ProbeOutcome::Alive, now),
            Verdict::Keep { warn: false }
        );
    }

    #[test]
    fn a_node_that_stops_being_stale_loses_its_strike() {
        let mut strikes = StrikeTracker::default();
        let now = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::Wait
        );
        strikes.retain(&HashSet::new());
        assert_eq!(
            strikes.record("n1", None, &unreachable(), now),
            Verdict::Wait
        );
    }

    #[test]
    fn a_second_failure_in_a_catch_up_burst_does_not_mark_down_but_a_spaced_one_does() {
        let gap = Duration::from_secs(15);
        let mut strikes = StrikeTracker::new(gap);
        let t0 = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &unreachable(), t0),
            Verdict::Wait
        );
        // A sweep that overran its tick probes again almost at once: the same
        // outage seen twice, not a second independent pass.
        assert_eq!(
            strikes.record("n1", None, &unreachable(), t0 + Duration::from_secs(1)),
            Verdict::Wait
        );
        // A genuine second pass, a health tick later, marks it Down.
        assert_eq!(
            strikes.record("n1", None, &unreachable(), t0 + Duration::from_secs(16)),
            Verdict::MarkDown
        );
    }

    #[test]
    fn a_second_failure_a_hair_under_one_tick_still_marks_down() {
        let gap = Duration::from_secs(15);
        let mut strikes = StrikeTracker::new(gap);
        let t0 = Instant::now();
        assert_eq!(
            strikes.record("n1", None, &unreachable(), t0),
            Verdict::Wait
        );
        // Just under a full tick, within the spacing slack: a timer landing
        // early must not push Down a whole pass later.
        assert_eq!(
            strikes.record(
                "n1",
                None,
                &unreachable(),
                t0 + Duration::from_millis(14_700)
            ),
            Verdict::MarkDown
        );
    }

    #[test]
    fn a_heartbeat_between_failures_starts_the_run_over() {
        let gap = Duration::from_secs(15);
        let mut strikes = StrikeTracker::new(gap);
        let t0 = Instant::now();
        let before = Some(Utc::now() - chrono::Duration::seconds(60));
        let after = Some(Utc::now() - chrono::Duration::seconds(20));
        let at = |secs| t0 + Duration::from_secs(secs);

        assert_eq!(
            strikes.record("n1", before, &unreachable(), at(0)),
            Verdict::Wait
        );
        assert_eq!(
            strikes.record("n1", after, &unreachable(), at(16)),
            Verdict::Wait,
            "a heartbeat landed between the two failures"
        );
        // The run restarted at the second failure, so spacing counts from there.
        assert_eq!(
            strikes.record("n1", after, &unreachable(), at(17)),
            Verdict::Wait
        );
        assert_eq!(
            strikes.record("n1", after, &unreachable(), at(32)),
            Verdict::MarkDown
        );
    }

    /// Fails every probe after `delay`, like an agent that never answers.
    #[derive(Clone)]
    struct SlowFailingProber {
        delay: Duration,
    }

    impl Prober for SlowFailingProber {
        async fn probe(&self, _target: ProbeTarget) -> ProbeOutcome {
            tokio::time::sleep(self.delay).await;
            unreachable()
        }
    }

    /// One more silent node than can be probed at once, so one probe queues.
    fn one_past_the_probe_limit() -> Vec<ProbeTarget> {
        (0..=MAX_CONCURRENT_PROBES)
            .map(|i| ProbeTarget {
                node: format!("n{i}"),
                endpoint: None,
                last_heartbeat: None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_queued_behind_the_limit_is_stamped_when_it_runs() {
        let t0 = Instant::now();
        let results = probe_all(
            one_past_the_probe_limit(),
            &SlowFailingProber {
                delay: NODE_PROBE_TIMEOUT,
            },
        )
        .await;

        let mut started: Vec<Duration> = results.iter().map(|r| r.at - t0).collect();
        started.sort();
        assert_eq!(started.len(), MAX_CONCURRENT_PROBES + 1);
        assert!(started[..MAX_CONCURRENT_PROBES]
            .iter()
            .all(|offset| offset.is_zero()));
        assert_eq!(started[MAX_CONCURRENT_PROBES], NODE_PROBE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_queued_late_in_an_overrun_pass_is_not_struck_twice_by_the_next() {
        let tick = Duration::from_secs(15);
        let mut liveness = NodeLiveness::new(1.0, tick);

        // Every probe times out, so the queued one starts a full probe bound late
        // and the pass overruns its tick.
        let first = probe_all(
            one_past_the_probe_limit(),
            &SlowFailingProber {
                delay: NODE_PROBE_TIMEOUT,
            },
        )
        .await;
        let late = first
            .iter()
            .max_by_key(|r| r.at)
            .map(|r| r.target.node.clone())
            .unwrap();
        assert!(liveness.judge_probes(first).is_empty());

        // The overdue tick fires the next pass at once, and now every probe
        // fails fast: the queued node's two failures are 10 s apart, not a tick.
        let confirmed = liveness.judge_probes(
            probe_all(
                one_past_the_probe_limit(),
                &SlowFailingProber {
                    delay: Duration::ZERO,
                },
            )
            .await,
        );
        assert_eq!(confirmed.len(), MAX_CONCURRENT_PROBES);
        assert!(
            !confirmed.contains(&late),
            "{late} failed twice within one tick"
        );
    }

    type ProbeHook = Box<dyn Fn(&str) + Send + Sync>;

    /// Answers for the nodes in `answering`, records each probe, and can run a
    /// hook mid-probe to model a heartbeat that lands while probes are in flight.
    #[derive(Clone, Default)]
    struct ScriptedProber {
        answering: Arc<Mutex<HashSet<String>>>,
        probed: Arc<Mutex<Vec<String>>>,
        during_probe: Arc<Mutex<Option<ProbeHook>>>,
    }

    impl ScriptedProber {
        fn answering(nodes: &[&str]) -> Self {
            let prober = Self::default();
            prober
                .answering
                .lock()
                .extend(nodes.iter().map(|n| n.to_string()));
            prober
        }

        fn probed(&self) -> Vec<String> {
            self.probed.lock().clone()
        }
    }

    impl Prober for ScriptedProber {
        async fn probe(&self, target: ProbeTarget) -> ProbeOutcome {
            self.probed.lock().push(target.node.clone());
            if let Some(hook) = self.during_probe.lock().as_ref() {
                hook(&target.node);
            }
            if self.answering.lock().contains(&target.node) {
                ProbeOutcome::Alive
            } else {
                unreachable()
            }
        }
    }

    fn silent_for(cm: &ClusterManager, nodes: &[&str], secs: i64) {
        for name in nodes {
            set_last_heartbeat(cm, name, Some(Utc::now() - chrono::Duration::seconds(secs)));
        }
    }

    fn silence(cm: &ClusterManager, nodes: &[&str]) {
        silent_for(cm, nodes, 200);
    }

    fn state(cm: &ClusterManager, name: &str) -> NodeState {
        cm.get_node(name).unwrap().state
    }

    async fn cluster_of(dir: &TempDir, nodes: &[&str]) -> Arc<ClusterManager> {
        let cm = test_cluster(dir).await;
        for name in nodes {
            register_node(&cm, name, 4, 8000);
        }
        cm
    }

    fn register_operator_node(cm: &ClusterManager, name: &str) {
        cm.register_node(
            name.into(),
            name.into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            spur_core::node::K8S_OPERATOR_VERSION.into(),
            NodeSource::Kubernetes {
                namespace: "default".into(),
            },
            HashMap::new(),
            Vec::new(),
            true,
        )
        .unwrap();
        wait_for("operator node registered", || cm.get_node(name).is_some());
    }

    async fn pass(
        liveness: &mut NodeLiveness,
        cm: &ClusterManager,
        prober: &ScriptedProber,
    ) -> Vec<JobFinalized> {
        liveness
            .sweep(cm, 90, MarkDownPolicy::Allowed, prober, || true)
            .await
    }

    fn assert_all_up(cm: &ClusterManager, nodes: &[&str]) {
        for name in nodes {
            assert_ne!(state(cm, name), NodeState::Down, "{name} was marked Down");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_answering_silent_node_does_not_shield_a_dead_one() {
        let dir = TempDir::new().unwrap();
        // n1's heartbeats are lost but its agent answers, n2 is dead, n3 is fine.
        let cm = cluster_of(&dir, &["n1", "n2", "n3"]).await;
        silence(&cm, &["n1", "n2"]);
        let prober = ScriptedProber::answering(&["n1"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        for _ in 0..2 {
            pass(&mut liveness, &cm, &prober).await;
        }

        assert_eq!(state(&cm, "n2"), NodeState::Down);
        assert_all_up(&cm, &["n1", "n3"]);
    }

    async fn assert_a_dead_node_goes_down_after_a_trip_clears(change_term: bool) {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1", "n2", "n3", "n4"]).await;
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));

        pass(&mut liveness, &cm, &prober).await;
        assert_all_up(&cm, &["n1", "n2", "n3"]);

        if change_term {
            liveness.observe_term(&cm, Some(2));
        }
        // n1 and n2 answer again but still cannot heartbeat; only n3 is dead.
        prober.answering.lock().extend(["n1".into(), "n2".into()]);
        for _ in 0..2 {
            pass(&mut liveness, &cm, &prober).await;
        }

        assert_eq!(state(&cm, "n3"), NodeState::Down);
        assert_all_up(&cm, &["n1", "n2", "n4"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_node_goes_down_once_the_failures_behind_a_trip_clear() {
        assert_a_dead_node_goes_down_after_a_trip_clears(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_node_goes_down_after_a_trip_and_a_new_term() {
        assert_a_dead_node_goes_down_after_a_trip_clears(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodes_that_missed_a_heartbeat_count_before_they_go_stale() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["s1", "s2", "r1", "r2", "h1"]).await;
        // A correlated fault with staggered last heartbeats: s1 and s2 are past
        // the timeout, r1 and r2 will be in a moment.
        silent_for(&cm, &["s1", "s2"], 100);
        silent_for(&cm, &["r1", "r2"], 60);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        for _ in 0..2 {
            assert!(pass(&mut liveness, &cm, &prober).await.is_empty());
        }

        assert_all_up(&cm, &["s1", "s2", "r1", "r2", "h1"]);
        let mut probed = prober.probed();
        probed.sort();
        assert_eq!(probed, vec!["s1", "s1", "s2", "s2"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_staggered_mass_failure_trips_before_early_nodes_go_down_at_a_short_timeout() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["e1", "e2", "l1", "l2"]).await;
        let (tick, timeout_secs) = (Duration::from_secs(15), 45);
        let mut liveness = NodeLiveness::new(0.5, tick);
        // The passes run moments apart in real time; their wall clock is injected.
        liveness.strikes.min_gap = Duration::ZERO;
        liveness.observe_term(&cm, Some(1));
        let prober = ScriptedProber::default();
        // Heartbeats stop at C: e1 and e2 last beat at C-22, l1 and l2 at C.
        let c = Utc::now();
        let nodes = ["e1", "e2", "l1", "l2"];
        for name in &nodes[..2] {
            set_last_heartbeat(&cm, name, Some(c - chrono::Duration::seconds(22)));
        }
        for name in &nodes[2..] {
            set_last_heartbeat(&cm, name, Some(c));
        }

        for secs_after_c in [9, 24, 39] {
            liveness
                .sweep_at(
                    &cm,
                    timeout_secs,
                    MarkDownPolicy::Allowed,
                    &prober,
                    || true,
                    PassStart {
                        wall: c + chrono::Duration::seconds(secs_after_c),
                        at: Instant::now(),
                    },
                )
                .await;
            assert_all_up(&cm, &nodes);
            assert_eq!(
                cm.node_metrics().liveness_breaker_tripped,
                secs_after_c == 39,
                "breaker at C+{secs_after_c}"
            );
        }
        let mut probed = prober.probed();
        probed.sort();
        assert_eq!(
            probed,
            ["e1", "e1", "e2", "e2"],
            "e1 and e2 failed at C+24 and C+39"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_that_crosses_the_timeout_while_probes_run_is_judged_when_the_pass_applies() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["d1", "late", "down", "h1"]).await;
        silence(&cm, &["d1"]);
        silent_for(&cm, &["late", "down"], 80);
        set_node_state(&cm, "down", NodeState::Down);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(1.0, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));
        let probe_phase = Duration::from_secs(30);

        liveness
            .sweep_at(
                &cm,
                90,
                MarkDownPolicy::Allowed,
                &prober,
                || true,
                PassStart {
                    wall: Utc::now(),
                    at: Instant::now().checked_sub(probe_phase).unwrap(),
                },
            )
            .await;

        assert_eq!(
            prober.probed(),
            ["d1"],
            "late and down were fresh at the start"
        );
        assert!(
            cm.get_node("late").unwrap().not_responding,
            "late was past the timeout when the pass applied"
        );
        assert_eq!(state(&cm, "down"), NodeState::Down, "down was recovered");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missed_heartbeats_do_not_trip_the_breaker_when_every_probe_answers() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["s1", "r1", "r2", "r3", "h1"]).await;
        // A heartbeat-path outage with staggered last heartbeats: every agent answers.
        silent_for(&cm, &["s1"], 100);
        silent_for(&cm, &["r1", "r2", "r3"], 60);
        let prober = ScriptedProber::answering(&["s1"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));

        pass(&mut liveness, &cm, &prober).await;

        assert!(!cm.node_metrics().liveness_breaker_tripped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silent_suspended_nodes_do_not_count_toward_the_limit() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["h1", "h2", "d1", "s1", "s2"]).await;
        silence(&cm, &["d1", "s1"]);
        silent_for(&cm, &["s2"], 60);
        set_node_state(&cm, "s1", NodeState::Suspended);
        set_node_state(&cm, "s2", NodeState::Suspended);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        // Counting s1 and s2 would make three suspects of five, over the limit.
        for _ in 0..2 {
            pass(&mut liveness, &cm, &prober).await;
        }

        assert_eq!(state(&cm, "d1"), NodeState::Down);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspended_nodes_do_not_raise_the_limit() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["h1", "d1", "d2", "s1", "s2"]).await;
        silence(&cm, &["d1", "d2"]);
        set_node_state(&cm, "s1", NodeState::Suspended);
        set_node_state(&cm, "s2", NodeState::Suspended);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        // Two of three watched nodes fail; counting s1 and s2 would allow two.
        for _ in 0..2 {
            pass(&mut liveness, &cm, &prober).await;
        }

        assert_all_up(&cm, &["d1", "d2"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trip_forgets_failed_probes_but_not_answering_nodes() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["a", "d1", "d2", "d3", "h1"]).await;
        silence(&cm, &["a", "d1"]);
        let prober = ScriptedProber::answering(&["a"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        pass(&mut liveness, &cm, &prober).await;

        silence(&cm, &["d2", "d3"]);
        pass(&mut liveness, &cm, &prober).await;

        assert_all_up(&cm, &["d1", "d2", "d3"]);
        assert!(liveness.strikes.failures.is_empty());
        assert_eq!(
            liveness
                .strikes
                .record("a", None, &ProbeOutcome::Alive, Instant::now()),
            Verdict::Keep { warn: false },
            "a node that kept answering through a trip is not new"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_operator_managed_node_counts_toward_the_limit() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["r1", "h1"]).await;
        register_operator_node(&cm, "k1");
        silence(&cm, &["k1"]);
        silent_for(&cm, &["r1"], 60);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        pass(&mut liveness, &cm, &prober).await;

        assert_all_up(&cm, &["k1"]);
        assert!(prober.probed().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_node_whose_agent_answers_is_kept() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::answering(&["n1"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        for _ in 0..3 {
            let evicted = liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
                .await;
            assert!(evicted.is_empty());
        }
        assert_eq!(state(&cm, "n1"), NodeState::Idle);
        assert_eq!(prober.probed(), vec!["n1", "n1", "n1"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_node_is_marked_down_after_failing_probes_on_two_sweeps() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "one failed probe only waits"
        );

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        wait_for("second failed probe marks the node down", || {
            cm.get_node("n1")
                .is_some_and(|n| n.state == NodeState::Down)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_heartbeat_landing_during_the_confirming_probe_keeps_the_node_up() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;

        let hook_cluster = cm.clone();
        *prober.during_probe.lock() = Some(Box::new(move |node| {
            hook_cluster.update_heartbeat(node, 0, 8000);
        }));
        let evicted = liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;

        assert!(evicted.is_empty());
        assert_eq!(state(&cm, "n1"), NodeState::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_heartbeat_between_failed_probes_starts_the_count_over() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1"]).await;
        silent_for(&cm, &["n1"], 30);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        // A timeout shorter than a tick: n1 heartbeats between passes, yet is
        // stale again by the next one.
        let timeout_secs = 10;

        liveness
            .sweep(&cm, timeout_secs, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        silent_for(&cm, &["n1"], 20);
        liveness
            .sweep(&cm, timeout_secs, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "failures either side of a heartbeat are not consecutive"
        );

        liveness
            .sweep(&cm, timeout_secs, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert_eq!(prober.probed(), vec!["n1", "n1", "n1"]);
        wait_for(
            "a second failure with no heartbeat between marks down",
            || {
                cm.get_node("n1")
                    .is_some_and(|n| n.state == NodeState::Down)
            },
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_breaker_marks_nobody_down_when_most_nodes_go_silent() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        for name in ["n1", "n2", "n3", "n4"] {
            register_node(&cm, name, 4, 8000);
        }
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        for _ in 0..3 {
            let evicted = liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
                .await;
            assert!(evicted.is_empty());
        }
        for name in ["n1", "n2", "n3"] {
            assert_eq!(state(&cm, name), NodeState::Idle, "{name}");
            // Kept up, but flagged so they take no new work while silent.
            assert!(
                cm.get_node(name).unwrap().not_responding,
                "{name} should be flagged not-responding"
            );
        }
        assert_eq!(
            prober.probed().len(),
            9,
            "a tripped breaker still probes every silent node, each pass"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_breaker_gauge_is_set_on_a_trip_and_cleared_by_a_pass_that_does_not_trip() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1", "n2", "n3", "n4"]).await;
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));
        assert!(!cm.node_metrics().liveness_breaker_tripped);

        pass(&mut liveness, &cm, &prober).await;
        let tripped = cm.node_metrics();
        assert!(tripped.liveness_breaker_tripped);
        assert_eq!(tripped.not_responding, 3);

        silent_for(&cm, &["n1", "n2", "n3"], 0);
        pass(&mut liveness, &cm, &prober).await;
        let cleared = cm.node_metrics();
        assert!(!cleared.liveness_breaker_tripped);
        assert_eq!(cleared.not_responding, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_breaker_gauge_counts_only_the_last_pass() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1", "n2", "n3", "n4"]).await;
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));
        pass(&mut liveness, &cm, &prober).await;
        assert!(cm.node_metrics().liveness_breaker_tripped);

        // Two of the three answer again: one suspect of four does not trip.
        prober.answering.lock().extend(["n1".into(), "n2".into()]);
        pass(&mut liveness, &cm, &prober).await;
        let metrics = cm.node_metrics();
        assert!(!metrics.liveness_breaker_tripped);
        assert_eq!(metrics.not_responding, 3, "answering nodes stay flagged");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_dropped_for_lost_leadership_does_not_set_the_breaker_gauge() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1", "n2", "n3", "n4"]).await;
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || false)
            .await;

        assert!(!cm.node_metrics().liveness_breaker_tripped);
        assert!(
            liveness.breaker_logged.is_none(),
            "a dropped pass must not log a trip"
        );
    }

    #[test]
    fn a_tripped_breaker_is_logged_on_the_trip_and_then_once_per_interval() {
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        let t0 = Instant::now();

        assert!(liveness.breaker_log_due(t0));
        assert!(!liveness.breaker_log_due(t0 + Duration::from_secs(30)));
        assert!(liveness.breaker_log_due(t0 + SILENT_NODE_WARN_INTERVAL));
        assert!(liveness.breaker_recovered());
        assert!(!liveness.breaker_recovered(), "recovery is logged once");
        assert!(
            liveness.breaker_log_due(t0 + SILENT_NODE_WARN_INTERVAL + Duration::from_secs(30)),
            "a new trip is logged at once"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_term_clears_the_breaker_gauge() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["n1", "n2", "n3", "n4"]).await;
        silence(&cm, &["n1", "n2", "n3"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));
        pass(&mut liveness, &cm, &prober).await;
        assert!(cm.node_metrics().liveness_breaker_tripped);

        liveness.observe_term(&cm, None);

        let metrics = cm.node_metrics();
        assert!(!metrics.liveness_breaker_tripped);
        assert_eq!(metrics.not_responding, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_silent_answering_gauge_counts_kept_nodes_until_they_heartbeat() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["a1", "a2", "d1", "h1", "h2"]).await;
        silence(&cm, &["a1", "a2", "d1"]);
        let prober = ScriptedProber::answering(&["a1", "a2"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));

        pass(&mut liveness, &cm, &prober).await;
        let metrics = cm.node_metrics();
        assert_eq!(metrics.silent_answering, 2, "d1 failed its probe");
        assert_eq!(metrics.not_responding, 3);

        silent_for(&cm, &["a1"], 0);
        pass(&mut liveness, &cm, &prober).await;
        assert_eq!(cm.node_metrics().silent_answering, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_silent_answering_gauge_clears_on_a_new_term_and_a_dropped_pass() {
        let dir = TempDir::new().unwrap();
        let cm = cluster_of(&dir, &["a1", "h1"]).await;
        silence(&cm, &["a1"]);
        let prober = ScriptedProber::answering(&["a1"]);
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        liveness.observe_term(&cm, Some(1));
        pass(&mut liveness, &cm, &prober).await;
        assert_eq!(cm.node_metrics().silent_answering, 1);

        liveness.observe_term(&cm, Some(2));
        assert_eq!(cm.node_metrics().silent_answering, 0);

        pass(&mut liveness, &cm, &prober).await;
        assert_eq!(cm.node_metrics().silent_answering, 1);
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || false)
            .await;
        assert_eq!(cm.node_metrics().silent_answering, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn half_the_nodes_going_silent_is_still_probed() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        for name in ["n1", "n2", "n3", "n4"] {
            register_node(&cm, name, 4, 8000);
        }
        silence(&cm, &["n1", "n2"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        for _ in 0..2 {
            liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
                .await;
        }
        wait_for("both silent nodes marked down", || {
            ["n1", "n2"]
                .iter()
                .all(|n| cm.get_node(n).is_some_and(|n| n.state == NodeState::Down))
        });
        assert_eq!(state(&cm, "n3"), NodeState::Idle);
        assert_eq!(state(&cm, "n4"), NodeState::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_operator_managed_node_is_marked_down_without_a_probe() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_operator_node(&cm, "k1");
        silence(&cm, &["k1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        wait_for("operator node marked down on the first sweep", || {
            cm.get_node("k1")
                .is_some_and(|n| n.state == NodeState::Down)
        });
        assert!(prober.probed().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_leadership_grace_window_probes_nobody() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        let evicted = liveness
            .sweep(&cm, 90, MarkDownPolicy::Suppressed, &prober, || true)
            .await;

        assert!(evicted.is_empty());
        assert!(prober.probed().is_empty());
        assert_eq!(state(&cm, "n1"), NodeState::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_term_forgets_failed_probes_from_the_last_one() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);

        liveness.observe_term(&cm, Some(1));
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        liveness.observe_term(&cm, Some(2));
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;

        assert_eq!(state(&cm, "n1"), NodeState::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn losing_leadership_clears_not_responding_flags_and_strikes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        let flagged = |cm: &ClusterManager| cm.get_node("n1").unwrap().not_responding;

        liveness.observe_term(&cm, Some(1));
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert!(flagged(&cm));
        liveness.observe_term(&cm, Some(1));
        assert!(flagged(&cm), "the same term keeps its flags");

        liveness.observe_term(&cm, None);
        assert!(!flagged(&cm), "a lost leadership must not keep its flags");
        liveness.observe_term(&cm, Some(2));
        assert!(!flagged(&cm));

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "the strike from the lost term must not count"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_to_back_sweeps_do_not_count_one_outage_twice() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::from_secs(15));

        // An overran sweep fires the next at once; both probes see the same outage.
        for _ in 0..2 {
            liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
                .await;
        }
        assert_eq!(prober.probed(), vec!["n1", "n1"]);
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "two probes moments apart are one strike"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_leadership_change_during_the_probe_discards_the_pass() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5, Duration::ZERO);
        let leading = Arc::new(AtomicBool::new(true));
        let lose_leadership = leading.clone();
        *prober.during_probe.lock() = Some(Box::new(move |_| {
            lose_leadership.store(false, Ordering::SeqCst);
        }));

        // Two failed sweeps would mark n1 Down, but leadership is lost while
        // each sweep's probe is in flight, so neither pass is applied.
        for _ in 0..2 {
            leading.store(true, Ordering::SeqCst);
            let evicted = liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || {
                    leading.load(Ordering::SeqCst)
                })
                .await;
            assert!(evicted.is_empty());
        }
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "a pass judged under a lost term must not be applied"
        );

        // Strikes were cleared on the term change, so a dead node still needs
        // two fresh failed probes once leadership is regained.
        *prober.during_probe.lock() = None;
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober, || true)
            .await;
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "one fresh strike after regaining leadership only waits"
        );
    }

    fn target(node: &str, addr: std::net::SocketAddr) -> ProbeTarget {
        ProbeTarget {
            node: node.into(),
            endpoint: Some(format!("http://{addr}")),
            last_heartbeat: None,
        }
    }

    #[tokio::test]
    async fn the_probe_reports_alive_when_the_agent_answers_as_the_node() {
        let addr = spawn_mock_agent_answering_ping("n1").await;
        assert!(matches!(
            probe_agent(target("n1", addr)).await,
            ProbeOutcome::Alive
        ));
    }

    #[tokio::test]
    async fn the_probe_rejects_an_agent_answering_as_another_node() {
        let addr = spawn_mock_agent_answering_ping("someone-else").await;
        let ProbeOutcome::Unreachable(reason) = probe_agent(target("n1", addr)).await else {
            panic!("an agent answering as another node must not count as alive");
        };
        assert!(reason.contains("someone-else"), "{reason}");
    }

    #[tokio::test]
    async fn the_probe_reports_a_refused_connection_as_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(matches!(
            probe_agent(target("n1", addr)).await,
            ProbeOutcome::Unreachable(_)
        ));
    }

    // The paused clock jumps to the next timer while the probe waits on the
    // socket, so the probe's own bound runs out without a real wait.
    #[tokio::test(start_paused = true)]
    async fn the_probe_gives_up_on_an_agent_that_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });
        let ProbeOutcome::Unreachable(reason) = probe_agent(target("n1", addr)).await else {
            panic!("an agent that never answers must not count as alive");
        };
        assert!(reason.starts_with("no answer within"), "{reason}");
    }

    #[tokio::test]
    async fn a_node_without_an_address_is_unreachable() {
        let outcome = probe_agent(ProbeTarget {
            node: "n1".into(),
            endpoint: None,
            last_heartbeat: None,
        })
        .await;
        assert!(matches!(outcome, ProbeOutcome::Unreachable(_)));
    }
}
