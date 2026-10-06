# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression test for a false node mark-down caused by a Raft leadership
blip shorter than one health-check tick.

The health checker only evaluates node heartbeats while it believes it is the
Raft leader, and only re-arms its post-election grace window when it observes
a leadership transition. The bug this guards: a controller that loses and then
regains leadership entirely between two of its own periodic samples never
observes the intermediate "not leader" moment, so the grace window silently
stays armed from its *original* election — even though its own heartbeat map
went stale for the whole blip. The next health pass then judges every node
against a stale map and marks healthy nodes down, evicting their running jobs.

Reproducing this needs a real multi-controller Raft election (an actual
quit-then-regain on the *same* controller), which is inherently a race against
the other two controllers' own randomized election timeouts — there is no
production API to force a specific winner. The test below biases that race
(freezing the interim leader immediately, so the original leader does not have
to out-run an already-stable incumbent) and retries a bounded number of times;
if a genuine same-controller reclaim is never observed, it skips rather than
assert on an untested scenario.
"""

import time

import pytest

from cluster import parse_job_id, wait_job_state, job_state

# openraft election_timeout_max in raft.rs; a blip longer than this guarantees
# the other two controllers complete an election before it ends.
ELECTION_TIMEOUT_MAX_SECS = 3.0
# Budget for the interim election to complete; the leader is resumed as soon
# as one is detected, so this is a timeout, not an enforced freeze duration.
INTERIM_ELECTION_TIMEOUT_SECS = 5
RACE_SETTLE_TIMEOUT_SECS = 12
MAX_RECLAIM_ATTEMPTS = 5
# spurctld's health tick is a fixed 30s; wait past at least one full cycle
# after the reclaim before judging node/job state.
HEALTH_SETTLE_SECS = 40


def _log_tail_is_leader(log: str) -> bool:
    become = log.rfind("become leader")
    quit_ = log.rfind("quit leader")
    return become != -1 and become > quit_


def _wait_initial_leader(cluster, n: int, timeout: float = 30.0) -> int:
    deadline = time.time() + timeout
    while time.time() < deadline:
        for i in range(n):
            if _log_tail_is_leader(cluster.spurctld_log(i)):
                return i
        time.sleep(0.5)
    raise TimeoutError("no controller became leader within the timeout")


def _wait_became_leader_since(cluster, indices, since_lens, timeout) -> int | None:
    """Poll until one of *indices* has a NEW 'become leader' line beyond its
    `since_lens` snapshot. Comparing against a snapshot (not the whole log)
    means an already-frozen or long-ago-elected controller can't be mistaken
    for one that just (re)won leadership."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        for i in indices:
            suffix = cluster.spurctld_log(i)[since_lens[i]:]
            if _log_tail_is_leader(suffix):
                return i
        time.sleep(0.3)
    return None


def _attempt_reclaim(cluster, n: int, leader_idx: int) -> int | None:
    """One blip-and-regain cycle on `leader_idx`. Returns the index that holds
    leadership once the cluster restabilizes (None if nothing stabilized)."""
    since_lens = {i: len(cluster.spurctld_log(i)) for i in range(n)}
    others = [i for i in range(n) if i != leader_idx]

    cluster.signal_controller(leader_idx, "STOP")
    try:
        interim = _wait_became_leader_since(
            cluster, others, since_lens, timeout=INTERIM_ELECTION_TIMEOUT_SECS + ELECTION_TIMEOUT_MAX_SECS
        )
        if interim is None:
            return None
        # Depose the interim leader immediately so the resumed original
        # leader races a cold peer instead of an already-stable incumbent.
        cluster.signal_controller(interim, "STOP")
    finally:
        cluster.signal_controller(leader_idx, "CONT")

    race_candidates = [i for i in range(n) if i != interim]
    winner = _wait_became_leader_since(
        cluster, race_candidates, since_lens, timeout=RACE_SETTLE_TIMEOUT_SECS
    )
    cluster.signal_controller(interim, "CONT")
    return winner


class TestLeadershipBlipHealth:
    def test_blip_reclaim_does_not_false_mark_down(self, ha_cluster):
        cluster = ha_cluster
        n = cluster.ha_controller_count
        target = cluster.node_names[0]

        out_path = f"{cluster.remote_dir}/blip-survivor.out"
        script = cluster.write_file(
            "blip-survivor.sh", "#!/bin/bash\nsleep 180\necho SURVIVED\n"
        )
        sb = cluster.sbatch(
            ["-J", "blip-survivor", "-N", "1", "-w", target, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed:\n{sb}"
        wait_job_state(cluster, job_id, "R", timeout=60)

        reclaimed = False
        for _ in range(MAX_RECLAIM_ATTEMPTS):
            leader_idx = _wait_initial_leader(cluster, n)
            winner = _attempt_reclaim(cluster, n, leader_idx)
            # Let the cluster fully restabilize before the next attempt/assert.
            _wait_initial_leader(cluster, n, timeout=20)
            if winner == leader_idx:
                reclaimed = True
                break

        if not reclaimed:
            pytest.skip(
                f"could not reproduce a same-controller leadership reclaim in "
                f"{MAX_RECLAIM_ATTEMPTS} attempts (this scenario races the other "
                f"controllers' own election timers; no production hook forces a winner)"
            )

        time.sleep(HEALTH_SETTLE_SECS)

        states = cluster.sinfo_nodes()
        assert states.get(target, "").lower() != "down", (
            f"node {target} was falsely marked down after a leadership blip:\n{states}"
        )
        state = job_state(cluster.squeue_all(), job_id)
        assert state == "R", (
            f"job {job_id} should still be running after the blip, got {state!r}"
        )
