// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Confirms a node is really gone before the leader marks it Down.
//!
//! Heartbeats live only in the leader's memory and often reach it through a
//! follower, so silence cannot tell a dead node from a broken path or a
//! struggling leader. The leader asks a silent node's agent directly, marks it
//! Down only after it fails to answer on two consecutive passes, and marks no
//! one Down when so many nodes go silent at once that the fault is likely its own.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use spur_core::node::NodeSource;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, error, warn};

use crate::cluster::{ClusterManager, MarkDownPolicy, StaleNodes};
use crate::raft::JobFinalized;

/// Ceiling on one probe, connect included.
const NODE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds a pass that has to probe many nodes at once.
const MAX_CONCURRENT_PROBES: usize = 64;
/// Consecutive passes a node must fail to answer before it is marked Down.
const FAILED_PROBES_TO_MARK_DOWN: u32 = 2;

#[derive(Debug)]
pub(crate) enum ProbeOutcome {
    Alive,
    Unreachable(String),
}

impl ProbeOutcome {
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
}

/// How a pass reaches a silent node, so the decision can be tested without one.
pub(crate) trait Prober: Clone + Send + Sync + 'static {
    fn probe(&self, target: ProbeTarget) -> impl Future<Output = ProbeOutcome> + Send;
}

/// Asks the node's own agent, over the same path the leader uses to launch
/// and cancel its jobs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentProbe;

impl Prober for AgentProbe {
    async fn probe(&self, target: ProbeTarget) -> ProbeOutcome {
        probe_agent(target).await
    }
}

pub(crate) async fn probe_agent(target: ProbeTarget) -> ProbeOutcome {
    probe_agent_within(target, NODE_PROBE_TIMEOUT).await
}

/// [`probe_agent`] with an explicit bound, so tests can exercise a silent
/// agent without waiting out the production one.
async fn probe_agent_within(target: ProbeTarget, timeout: Duration) -> ProbeOutcome {
    let Some(endpoint) = target.endpoint else {
        return ProbeOutcome::Unreachable("node has no address to probe".into());
    };
    let ping = async {
        let mut agent = crate::agent_client::connect(endpoint)
            .await
            .map_err(|e| format!("connect failed: {e:?}"))?;
        agent
            .ping(())
            .await
            .map(|response| response.into_inner().hostname)
            .map_err(|status| format!("ping failed: {}", status.message()))
    };
    match tokio::time::timeout(timeout, ping).await {
        Ok(Ok(hostname)) if hostname == target.node => ProbeOutcome::Alive,
        Ok(Ok(hostname)) => ProbeOutcome::Unreachable(format!(
            "the agent at the node's address answered as {hostname}"
        )),
        Ok(Err(reason)) => ProbeOutcome::Unreachable(reason),
        Err(_) => ProbeOutcome::Unreachable(format!("no answer within {timeout:?}")),
    }
}

/// Whether so many watched nodes went stale in one pass that the leader should
/// suspect its own connectivity. Never trips on a single node.
pub(crate) fn breaker_tripped(stale: usize, monitored: usize, max_fraction: f64) -> bool {
    let allowed = (monitored as f64 * max_fraction).floor() as usize;
    stale > allowed.max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The agent answered; `first` is true on the first pass of this silence.
    Keep { first: bool },
    /// The probe failed; the node gets one more pass.
    Wait,
    /// The probe failed on consecutive passes.
    MarkDown,
}

/// Per-node probe history across passes. Only consecutive failures count, so a
/// node that answers, or stops being stale, starts over.
#[derive(Debug, Default)]
pub(crate) struct StrikeTracker {
    failures: HashMap<String, u32>,
    answering: HashSet<String>,
}

impl StrikeTracker {
    pub(crate) fn record(&mut self, node: &str, outcome: &ProbeOutcome) -> Verdict {
        match outcome {
            ProbeOutcome::Alive => {
                self.failures.remove(node);
                Verdict::Keep {
                    first: self.answering.insert(node.to_string()),
                }
            }
            ProbeOutcome::Unreachable(_) => {
                self.answering.remove(node);
                let failures = self.failures.entry(node.to_string()).or_insert(0);
                *failures += 1;
                if *failures >= FAILED_PROBES_TO_MARK_DOWN {
                    Verdict::MarkDown
                } else {
                    Verdict::Wait
                }
            }
        }
    }

