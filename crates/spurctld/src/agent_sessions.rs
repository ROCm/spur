// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A cut can outlive the agent lifetime that took it. Registration names the
//! current one -- agent-supplied, so this settles a race and proves no identity.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

/// Leader-local record of the agent lifetime each node last announced. Never
/// persisted: only this leader issued the pulls a record here can disown.
#[derive(Default)]
pub(crate) struct AgentSessions {
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    term: Option<u64>,
    current: HashMap<String, String>,
    registrations: HashSet<String>,
    holds: HashMap<String, HashMap<u64, Option<u64>>>,
    next_hold: u64,
}

/// Captures the caller's term so a queued registration cannot begin after
/// leadership changes. An owned write may still resolve across that change.
#[derive(Clone)]
pub(crate) struct AgentSessionTerm {
    sessions: Arc<AgentSessions>,
    term: Option<u64>,
}

/// Registration writes can outlive a leader term or their caller's timeout.
/// Keep their endpoint publication exclusive until the write has resolved.
pub(crate) struct RegistrationInFlight {
    sessions: Arc<AgentSessions>,
    node: Option<String>,
    term: Option<u64>,
}

impl RegistrationInFlight {
    pub(crate) fn accept(mut self, session: &str) {
        let Some(node) = self.node.take() else {
            return;
        };
        let mut state = self.sessions.state.lock();
        // Another leader may have registered a newer endpoint before this
        // write's continuation runs. Its session is unknown in the new term.
        if state.term == self.term {
            state.current.insert(node.clone(), session.to_string());
        }
        state.registrations.remove(&node);
    }
}

impl Drop for RegistrationInFlight {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            self.sessions.state.lock().registrations.remove(&node);
        }
    }
}

pub(crate) struct ReconcileHold {
    sessions: Arc<AgentSessions>,
    node: String,
    id: u64,
}

impl Drop for ReconcileHold {
    fn drop(&mut self) {
        let mut state = self.sessions.state.lock();
        if let Some(holds) = state.holds.get_mut(&self.node) {
            holds.remove(&self.id);
            if holds.is_empty() {
                state.holds.remove(&self.node);
            }
        }
    }
}

impl AgentSessions {
    pub(crate) fn in_term(self: &Arc<Self>, term: Option<u64>) -> AgentSessionTerm {
        let scoped = AgentSessionTerm {
            sessions: self.clone(),
            term,
        };
        drop(scoped.state());
        scoped
    }

    /// Removal is replicated, so it applies even without a local leadership term.
    pub(crate) fn forget(&self, node: &str) {
        let mut state = self.state.lock();
        state.current.remove(node);
    }
}

