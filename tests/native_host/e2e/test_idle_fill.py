# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for idle-fill scheduling.

Idle-fill lets a job that has exceeded its QOS group node cap run anyway, on
nodes no job with a quota claim wants.  Such a run is *borrowed*: it is outside
every quota aggregate, it is flagged through ``squeue %W`` and
``sacct --format=Borrowed``, and it is reclaimed when a job that does hold a claim
needs the capacity.

Requires:
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
  - enough nodes to stage the scenario; each test skips when short

Every test fills the cluster before asking for the behaviour under test.  That is
not incidental: with an idle node left over, a waiting job is satisfied directly
and no reclaim ever fires, so a test that forgets to fill the cluster passes
while asserting nothing.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job_state

_SLEEP = "#!/bin/bash\nsleep 900\n"

# Long enough that the grace period is not the thing under test, short enough not
# to pad every test: reclaim waits idle_fill_exempt_secs before touching a run.
_EXEMPT_SECS = 5
_WAIT_RECLAIM = 180
# Several scheduler cycles at the default one-second interval. Used wherever the
# assertion is "nothing happened", which needs time to have been given a chance.
_GUARD_SECS = 20


def _config(*, enabled: bool = True, default_time: str | None = "10:00") -> dict:
    partition = {
        "name": "default",
        "state": "UP",
        "default": True,
        "nodes": "ALL",
        "max_time": "24:00:00",
        "preempt_mode": "off",
    }
    if default_time is not None:
        partition["default_time"] = default_time
    return {
        "partitions": [partition],
        "scheduler": {
            "idle_fill_enabled": enabled,
            "idle_fill_exempt_secs": _EXEMPT_SECS,
            # 0 means "no cluster-wide default", so a job submitted without -t has
            # no effective time limit. test_no_time_limit depends on this.
            "default_time_limit_minutes": 0,
        },
        "auth": {"plugin": "none", "allow_root_jobs": True},
    }


def _require_nodes(cluster, count: int) -> list[str]:
    if len(cluster.node_names) < count:
        pytest.skip(
            f"needs {count} nodes to stage the scenario, "
            f"inventory has {len(cluster.node_names)}"
        )
    return cluster.node_names


def _borrowed(cluster, job_id: int) -> str | None:
    """The Borrowed column for one job, as squeue reports it ('yes' / 'no')."""
    out = cluster.squeue(["-t", "all", "-o", "%i %W"])
    for line in out.splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 2 and parts[0] == str(job_id):
            return parts[1]
    return None


def _diagnostics(cluster) -> str:
    """Everything needed to tell a scheduling decision apart from a dead cluster."""
    parts = [
        f"squeue:\n{cluster.squeue(['-t', 'all', '-o', '%i %j %u %t %N %W %r'])}",
        f"sinfo:\n{cluster.sinfo()}",
    ]
    try:
        log = cluster.nodes[0].read_file(f"{cluster.log_dir}/spurctld.log")
        parts.append(f"spurctld.log tail:\n{log[-4000:]}")
    except Exception as e:  # noqa: BLE001 - diagnostics must never mask the failure
        parts.append(f"spurctld.log unavailable: {e}")
    for i in range(len(cluster.nodes)):
        try:
            parts.append(
                f"spurd.log on {cluster.node_names[i]}:\n{cluster.spurd_log(i)[-2500:]}"
            )
        except Exception as e:  # noqa: BLE001
            parts.append(f"spurd.log[{i}] unavailable: {e}")
    return "\n\n".join(parts)


def _await_running(cluster, job_id: int, timeout: int = _WAIT_RECLAIM) -> None:
    """wait_job_state, but report the cluster's state when it times out. A bare
    'did not reach R' cannot distinguish a scheduling decision from a dead agent."""
    try:
        wait_job_state(cluster, job_id, "R", timeout=timeout)
    except TimeoutError:
        raise AssertionError(
            f"job {job_id} never started within {timeout}s\n\n{_diagnostics(cluster)}"
        ) from None


def _assert_state(cluster, job_id: int, expected: str, label: str = "") -> None:
    show = cluster.scontrol("show", "job", str(job_id))
    tag = f" ({label})" if label else ""
    assert f"JobState={expected}" in show, (
        f"scontrol show job {job_id}{tag}: expected JobState={expected!r}:\n{show}"
    )


