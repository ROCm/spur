# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression test for a false node mark-down after a Raft leadership blip.

A controller that loses and regains leadership between two of its own health
ticks never observes the intermediate "not leader" moment, so its grace window
stays armed from the *original* election even though its heartbeat map went
stale for the whole blip. The next health pass then judges every node against
that stale map and marks healthy ones down, evicting their running jobs.
"""

import time

import pytest

from cluster import (
    HA_HEALTH_TICK_SECS,
    HA_HEARTBEAT_TIMEOUT_SECS,
    log_tail_is_leader,
    parse_job_id,
    wait_job_state,
    job_state,
)

# The repro needs HA_HEARTBEAT_TIMEOUT_SECS < blip < HA_HEALTH_TICK_SECS: long
# enough that the regaining leader's heartbeat map is past the timeout (a
# follower forwards heartbeats and records none of its own), short enough that
# a once-per-tick is_leader() sampler never observes "not leader".
BLIP_HOLD_SECS = 42
# Budget for the interim election; the original is resumed as soon as one is
# seen, so this is a timeout, not an enforced freeze duration.
INTERIM_ELECTION_TIMEOUT_SECS = 10
RACE_SETTLE_TIMEOUT_SECS = 30
# Staging the blip is timing-sensitive and exhausting the attempts now fails the
# test, so budget more of them than the repro typically needs.
MAX_RECLAIM_ATTEMPTS = 5
GRACE_SECS = max(HA_HEARTBEAT_TIMEOUT_SECS, HA_HEALTH_TICK_SECS)
# Wait past grace plus one full health tick before judging node/job state.
HEALTH_SETTLE_SECS = GRACE_SECS + 2 * HA_HEALTH_TICK_SECS + 10
# Long enough to outlast MAX_RECLAIM_ATTEMPTS worth of retries plus
# HEALTH_SETTLE_SECS, so the job can't complete out from under the assertion.
SURVIVOR_JOB_SECS = 1800


def _wait_initial_leader(cluster, timeout: float = 60.0) -> int:
    """A split first vote is common, so require the leader to actually settle
    before a blip is staged against it."""
    cluster._wait_leader_elected(timeout=int(timeout))
    leader = cluster._current_raft_leader()
    if leader is None:
        raise TimeoutError("no stable controller leader within the timeout")
    return leader


def _wait_became_leader_since(cluster, indices, since_lens, timeout, want=None):
    """Poll until one of *indices* has a NEW 'become leader' line beyond its
    `since_lens` snapshot, so a long-ago-elected controller can't be mistaken
    for one that just (re)won. With *want*, keeps waiting past other winners."""
    deadline = time.time() + timeout
    seen = None
    while time.time() < deadline:
        for i in indices:
            if log_tail_is_leader(cluster.spurctld_log(i)[since_lens[i]:]):
                if want is None or i == want:
                    return i
                seen = i
        time.sleep(0.3)
    return seen


def _attempt_reclaim(cluster, n: int, leader_idx: int) -> tuple[int | None, float]:
    """One blip-and-regain cycle on `leader_idx`, returning who ended up leading
    and how long `leader_idx` spent not leading.

    Freezing `leader_idx` for the whole blip does not work: the interim commits
    a blank entry for its new term, leaving `leader_idx` log-behind so Raft must
    reject its vote. Instead it is frozen only long enough for the interim to be
    elected, then resumed as a live, caught-up FOLLOWER -- which records no
    heartbeats of its own, so its map goes stale while it stays eligible to win.
    """
    since_lens = {i: len(cluster.spurctld_log(i)) for i in range(n)}
    others = [i for i in range(n) if i != leader_idx]
    blip_start = time.time()

    try:
        cluster.signal_controller(leader_idx, "STOP")
        interim = _wait_became_leader_since(
            cluster, others, since_lens, timeout=INTERIM_ELECTION_TIMEOUT_SECS
        )
        cluster.signal_controller(leader_idx, "CONT")
        if interim is None:
            return None, 0.0

        held = time.time() - blip_start
        time.sleep(max(0.0, BLIP_HOLD_SECS - held))

        # Freeze the incumbent so the (now caught-up) original can win the race.
        cluster.signal_controller(interim, "STOP")
        winner = _wait_became_leader_since(
            cluster, [i for i in range(n) if i != interim], since_lens,
            timeout=RACE_SETTLE_TIMEOUT_SECS, want=leader_idx,
        )
        return winner, time.time() - blip_start
    finally:
        for i in range(n):
            cluster.signal_controller(i, "CONT")


class TestLeadershipBlipHealth:
    def test_blip_reclaim_does_not_false_mark_down(self, ha_cluster):
        cluster = ha_cluster
        n = cluster.ha_controller_count
        target = cluster.node_names[0]

        out_path = f"{cluster.remote_dir}/blip-survivor.out"
        script = cluster.write_file(
            "blip-survivor.sh", f"#!/bin/bash\nsleep {SURVIVOR_JOB_SECS}\necho SURVIVED\n"
        )
        sb = cluster.sbatch(
            ["-J", "blip-survivor", "-N", "1", "-w", target, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed:\n{sb}"
        wait_job_state(cluster, job_id, "R", timeout=60)

        # A buggy never-reset grace is still legitimately armed until the
        # ORIGINAL window expires; wait it out so a pass proves re-arming.
        elapsed = time.time() - cluster.ha_leader_elected_at
        remaining = GRACE_SECS + 5 - elapsed
        if remaining > 0:
            time.sleep(remaining)

        reclaimed = False
        attempts = []
        for _ in range(MAX_RECLAIM_ATTEMPTS):
            leader_idx = _wait_initial_leader(cluster)
            # Start just after a tick boundary so the whole blip fits between
            # two ticks (otherwise a mid-blip tick re-arms even the old code)
            # and the next tick lands while the map is still stale.
            offset = (time.time() - cluster.ha_controllers_started_at) % HA_HEALTH_TICK_SECS
            time.sleep((HA_HEALTH_TICK_SECS - offset + 2) % HA_HEALTH_TICK_SECS)

            winner, blip = _attempt_reclaim(cluster, n, leader_idx)
            # Let the cluster fully restabilize before the next attempt/assert.
            _wait_initial_leader(cluster, timeout=60)
            attempts.append(f"winner={winner} want={leader_idx} blip={blip:.1f}s")
            # Outside this band the run proves nothing: a blip under the
            # heartbeat timeout leaves no node stale enough to be a mark-down
            # candidate, and one over a tick lets the old code re-arm correctly.
            if (
                winner == leader_idx
                and HA_HEARTBEAT_TIMEOUT_SECS < blip < HA_HEALTH_TICK_SECS
            ):
                reclaimed = True
                break

        if not reclaimed:
            # Not a skip: the harness ran, so this is the regression scenario
            # failing to stage rather than a missing prerequisite.
            pytest.fail(
                "no same-controller reclaim with a blip inside "
                f"({HA_HEARTBEAT_TIMEOUT_SECS}s, {HA_HEALTH_TICK_SECS}s) in "
                f"{MAX_RECLAIM_ATTEMPTS} attempts: " + "; ".join(attempts)
            )

        time.sleep(HEALTH_SETTLE_SECS)

        states = cluster.sinfo_nodes()
        assert target in states, f"{target} missing from sinfo:\n{states}"
        assert states.get(target, "").lower() != "down", (
            f"node {target} was falsely marked down after a leadership blip:\n{states}"
        )
        state = job_state(cluster.squeue_all(), job_id)
        assert state == "R", (
            f"job {job_id} should still be running after the blip, got {state!r}"
        )