impl AgentSessionTerm {
    fn state(&self) -> Option<MutexGuard<'_, SessionState>> {
        let term = self.term?;
        let mut state = self.sessions.state.lock();
        if state.term.is_some_and(|current| current > term) {
            return None;
        }
        if state.term != Some(term) {
            // Registration can precede the scheduler's first tick in this term.
            // Rotate once under the same lock that publishes sessions and holds.
            state.current.clear();
            state.holds.retain(|_, holds| {
                holds.retain(|_, owner_term| owner_term.is_none());
                !holds.is_empty()
            });
            state.term = Some(term);
        }
        Some(state)
    }

    pub(crate) fn begin_registration(
        &self,
        node: &str,
        current_term: Option<u64>,
    ) -> Result<RegistrationInFlight, &'static str> {
        if self.term.is_none() || self.term != current_term {
            return Err("leadership changed before registration");
        }
        let Some(mut state) = self.state() else {
            return Err("leadership changed before registration");
        };
        if !state.registrations.insert(node.to_string()) {
            return Err("a previous registration is still awaiting Raft");
        }
        Ok(RegistrationInFlight {
            sessions: self.sessions.clone(),
            node: Some(node.to_string()),
            term: self.term,
        })
    }

    pub(crate) fn allows_ledger_pull(&self, node: &str) -> bool {
        self.state()
            .is_some_and(|state| !state.registrations.contains(node))
    }

    pub(crate) fn hold(&self, node: &str) -> Option<ReconcileHold> {
        Some(self.hold_for(node, self.term, self.state()?))
    }

    /// A submitted write can resolve in a later term, so its gate cannot
    /// disappear when the task's original leadership ends.
    pub(crate) fn hold_until_resolved(&self, node: &str) -> ReconcileHold {
        self.hold_for(node, None, self.sessions.state.lock())
    }

    fn hold_for(
        &self,
        node: &str,
        owner_term: Option<u64>,
        mut state: MutexGuard<'_, SessionState>,
    ) -> ReconcileHold {
        let id = state.next_hold;
        state.next_hold = state.next_hold.wrapping_add(1);
        state
            .holds
            .entry(node.to_string())
            .or_default()
            .insert(id, owner_term);
        ReconcileHold {
            sessions: self.sessions.clone(),
            node: node.to_string(),
            id,
        }
    }

    pub(crate) fn is_held(&self, node: &str) -> bool {
        self.state().is_some_and(|state| {
            state.holds.contains_key(node) || state.registrations.contains(node)
        })
    }

    /// An unnamed registration still supersedes the previous process, so its
    /// sentinel survives until a named registration or a new leader term.
    #[cfg(test)]
    pub(crate) fn observe_registration(&self, node: &str, session: &str) {
        let Some(mut state) = self.state() else {
            return;
        };
        if state.registrations.contains(node) {
            return;
        }
        state.current.insert(node.to_string(), session.to_string());
    }

    /// Whether a cut carrying `session` can still be read as this node's current
    /// state. Nothing recorded means no registration has replaced what it names.
    pub(crate) fn vouches_for(&self, node: &str, session: &str) -> bool {
        // A legacy registration replaces the previous process without naming a
        // new lifetime; it cannot license an old cut or an unnamed one.
        !session.is_empty()
            && self.state().is_some_and(|state| {
                !state.registrations.contains(node)
                    && state
                        .current
                        .get(node)
                        .is_none_or(|current| current == session)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entering_a_term_after_registration_preserves_its_session_and_hold() {
        let sessions = Arc::new(AgentSessions::default());
        let registration = sessions.in_term(Some(7));
        let hold = registration.hold("n1").expect("current term");
        registration.observe_registration("n1", "session-b");

        let scheduler = sessions.in_term(Some(7));
        assert!(scheduler.is_held("n1"));
        assert!(scheduler.vouches_for("n1", "session-b"));
        assert!(!scheduler.vouches_for("n1", "session-a"));
        let follower = sessions.in_term(None);
        assert!(!follower.is_held("n1"));
        assert!(!follower.vouches_for("n1", "session-b"));
        assert!(scheduler.is_held("n1"));
        drop(hold);
        assert!(!scheduler.is_held("n1"));
    }

    #[test]
    fn registration_keeps_authority_and_scheduling_gated_across_terms() {
        let sessions = Arc::new(AgentSessions::default());
        let previous = sessions.in_term(Some(7));
        previous.observe_registration("n1", "session-a");
        let pending = previous.begin_registration("n1", Some(7)).expect("owner");
        let current = sessions.in_term(Some(8));
        assert!(current.is_held("n1"));
        assert!(!current.allows_ledger_pull("n1"));
        assert!(!current.vouches_for("n1", "session-a"));
        assert!(!current.vouches_for("n1", "session-b"));
        assert!(current.begin_registration("n1", Some(8)).is_err());
        pending.accept("session-b");
        assert!(!current.is_held("n1"));
        assert!(current.allows_ledger_pull("n1"));
        assert!(current.vouches_for("n1", "session-b"));
        assert!(current.vouches_for("n1", "session-c"));
        assert!(previous.begin_registration("n1", Some(8)).is_err());
    }

    #[test]
    fn failed_registration_preserves_only_the_current_terms_previous_session() {
        for next_term in [7, 8] {
            let sessions = Arc::new(AgentSessions::default());
            let previous = sessions.in_term(Some(7));
            previous.observe_registration("n1", "session-a");
            let pending = previous.begin_registration("n1", Some(7)).expect("owner");
            let current = sessions.in_term(Some(next_term));
            drop(pending);
            assert!(!current.is_held("n1"));
            assert!(current.vouches_for("n1", "session-a"));
            assert_eq!(current.vouches_for("n1", "session-b"), next_term == 8);
        }
    }

    #[test]
    fn a_node_that_has_never_registered_is_vouched_for() {
        // A controller that has just taken over knows no lifetimes, and stalling
        // reconciliation until every node happens to restart strands its capacity.
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        assert!(sessions.vouches_for("n1", "session-a"));
    }

    #[test]
    fn a_cut_from_the_lifetime_that_registered_is_vouched_for() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        assert!(sessions.vouches_for("n1", "session-a"));
    }

    #[test]
    fn a_cut_from_a_lifetime_a_re_registration_replaced_is_not() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        sessions.observe_registration("n1", "session-b");
        assert!(!sessions.vouches_for("n1", "session-a"));
        assert!(sessions.vouches_for("n1", "session-b"));
    }

    #[test]
    fn one_nodes_lifetime_says_nothing_about_another() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        assert!(sessions.vouches_for("n2", "session-a"));
    }

    #[test]
    fn an_agent_that_names_no_lifetime_replaces_the_previous_registration() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        sessions.observe_registration("n1", "");
        assert!(!sessions.vouches_for("n1", "session-a"));
        assert!(!sessions.vouches_for("n1", ""));
        assert!(!sessions.vouches_for("n1", "session-b"));
    }

    #[test]
    fn a_cut_naming_no_lifetime_is_never_vouched_for() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        assert!(!sessions.vouches_for("n1", ""));
        assert!(!sessions.vouches_for("n2", ""));
        assert!(sessions.vouches_for("n2", "session-b"));
    }

    #[test]
    fn a_term_that_ends_leaves_no_lifetime_behind() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        let sessions = sessions.sessions.in_term(Some(2));
        assert!(sessions.vouches_for("n1", "session-b"));
    }

    #[test]
    fn a_removed_node_is_forgotten() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        sessions.sessions.forget("n1");
        assert!(sessions.vouches_for("n1", "session-b"));
    }

    #[test]
    fn removing_a_node_cannot_release_a_concurrent_registrations_hold() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        let registration = sessions.hold("n1").expect("current term");
        sessions.sessions.forget("n1");
        sessions.observe_registration("n1", "session-b");
        assert!(sessions.is_held("n1"));
        assert!(sessions.vouches_for("n1", "session-b"));
        assert!(!sessions.vouches_for("n1", "session-a"));
        drop(registration);
        assert!(!sessions.is_held("n1"));
    }
    #[test]
    fn overlapping_holds_release_independently() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        let first = sessions.hold("n1");
        let second = sessions.hold("n1");
        drop(first);
        assert!(sessions.is_held("n1"));
        drop(second);
        assert!(!sessions.is_held("n1"));
    }

    #[test]
    fn a_previous_terms_guard_cannot_release_a_new_hold() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        let old = sessions.hold("n1");
        let sessions = sessions.sessions.in_term(Some(2));
        let current = sessions.hold("n1");
        drop(old);
        assert!(sessions.is_held("n1"));
        drop(current);
        assert!(!sessions.is_held("n1"));
    }

    #[test]
    fn an_unknown_writes_hold_survives_its_original_term() {
        let original = Arc::new(AgentSessions::default()).in_term(Some(1));
        let transient = original.hold("n1");
        let next = original.sessions.in_term(Some(2));
        assert!(!next.is_held("n1"));

        let pending = original.hold_until_resolved("n1");
        assert!(next.is_held("n1"));
        let latest = original.sessions.in_term(Some(3));
        assert!(latest.is_held("n1"));
        drop(transient);
        assert!(latest.is_held("n1"));
        drop(pending);
        assert!(!latest.is_held("n1"));
    }

    #[test]
    fn legacy_registration_replaces_the_previous_lifetime_when_its_write_resolves() {
        let sessions = Arc::new(AgentSessions::default()).in_term(Some(1));
        sessions.observe_registration("n1", "session-a");
        let pending = sessions.begin_registration("n1", Some(1)).expect("owner");
        assert!(!sessions.vouches_for("n1", "session-a"));
        pending.accept("");
        assert!(!sessions.vouches_for("n1", "session-a"));
        assert!(!sessions.vouches_for("n1", "session-b"));
        assert!(!sessions.is_held("n1"));
    }
}