    /// Forgets every node that is no longer stale.
    pub(crate) fn retain(&mut self, stale: &HashSet<String>) {
        self.failures.retain(|node, _| stale.contains(node));
        self.answering.retain(|node| stale.contains(node));
    }

    pub(crate) fn clear(&mut self) {
        self.failures.clear();
        self.answering.clear();
    }
}

/// The health loop's liveness state. Leader-local: reset whenever the term changes.
#[derive(Debug)]
pub(crate) struct NodeLiveness {
    strikes: StrikeTracker,
    max_stale_fraction: f64,
}

impl NodeLiveness {
    pub(crate) fn new(max_stale_fraction: f64) -> Self {
        Self {
            strikes: StrikeTracker::default(),
            max_stale_fraction,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.strikes.clear();
    }

    /// One health pass: confirms which stale nodes are really gone, then
    /// applies the pass with only those allowed to go Down.
    pub(crate) async fn sweep<P: Prober>(
        &mut self,
        cluster: &ClusterManager,
        timeout_secs: u64,
        mark_down: MarkDownPolicy,
        prober: &P,
    ) -> Vec<JobFinalized> {
        let mark_down = match mark_down {
            MarkDownPolicy::Allowed => {
                let stale = cluster.stale_nodes(timeout_secs);
                MarkDownPolicy::Only(self.confirm(cluster, stale, prober).await)
            }
            other => other,
        };
        cluster.check_node_health(timeout_secs, mark_down)
    }

    async fn confirm<P: Prober>(
        &mut self,
        cluster: &ClusterManager,
        stale: StaleNodes,
        prober: &P,
    ) -> HashSet<String> {
        let stale_names: HashSet<String> = stale.names.into_iter().collect();
        self.strikes.retain(&stale_names);
        if stale_names.is_empty() {
            return HashSet::new();
        }
        if breaker_tripped(stale_names.len(), stale.monitored, self.max_stale_fraction) {
            error!(
                stale = stale_names.len(),
                monitored = stale.monitored,
                "too many nodes went silent in one health pass; suspecting the control plane and marking none of them Down"
            );
            self.strikes.clear();
            return HashSet::new();
        }

        let mut confirmed = HashSet::new();
        let mut targets = Vec::new();
        for name in stale_names {
            let Some(node) = cluster.get_node(&name) else {
                continue;
            };
            if node.source != NodeSource::NativeHost {
                // The operator answers for its nodes, so there is no agent of
                // their own to ask: they keep the plain timeout rule.
                confirmed.insert(name);
                continue;
            }
            targets.push(ProbeTarget {
                endpoint: crate::scheduler_loop::node_comm_http_url(&node),
                node: name,
            });
        }

        for (target, outcome) in probe_all(targets, prober).await {
            let node = target.node;
            match self.strikes.record(&node, &outcome) {
                Verdict::Keep { first: true } => warn!(
                    node = %node,
                    "heartbeats from this node stopped reaching the leader, but its agent answers; not marking it Down"
                ),
                Verdict::Keep { first: false } => {
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
                    confirmed.insert(node);
                }
            }
        }
        confirmed
    }
}

/// Probes every target, at most [`MAX_CONCURRENT_PROBES`] at a time. A probe
/// task that dies yields no outcome, so its node waits for the next pass.
async fn probe_all<P: Prober>(
    targets: Vec<ProbeTarget>,
    prober: &P,
) -> Vec<(ProbeTarget, ProbeOutcome)> {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
    let mut probes = JoinSet::new();
    for target in targets {
        let prober = prober.clone();
        let permits = permits.clone();
        probes.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let outcome = prober.probe(target.clone()).await;
            (target, outcome)
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
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use parking_lot::Mutex;
    use spur_core::node::{NodeSource, NodeState};
    use spur_core::resource::ResourceSet;
    use tempfile::TempDir;

    use super::*;
    use crate::cluster::tests::{register_node, set_last_heartbeat, test_cluster, wait_for};
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
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::Wait);
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::MarkDown);
    }

    #[test]
    fn an_answer_between_failures_starts_the_count_over() {
        let mut strikes = StrikeTracker::default();
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::Wait);
        assert_eq!(
            strikes.record("n1", &ProbeOutcome::Alive),
            Verdict::Keep { first: true }
        );
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::Wait);
    }

    #[test]
    fn an_answering_node_is_reported_new_once_per_silence() {
        let mut strikes = StrikeTracker::default();
        assert_eq!(
            strikes.record("n1", &ProbeOutcome::Alive),
            Verdict::Keep { first: true }
        );
        assert_eq!(
            strikes.record("n1", &ProbeOutcome::Alive),
            Verdict::Keep { first: false }
        );
        strikes.retain(&HashSet::new());
        assert_eq!(
            strikes.record("n1", &ProbeOutcome::Alive),
            Verdict::Keep { first: true }
        );
    }

    #[test]
    fn a_node_that_stops_being_stale_loses_its_strike() {
        let mut strikes = StrikeTracker::default();
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::Wait);
        strikes.retain(&HashSet::new());
        assert_eq!(strikes.record("n1", &unreachable()), Verdict::Wait);
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

    fn silence(cm: &ClusterManager, nodes: &[&str]) {
        for name in nodes {
            set_last_heartbeat(cm, name, Some(Utc::now() - chrono::Duration::seconds(200)));
        }
    }

    fn state(cm: &ClusterManager, name: &str) -> NodeState {
        cm.get_node(name).unwrap().state
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_node_whose_agent_answers_is_kept() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::answering(&["n1"]);
        let mut liveness = NodeLiveness::new(0.5);

        for _ in 0..3 {
            let evicted = liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
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
        let mut liveness = NodeLiveness::new(0.5);

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
            .await;
        assert_eq!(
            state(&cm, "n1"),
            NodeState::Idle,
            "one failed probe only waits"
        );

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
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
        let mut liveness = NodeLiveness::new(0.5);
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
            .await;

        let hook_cluster = cm.clone();
        *prober.during_probe.lock() = Some(Box::new(move |node| {
            hook_cluster.update_heartbeat(node, 0, 8000);
        }));
        let evicted = liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
            .await;

        assert!(evicted.is_empty());
        assert_eq!(state(&cm, "n1"), NodeState::Idle);
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
        let mut liveness = NodeLiveness::new(0.5);

        for _ in 0..3 {
            let evicted = liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
                .await;
            assert!(evicted.is_empty());
        }
        for name in ["n1", "n2", "n3"] {
            assert_eq!(state(&cm, name), NodeState::Idle, "{name}");
        }
        assert!(
            prober.probed().is_empty(),
            "a tripped breaker probes nobody"
        );
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
        let mut liveness = NodeLiveness::new(0.5);

        for _ in 0..2 {
            liveness
                .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
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
        cm.register_node(
            "k1".into(),
            "k1".into(),
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
            true,
        )
        .unwrap();
        wait_for("operator node registered", || cm.get_node("k1").is_some());
        silence(&cm, &["k1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5);

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
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
        let mut liveness = NodeLiveness::new(0.5);

        let evicted = liveness
            .sweep(&cm, 90, MarkDownPolicy::Suppressed, &prober)
            .await;

        assert!(evicted.is_empty());
        assert!(prober.probed().is_empty());
        assert_eq!(state(&cm, "n1"), NodeState::Idle);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_forgets_earlier_failed_probes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        silence(&cm, &["n1"]);
        let prober = ScriptedProber::default();
        let mut liveness = NodeLiveness::new(0.5);

        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
            .await;
        liveness.reset();
        liveness
            .sweep(&cm, 90, MarkDownPolicy::Allowed, &prober)
            .await;

        assert_eq!(state(&cm, "n1"), NodeState::Idle);
    }

    fn target(node: &str, addr: std::net::SocketAddr) -> ProbeTarget {
        ProbeTarget {
            node: node.into(),
            endpoint: Some(format!("http://{addr}")),
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

    #[tokio::test]
    async fn the_probe_gives_up_on_an_agent_that_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });
        let outcome = probe_agent_within(target("n1", addr), Duration::from_millis(300)).await;
        assert!(matches!(outcome, ProbeOutcome::Unreachable(_)));
    }

    #[tokio::test]
    async fn a_node_without_an_address_is_unreachable() {
        let outcome = probe_agent(ProbeTarget {
            node: "n1".into(),
            endpoint: None,
        })
        .await;
        assert!(matches!(outcome, ProbeOutcome::Unreachable(_)));
    }
}
