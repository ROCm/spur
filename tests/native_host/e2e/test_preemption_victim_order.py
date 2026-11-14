# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Black-box end-to-end tests for ``scheduler.preempt_victim_order``.

Two nodes each hold victims of one equal-priority QOS, and a higher QOS job needs one
whole node. Which node's jobs are evicted depends only on the configured order.
Every layout runs in both orientations, so node name alone can never pass a test.

Requires:
  - at least 2 nodes in SPUR_TEST_NODES (extra nodes are excluded from the aggressor)
  - Postgres on node 0 (accounting_cluster fixture, skips when Docker is absent)
"""

import json
import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

_SLEEP_SCRIPT = "#!/bin/bash\nsleep 600\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 5\n"

_WAIT_RUN = 60
_WAIT_PREEMPT = 60
# Several scheduler cycles, so a victim still running was passed over, not missed.
_GUARD_SECS = 10
# Separates an "old" victim's work from a "fresh" one by far more than one cycle.
_AGE_GAP_SECS = 25
# QOS cache refreshes on the accounting interval.
_CACHE_WARMUP_SECS = 15

_VICTIM_QOS = "vo-victim"
_HUNTER_QOS = "vo-hunter"


@pytest.fixture
def cluster_config_overrides():
    return {
        "partitions": [
            {
                "name": "default",
                "state": "UP",
                "default": True,
                "nodes": "ALL",
                "max_time": "24:00:00",
                "default_time": "10:00",
                "preempt_mode": "cancel",
            }
        ],
        "scheduler": {"preempt_type": "qos_priority", "preempt_exempt_time": 0},
        "auth": {"allow_root_jobs": True},
    }


@pytest.fixture
def vo(accounting_cluster):
    c = accounting_cluster
    if len(c.node_names) < 2:
        pytest.skip(f"needs at least 2 nodes in SPUR_TEST_NODES (got {len(c.node_names)})")
    c.sacctmgr(["add", "qos", f"name={_VICTIM_QOS}", "priority=100", "preemptmode=cancel"])
    c.sacctmgr(["add", "qos", f"name={_HUNTER_QOS}", "priority=10000",
                f"preempt={_VICTIM_QOS}"])
    time.sleep(_CACHE_WARMUP_SECS)
    jobs: list[int] = []
    yield c, jobs
    for job_id in jobs:
        c.cli_allow_fail(["scancel", str(job_id)])


def _set_order(c, keys: list[str]) -> None:
    c.config_overrides.setdefault("scheduler", {})["preempt_victim_order"] = keys
    c._write_config()
    c.scontrol("reconfigure")


def _start_victim(c, jobs: list[int], label: str, node: str, extra=()) -> int:
    script = c.write_file(f"{label}.sh", _SLEEP_SCRIPT)
    out = c.sbatch(["-J", label, "-q", _VICTIM_QOS, "-N1", "-n1", f"--nodelist={node}",
                    *extra, script])
    job_id = parse_job_id(out)
    assert job_id is not None, f"{label}: submit failed: {out}"
    jobs.append(job_id)
    wait_job_state(c, job_id, "R", timeout=_WAIT_RUN)
    return job_id


def _start_aggressor(c, jobs: list[int], extra=()) -> int:
    script = c.write_file("vo-aggressor.sh", _QUICK_SCRIPT)
    args = ["-J", "vo-aggressor", "-q", _HUNTER_QOS, "-N1", "--exclusive", *extra]
    extra = c.node_names[2:]
    if extra:
        args += ["-x", ",".join(extra)]
    job_id = parse_job_id(c.sbatch(args + [script]))
    assert job_id is not None, "aggressor submit failed"
    jobs.append(job_id)
    return job_id


def _assert_evicted(c, victims: dict[str, int], evicted: set[str], aggressor: int) -> None:
    """Exactly the `evicted` victims go, and the aggressor then runs to completion."""
    for label in sorted(evicted):
        state = wait_job(c, victims[label], timeout=_WAIT_PREEMPT)
        assert state in ("CA", "GONE"), f"{label} should be preempted, got {state!r}"
    final = wait_job(c, aggressor, timeout=_WAIT_RUN)
    assert final == "CD", f"aggressor should run on the freed node, got {final!r}"
    time.sleep(_GUARD_SECS)
    for label, job_id in victims.items():
        if label not in evicted:
            assert job_state(c.squeue_all(), job_id) == "R", f"{label} should still be running"


def _old_and_fresh(c, jobs, old_node: str, fresh_nodes: list[str]) -> dict[str, int]:
    """One old victim on `old_node`, then fresh ones `_AGE_GAP_SECS` later."""
    victims = {"old": _start_victim(c, jobs, "vo-old", old_node)}
    time.sleep(_AGE_GAP_SECS)
    for i, node in enumerate(fresh_nodes):
        victims[f"fresh{i}"] = _start_victim(c, jobs, f"vo-fresh{i}", node)
    return victims


class TestVictimOrder:

    @pytest.mark.parametrize("old_first", [True, False], ids=["old-on-node0", "old-on-node1"])
    def test_default_order_evicts_the_job_with_less_work(self, vo, old_first):
        c, jobs = vo
        n0, n1 = c.node_names[:2]
        old, fresh = (n0, n1) if old_first else (n1, n0)
        victims = _old_and_fresh(c, jobs, old, [fresh])
        aggressor = _start_aggressor(c, jobs)
        _assert_evicted(c, victims, {"fresh0"}, aggressor)

    @pytest.mark.parametrize("old_first", [True, False], ids=["old-on-node0", "old-on-node1"])
    def test_two_short_jobs_lose_less_than_one_long_one(self, vo, old_first):
        """The default spends two fresh jobs on one node before one old job."""
        c, jobs = vo
        n0, n1 = c.node_names[:2]
        old, fresh = (n0, n1) if old_first else (n1, n0)
        victims = _old_and_fresh(c, jobs, old, [fresh, fresh])
        aggressor = _start_aggressor(c, jobs)
        _assert_evicted(c, victims, {"fresh0", "fresh1"}, aggressor)

    @pytest.mark.parametrize("old_first", [True, False], ids=["old-on-node0", "old-on-node1"])
    def test_reconfigured_fewest_jobs_first_evicts_one_long_job(self, vo, old_first):
        """`scontrol reconfigure` swaps the order live: fewer evictions now win."""
        c, jobs = vo
        _set_order(c, ["fewest_jobs", "least_work", "hash"])
        n0, n1 = c.node_names[:2]
        old, fresh = (n0, n1) if old_first else (n1, n0)
        victims = _old_and_fresh(c, jobs, old, [fresh, fresh])
        aggressor = _start_aggressor(c, jobs)
        _assert_evicted(c, victims, {"old"}, aggressor)

    @pytest.mark.parametrize("old_first", [True, False], ids=["old-on-node0", "old-on-node1"])
    def test_empty_order_falls_back_to_node_name(self, vo, old_first):
        c, jobs = vo
        _set_order(c, [])
        n0, n1 = c.node_names[:2]
        old, fresh = (n0, n1) if old_first else (n1, n0)
        victims = _old_and_fresh(c, jobs, old, [fresh])
        aggressor = _start_aggressor(c, jobs)
        lowest = "old" if old == min(n0, n1) else "fresh0"
        _assert_evicted(c, victims, {lowest}, aggressor)

    @pytest.mark.parametrize("old_first", [True, False], ids=["old-on-node0", "old-on-node1"])
    def test_time_spent_suspended_is_not_lost_work(self, vo, old_first):
        """The old job sat suspended for most of its life, so it has less work to lose."""
        c, jobs = vo
        n0, n1 = c.node_names[:2]
        old, fresh = (n0, n1) if old_first else (n1, n0)
        resumed = _start_victim(c, jobs, "vo-resumed", old)
        c.scontrol("suspend", str(resumed))
        wait_job_state(c, resumed, "S", timeout=_WAIT_RUN)
        # Counting wall time, the resumed job would be the older one by a whole gap.
        time.sleep(_AGE_GAP_SECS)
        steady = _start_victim(c, jobs, "vo-steady", fresh)
        time.sleep(_AGE_GAP_SECS)
        c.scontrol("resume", str(resumed))
        wait_job_state(c, resumed, "R", timeout=_WAIT_RUN)
        aggressor = _start_aggressor(c, jobs)
        _assert_evicted(c, {"resumed": resumed, "steady": steady}, {"resumed"}, aggressor)


# A fixed path, so the config written before the agents start can name it.
_CDI_DIR = "/tmp/spur-e2e-vo-cdi"
_FAKE_MINOR = 250


def _render_minors(node) -> list[int]:
    out = node.exec_allow_fail("ls /dev/dri 2>/dev/null")
    return sorted(int(n[len("renderD"):]) for n in out.split() if n.startswith("renderD"))


def _gpu_spec(minors: list[int]) -> str:
    """A static AMD CDI spec, one GPU per render minor. The loader never opens the paths."""
    devices = [
        {
            "name": str(i),
            "annotations": {
                "spur.amd.com/render-minor": str(minor),
                "spur.amd.com/stable-id": str((i + 1) << 16),
                "spur.amd.com/gpu-type": "mi300x",
            },
            "containerEdits": {"deviceNodes": [{"path": f"/dev/dri/renderD{minor}"}]},
        }
        for i, minor in enumerate(minors)
    ]
    return json.dumps({"cdiVersion": "0.6.0", "kind": "amd.com/gpu", "devices": devices})


def _declare_gpus(c, node_index: int, count: int) -> bool:
    """Declares `count` GPUs on a node, real render minors first; True if all are real."""
    node = c.nodes[node_index]
    real = _render_minors(node)
    minors = (real + list(range(_FAKE_MINOR, _FAKE_MINOR + count)))[:count]
    node.exec(f"mkdir -p {_CDI_DIR}")
    node.write_file(f"{_CDI_DIR}/amd.json", _gpu_spec(minors))
    return len(real) >= count


class TestOneNodeGpuTotal:
    """`-N1 --gpus=2` may only empty a node with two GPUs. A static CDI spec declares 2 and
    1 GPUs whatever the bed has; least_work alone would pick the 1-GPU node's fresh victim."""

    @pytest.fixture
    def cluster_config_overrides(self, cluster_config_overrides):
        return {**cluster_config_overrides,
                "devices": {"auto_detect": False, "cdi_spec_dirs": [_CDI_DIR]}}

    @pytest.mark.parametrize("big_first", [True, False], ids=["big-is-node0", "big-is-node1"])
    def test_only_a_node_that_can_host_the_gpu_total_is_emptied(self, vo, big_first):
        c, jobs = vo
        big, small = (0, 1) if big_first else (1, 0)
        try:
            all_real = _declare_gpus(c, big, 2)
            _declare_gpus(c, small, 1)
            c.start_agents()
            names = c.node_names
            for index, want in ((big, 2), (small, 1)):
                deadline = time.time() + 60
                while c.node_gpu_count(names[index]) != want and time.time() < deadline:
                    time.sleep(2)
                got = c.node_gpu_count(names[index])
                if got != want:
                    pytest.skip(f"static CDI advertised {got} GPUs on {names[index]}, wanted {want}")

            victims = {"old": _start_victim(c, jobs, "vo-old", names[big], ["--exclusive"])}
            time.sleep(_AGE_GAP_SECS)
            victims["fresh"] = _start_victim(c, jobs, "vo-fresh", names[small], ["--exclusive"])
            aggressor = _start_aggressor(c, jobs, ["--gpus=2"])

            state = wait_job(c, victims["old"], timeout=_WAIT_PREEMPT)
            assert state in ("CA", "GONE"), f"the 2-GPU node's victim should go, got {state!r}"
            time.sleep(_GUARD_SECS)
            assert job_state(c.squeue_all(), victims["fresh"]) == "R", (
                "the 1-GPU node cannot host the job, so its victim must keep running"
            )
            show = c.scontrol("show", "job", str(aggressor))
            assert f"NodeList={names[small]}" not in show, show
            if all_real:
                final = wait_job(c, aggressor, timeout=_WAIT_RUN)
                assert final == "CD", f"aggressor should run on the freed node, got {final!r}"
        finally:
            for node in c.nodes[:2]:
                node.exec_allow_fail(f"rm -rf {_CDI_DIR}")