def _submit(cluster, name: str, qos: str, *extra: str) -> int:
    script = cluster.write_file(f"{name}.sh", _SLEEP)
    args = ["-N1", "--exclusive", "-q", qos, f"--job-name={name}"]
    args.extend(extra)
    args.append(script)
    job_id = parse_job_id(cluster.sbatch(args))
    assert job_id is not None, f"{name} submit failed"
    return job_id


def _fill_remaining(cluster, qos: str, tag: str, limit: int = 8) -> list[int]:
    """Occupy every idle node with in-quota jobs, so nothing is left spare."""
    filled = []
    for i in range(limit):
        if _idle_nodes(cluster) == 0:
            break
        job_id = _submit(cluster, f"{tag}{i}", qos, "-t", "30")
        wait_job_state(cluster, job_id, "R", timeout=60)
        filled.append(job_id)
    assert _idle_nodes(cluster) == 0, "cluster must be full before asking for reclaim"
    return filled


def _idle_nodes(cluster) -> int:
    return sum(
        1
        for state in cluster.sinfo_nodes().values()
        if state.lower().startswith("idle")
    )


def _cancel_all(cluster, job_ids) -> None:
    for job_id in job_ids:
        if job_id is not None:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestIdleFillSwitchedOffChangesNothing:
    """The regression guard. With the master switch off, an over-quota job must
    pend with QOSGrpNodeLimit and must not be lent the idle node sitting next to
    it. This is the assertion that the feature is genuinely opt-in."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config(enabled=False)

    def test_over_quota_job_still_pends_when_idle_fill_is_disabled(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        c.sacctmgr(["add", "qos", "name=offcap", "grptres=node=1"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "off-legit", "offcap", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            over = _submit(c, "off-over", "offcap", "-t", "30")
            ids.append(over)
            wait_job_state(c, over, "PD", timeout=60)

            # A node is genuinely free, so only the switch is keeping this job down.
            assert _idle_nodes(c) > 0, "fixture needs a spare node to be meaningful"

            time.sleep(_GUARD_SECS)
            assert job_state(c.squeue_all(), over) == "PD", (
                "with idle_fill_enabled=false an over-quota job must never be lent a node"
            )
            _assert_state(c, over, "PENDING", "switch off")
            show = c.scontrol("show", "job", str(over))
            assert "QOSGrpNodeLimit" in show, (
                f"reason must stay the group node cap:\n{show}"
            )
            assert _borrowed(c, over) == "no", "nothing may be flagged borrowed"
        finally:
            _cancel_all(c, ids)


class TestOverQuotaJobBorrowsIdleCapacity:
    """With the switch on, the same job runs on the spare node and is flagged
    borrowed, while the in-quota job beside it is not."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_over_quota_job_runs_on_idle_capacity_and_is_flagged(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        c.sacctmgr(["add", "qos", "name=borrowcap", "grptres=node=1"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "b-legit", "borrowcap", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)
            assert _borrowed(c, legit) == "no", "an in-quota run is not borrowed"

            over = _submit(c, "b-over", "borrowcap", "-t", "30")
            ids.append(over)
            wait_job_state(c, over, "R", timeout=60)
            _assert_state(c, over, "RUNNING", "borrowed")
            assert _borrowed(c, over) == "yes", (
                "a run that only started because capacity was spare must be flagged borrowed"
            )

            # The stamp must also reach accounting, which is where a completed run is
            # inspected after the fact.
            out = c.sacct(["-j", str(over), "--format=JobID,Borrowed"])
            assert "yes" in out, f"sacct must report the run as borrowed:\n{out}"
        finally:
            _cancel_all(c, ids)


class TestLegitimateClaimReclaimsABorrowedNode:
    """A job holding a real quota claim recovers a borrowed node. The borrowed run
    is requeued with its spec intact, and the in-quota run beside it is untouched.

    The victim's QOS sets preemptmode=cancel deliberately: reclaim must still
    requeue it, because that setting governs preemption between two jobs that both
    hold a claim, and a borrowed run holds none."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_borrowed_node_is_reclaimed_and_requeued_not_cancelled(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        c.sacctmgr(["add", "qos", "name=rcteam", "grptres=node=1", "preemptmode=cancel"])
        c.sacctmgr(["add", "qos", "name=rcclaim", "grptres=node=8"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "rc-legit", "rcteam", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            borrowed = _submit(c, "rc-borrow", "rcteam", "-t", "30")
            ids.append(borrowed)
            wait_job_state(c, borrowed, "R", timeout=60)
            assert _borrowed(c, borrowed) == "yes", "fixture needs a borrowed run"

            ids.extend(_fill_remaining(c, "rcclaim", "rc-fill"))
            time.sleep(_EXEMPT_SECS + 3)

            claim = _submit(c, "rc-claim", "rcclaim", "-t", "30")
            ids.append(claim)

            # The borrowed run must be requeued, not cancelled, despite
            # preemptmode=cancel on its QOS.
            wait_job_state(c, borrowed, "PD", timeout=_WAIT_RECLAIM)
            _assert_state(c, borrowed, "PENDING", "after reclaim")
            assert _borrowed(c, borrowed) == "yes", (
                "a requeued borrowed run keeps its stamp"
            )

            _await_running(c, claim)
            _assert_state(c, claim, "RUNNING", "claim placed")

            # The job with a claim was never a candidate for eviction.
            assert job_state(c.squeue_all(), legit) == "R", (
                "a run inside its quota must never be reclaimed"
            )
            _assert_state(c, legit, "RUNNING", "in-quota untouched")

            # Spec intact through the requeue: same node count and time limit.
            show = c.scontrol("show", "job", str(borrowed))
            assert "NumNodes=1" in show, f"requeued spec must be intact:\n{show}"
            assert "TimeLimit=00:30:00" in show, f"requeued spec must be intact:\n{show}"

            # And the reclaimed run is visible as borrowed in accounting.
            out = c.sacct(["-j", str(borrowed), "--format=JobID,State,Borrowed"])
            assert "yes" in out, f"sacct must flag the reclaimed run:\n{out}"
        finally:
            _cancel_all(c, ids)


class TestMultiBorrowReclaim:
    """The permanent guard for the multi-borrow defect: a QOS holding two or more
    borrowed runs must still yield capacity to a legitimate claim.

    A per-run over-quota test lets every borrower measure itself against the same
    slice of headroom, so all of them read as legitimate and the reclaimable pool
    comes back empty. Both cases below hold two borrowed runs at once; the second
    is the one a per-run test provably fails, because with the team's legitimate
    usage back to zero each borrower on its own fits the cap."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_two_borrowers_yield_to_a_legitimate_claim(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 3)
        c.sacctmgr(["add", "qos", "name=mbteam", "grptres=node=1"])
        c.sacctmgr(["add", "qos", "name=mbclaim", "grptres=node=8"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "mb-legit", "mbteam", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            borrowers = []
            for i in (1, 2):
                job_id = _submit(c, f"mb-borrow{i}", "mbteam", "-t", "30")
                ids.append(job_id)
                borrowers.append(job_id)
                wait_job_state(c, job_id, "R", timeout=60)
                assert _borrowed(c, job_id) == "yes", f"mb-borrow{i} must be borrowed"

            ids.extend(_fill_remaining(c, "mbclaim", "mb-fill"))
            time.sleep(_EXEMPT_SECS + 3)

            claim = _submit(c, "mb-claim", "mbclaim", "-t", "30")
            ids.append(claim)
            _await_running(c, claim)

            # Exactly one borrower gives up its node: the claim needed one node, and
            # reclaim evicts the minimum that closes the shortfall.
            sq = c.squeue_all()
            states = [job_state(sq, b) for b in borrowers]
            assert states.count("PD") == 1, (
                f"exactly one borrower must be reclaimed, got {states}"
            )
            assert states.count("R") == 1, (
                f"the other borrower must keep running, got {states}"
            )
            assert job_state(sq, legit) == "R", "the in-quota run must be untouched"
        finally:
            _cancel_all(c, ids)

    def test_two_borrowers_with_no_legitimate_sibling_are_still_reclaimable(
        self, accounting_cluster
    ):
        c = accounting_cluster
        _require_nodes(c, 3)
        c.sacctmgr(["add", "qos", "name=mzteam", "grptres=node=1"])
        c.sacctmgr(["add", "qos", "name=mzclaim", "grptres=node=8"])
        time.sleep(15)

        ids = []
        try:
            # The legitimate run exists only to push the siblings over the cap, so
            # that they are stamped. It goes away in a moment.
            legit = _submit(c, "mz-legit", "mzteam", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            borrowers = []
            for i in (1, 2):
                job_id = _submit(c, f"mz-borrow{i}", "mzteam", "-t", "30")
                ids.append(job_id)
                borrowers.append(job_id)
                wait_job_state(c, job_id, "R", timeout=60)
                assert _borrowed(c, job_id) == "yes", f"mz-borrow{i} must be borrowed"

            ids.extend(_fill_remaining(c, "mzclaim", "mz-fill"))

            # Drop the team's legitimate usage to zero while both borrowed runs keep
            # their nodes. Each borrower now fits the cap on its own, which is exactly
            # the state a per-run over-quota test mistakes for "became legitimate".
            c.cli_allow_fail(["scancel", str(legit)])
            ids.remove(legit)
            time.sleep(8)
            ids.extend(_fill_remaining(c, "mzclaim", "mz-refill"))
            time.sleep(_EXEMPT_SECS + 3)

            for job_id in borrowers:
                assert job_state(c.squeue_all(), job_id) == "R", (
                    "both borrowed runs must still hold their nodes at this point"
                )

            claim = _submit(c, "mz-claim", "mzclaim", "-t", "30")
            ids.append(claim)
            _await_running(c, claim)

            sq = c.squeue_all()
            states = [job_state(sq, b) for b in borrowers]
            assert states.count("PD") == 1, (
                "a team holding two nodes on a cap of one is over quota, so a "
                f"legitimate claim must reclaim one of its borrowed runs, got {states}"
            )
        finally:
            _cancel_all(c, ids)


class TestUnplaceableJobEvictsNothing:
    """A job that can never be placed must not evict a borrowed run, however long
    it waits. Without this, a job asking for the impossible would requeue one
    borrowed job per scheduler cycle forever."""

    @pytest.fixture
    def cluster_config_overrides(self):
        return _config()

    def test_a_job_that_can_never_be_placed_reclaims_nothing(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        c.sacctmgr(["add", "qos", "name=upteam", "grptres=node=1"])
        c.sacctmgr(["add", "qos", "name=upclaim", "grptres=node=8"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "up-legit", "upteam", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            borrowed = _submit(c, "up-borrow", "upteam", "-t", "30")
            ids.append(borrowed)
            wait_job_state(c, borrowed, "R", timeout=60)
            assert _borrowed(c, borrowed) == "yes", "fixture needs a borrowed run"

            ids.extend(_fill_remaining(c, "upclaim", "up-fill"))
            time.sleep(_EXEMPT_SECS + 3)

            # More CPUs than any node has, so no eviction can ever help it.
            greedy = _submit(c, "up-greedy", "upclaim", "-t", "30", "-c", "9999")
            ids.append(greedy)
            wait_job_state(c, greedy, "PD", timeout=60)

            # Several cycles: the borrowed run must survive all of them.
            time.sleep(_GUARD_SECS)
            sq = c.squeue_all()
            assert job_state(sq, borrowed) == "R", (
                "an unplaceable job must not evict a borrowed run"
            )
            _assert_state(c, borrowed, "RUNNING", "survived the unplaceable job")
            assert job_state(sq, greedy) == "PD", "the unplaceable job stays pending"
        finally:
            _cancel_all(c, ids)


class TestNoTimeLimitIsRefusedIdleFill:
    """A job with no effective time limit is never lent capacity. An unbounded
    borrowed run could hold its node forever, and the team whose quota it borrowed
    would have no recourse."""

    @pytest.fixture
    def cluster_config_overrides(self):
        # No partition default_time and no cluster default, so a job submitted
        # without -t genuinely has no limit.
        return _config(default_time=None)

    def test_a_job_with_no_time_limit_does_not_borrow(self, accounting_cluster):
        c = accounting_cluster
        _require_nodes(c, 2)
        c.sacctmgr(["add", "qos", "name=ntcap", "grptres=node=1"])
        time.sleep(15)

        ids = []
        try:
            legit = _submit(c, "nt-legit", "ntcap", "-t", "30")
            ids.append(legit)
            wait_job_state(c, legit, "R", timeout=60)

            # Deliberately no -t.
            unbounded = _submit(c, "nt-unbounded", "ntcap")
            ids.append(unbounded)
            wait_job_state(c, unbounded, "PD", timeout=60)

            assert _idle_nodes(c) > 0, "fixture needs a spare node to be meaningful"
            time.sleep(_GUARD_SECS)

            assert job_state(c.squeue_all(), unbounded) == "PD", (
                "a job with no effective time limit must not be lent a node"
            )
            show = c.scontrol("show", "job", str(unbounded))
            assert "TimeLimit=UNLIMITED" in show, (
                f"fixture must produce an unbounded job:\n{show}"
            )
            assert "QOSGrpNodeLimit" in show, (
                f"it stays blocked by the group node cap:\n{show}"
            )
            assert _borrowed(c, unbounded) == "no", "and it is not flagged borrowed"
        finally:
            _cancel_all(c, ids)
