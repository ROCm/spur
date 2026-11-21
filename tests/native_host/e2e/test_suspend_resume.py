# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for job suspend/resume.

Covers RUNNING<->SUSPENDED transitions, whole process-tree freeze/thaw,
run-time/time-limit accounting while suspended, allocation retention, display,
CLI guards, cancel-of-suspended cleanup, restart persistence, and multi-node
dispatch. Each test cancels its job in a finally block.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_until


def _wait_state(cluster, job_id, want, timeout=30):
    """Poll squeue until the job reaches `want` (a state code like 'R'/'S').
    Returns True on success, False on timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if job_state(cluster.squeue_all(), job_id) == want:
            return True
        time.sleep(1)
    return False


def _cleanup(cluster, job_id):
    """Best-effort: resume (so a frozen tree can be killed) then cancel."""
    if job_id is None:
        return
    cluster.cli_allow_fail(["scontrol", "resume", str(job_id)])
    cluster.cli_allow_fail(["scancel", str(job_id)])


def _submit_sleep(cluster, name, out_path=None, extra=None, sleep_secs=600):
    """Submit a long-running batch job (bash -> sleep child) and return its id.

    The sleep duration doubles as a unique tag: pass a distinct sleep_secs per
    test so process-state checks target THIS job's sleep via
    `pgrep -f 'sleep <secs>'` and ignore unrelated sleeps from other tests
    sharing the node (CI runs many tests on the same cluster)."""
    body = f"#!/bin/bash\necho STARTED\nsleep {sleep_secs}\n"
    script = cluster.write_file(f"{name}.sh", body)
    args = ["-J", name, "-N", "1"]
    if out_path:
        args += ["-o", out_path]
    if extra:
        args += extra
    args.append(script)
    job_id = parse_job_id(cluster.sbatch(args))
    assert job_id is not None, "submit failed"
    assert _wait_state(cluster, job_id, "R", timeout=60), "job never reached RUNNING"
    return job_id


def _job_sleep_pids_cmd(sleep_secs):
    """Shell snippet listing pids of actual `sleep <secs>` processes.

    Filters `pgrep -x sleep` (exact binary name) by reading each cmdline for the
    unique duration. Avoids `pgrep -f 'sleep N'`, which self-matches the very
    shell running the check (its command line contains the literal string)."""
    return (
        "for p in $(pgrep -x sleep); do "
        f"tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null "
        f"| grep -qE 'sleep {sleep_secs}( |$)' && echo $p; done"
    )


def _sleep_states(nodes, sleep_secs):
    """State chars of THIS job's sleep procs (unique secs tag) across all nodes.

    A 1-node job may land on any node of a multi-node cluster, so search every
    node; the unique tag keeps the match to this job."""
    states = []
    for node in nodes:
        out = node.exec(
            f"for p in $({_job_sleep_pids_cmd(sleep_secs)}); do "
            "awk '{print $3}' /proc/$p/stat 2>/dev/null; done"
        )
        states.extend(out.split())
    return states


def _wait_sleep_states(nodes, sleep_secs, timeout=15):
    """Poll until the job's sleep process appears on some node, then return its
    state chars. The agent may spawn the sleep child a moment after the job
    flips to RUNNING, so wait rather than assume it is already up."""
    deadline = time.time() + timeout
    states = []
    while time.time() < deadline:
        states = _sleep_states(nodes, sleep_secs)
        if states:
            return states
        time.sleep(1)
    return states


def _wait_sleep_all(nodes, sleep_secs, pred, timeout=10):
    """Poll until the job's sleep procs exist and all satisfy `pred`. Signal
    delivery + scheduler state change is not instant."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        states = _sleep_states(nodes, sleep_secs)
        if states and all(pred(s) for s in states):
            return True
        time.sleep(1)
    return False


class TestSuspendResumeBasics:
    def test_suspend_then_resume_state_transitions(self, cluster):
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-basic")
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S"), "job did not reach SUSPENDED"
            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R"), "job did not return to RUNNING"
        finally:
            _cleanup(cluster, job_id)

    def test_suspend_resume_shown_in_scontrol(self, cluster):
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-show")
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=SUSPENDED" in show, f"scontrol show:\n{show}"
        finally:
            _cleanup(cluster, job_id)

    def test_multiple_suspend_resume_cycles(self, cluster):
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-cycles")
            for _ in range(3):
                cluster.scontrol("suspend", str(job_id))
                assert _wait_state(cluster, job_id, "S")
                cluster.scontrol("resume", str(job_id))
                assert _wait_state(cluster, job_id, "R")
        finally:
            _cleanup(cluster, job_id)


class TestSuspendProcessSemantics:
    def test_whole_process_tree_freezes_and_thaws(self, cluster):
        """SIGSTOP must reach the batch shell AND its sleep child (state 'T'),
        and SIGCONT must thaw both. Regression test for the managed-executor
        process-group fix."""
        job_id = None
        secs = 604  # unique tag for this job's sleep
        try:
            job_id = _submit_sleep(cluster, "sr-tree", sleep_secs=secs)
            nodes = cluster.nodes  # 1-node job may land on any node
            assert _wait_sleep_states(nodes, secs), "no sleep process found for this job"

            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            assert _wait_sleep_all(nodes, secs, lambda s: s.startswith("T")), (
                f"expected this job's sleep stopped (T), got {_sleep_states(nodes, secs)}"
            )

            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R")
            assert _wait_sleep_all(nodes, secs, lambda s: not s.startswith("T")), (
                f"expected this job's sleep running after resume, "
                f"got {_sleep_states(nodes, secs)}"
            )
        finally:
            _cleanup(cluster, job_id)

    def test_cancel_suspended_leaves_no_orphans(self, cluster):
        """Cancelling a suspended job must reap the whole tree (no orphaned
        sleep processes reparented to init)."""
        job_id = None
        secs = 605  # unique tag for this job's sleep
        try:
            job_id = _submit_sleep(cluster, "sr-orphan", sleep_secs=secs)
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")

            cluster.scancel(str(job_id))
            job_id = None  # cancelled; nothing left for _cleanup
            deadline = time.time() + 15
            remaining = None
            while time.time() < deadline:
                # 1-node job may land on any node — count across all of them.
                remaining = len(_sleep_states(cluster.nodes, secs))
                if remaining == 0:
                    break
                time.sleep(2)
            assert remaining == 0, (
                f"{remaining} orphaned sleep process(es) survived cancel"
            )
        finally:
            _cleanup(cluster, job_id)


class TestSuspendAccounting:
    def test_run_time_excludes_suspended_interval(self, cluster):
        """squeue TIME must not advance while the job is suspended."""
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-acct")
            time.sleep(6)
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            t_susp = _squeue_time(cluster, job_id)
            time.sleep(12)  # held suspended
            t_still = _squeue_time(cluster, job_id)
            assert t_still <= t_susp + 2, (
                f"run time advanced while suspended: {t_susp}s -> {t_still}s"
            )
        finally:
            _cleanup(cluster, job_id)

    def test_time_limit_not_consumed_while_suspended(self, cluster):
        """A job suspended past its wall-clock limit must NOT be killed while
        suspended; it should still be SUSPENDED after the limit elapses."""
        job_id = None
        try:
            out_path = f"{cluster.remote_dir}/sr-tl.out"
            job_id = _submit_sleep(
                cluster, "sr-tl", out_path=out_path, extra=["-t", "0:00:20"]
            )
            time.sleep(5)
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            time.sleep(30)  # well past the 20s limit
            state = job_state(cluster.squeue_all(), job_id)
            assert state == "S", (
                f"suspended job should survive its time limit, got {state}"
            )
        finally:
            _cleanup(cluster, job_id)


class TestSuspendScheduling:
    def test_allocation_retained_while_suspended(self, cluster):
        """A suspended job keeps its node; sinfo must not show the node idle."""
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-alloc")
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            states = cluster.sinfo_nodes()
            assert any(s.startswith(("alloc", "mix")) for s in states.values()), (
                f"node should remain allocated while job suspended:\n{states}"
            )
        finally:
            _cleanup(cluster, job_id)


class TestSuspendCliGuards:
    def test_suspend_pending_job_rejected(self, cluster):
        """Suspending a held (PENDING) job is rejected and leaves it PENDING."""
        job_id = None
        try:
            script = cluster.write_file("sr-pending.sh", "#!/bin/bash\necho PD\n")
            sb = cluster.sbatch(["-J", "sr-pending", "-N", "1", "-H", script])
            job_id = parse_job_id(sb)
            assert job_id is not None
            assert _wait_state(cluster, job_id, "PD", timeout=15)
            out = cluster.cli_allow_fail(["scontrol", "suspend", str(job_id)])
            assert job_state(cluster.squeue_all(), job_id) == "PD", (
                f"job should still be PENDING; cli said:\n{out}"
            )
        finally:
            _cleanup(cluster, job_id)

    def test_suspend_unknown_job_errors(self, cluster):
        out = cluster.cli_allow_fail(["scontrol", "suspend", "999999"])
        assert out.strip(), "expected an error message for unknown job id"


class TestSuspendPersistence:
    def test_suspend_survives_controller_restart(self, cluster):
        """After the controller restarts, a suspended job is still SUSPENDED
        (Raft log replay of JobSuspend)."""
        job_id = None
        try:
            job_id = _submit_sleep(cluster, "sr-restart")
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            cluster.restart_controller()
            assert _wait_state(cluster, job_id, "S", timeout=30), (
                "job should still be SUSPENDED after controller restart"
            )
            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R")
        finally:
            _cleanup(cluster, job_id)


class TestSuspendMultiNode:
    def test_multi_node_suspend_dispatches_to_all_agents(self, multi_node_cluster):
        """A multi-node job suspended must dispatch SuspendJob to EVERY allocated
        agent (controller fan-out), and the running workload must freeze.

        A plain `sbatch -N k` batch body runs only on the primary node (work
        distribution needs `srun`, a separate Spur gap), so we assert the
        *dispatch* reaches every agent — the multi-node behavior under test —
        plus that the primary node's process actually freezes.
        """
        cluster = multi_node_cluster
        job_id = None
        secs = 606  # unique tag for this job's sleep
        try:
            n_nodes = len(cluster.nodes)
            script = cluster.write_file(
                "sr-mn.sh", f"#!/bin/bash\necho STARTED\nsleep {secs}\n", all_nodes=True
            )
            sb = cluster.sbatch(["-J", "sr-mn", "-N", str(n_nodes), script])
            job_id = parse_job_id(sb)
            assert job_id is not None
            assert _wait_state(cluster, job_id, "R", timeout=60)

            show = cluster.scontrol("show", "job", str(job_id))
            assert f"NumNodes={n_nodes}" in show, f"expected {n_nodes}-node alloc:\n{show}"

            assert _wait_sleep_states([cluster.nodes[0]], secs), "no sleep process found"

            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            time.sleep(2)

            # The agent address is masked in CI logs, so assert the suspend
            # dispatch COUNT (resume=false) equals the node count rather than
            # matching hostnames. Strip ANSI color codes from the log first.
            dispatched = cluster.nodes[0].exec(
                f"sed 's/\\x1b\\[[0-9;]*m//g' '{cluster.log_dir}/spurctld.log' "
                f"| grep 'sent SuspendJob' | grep 'resume=false' "
                f"| grep -cE 'job_id={job_id}( |$|[^0-9])' || true"
            ).strip()
            assert dispatched == str(n_nodes), (
                f"expected {n_nodes} SuspendJob dispatches for job {job_id}, "
                f"got {dispatched}"
            )

            assert _wait_sleep_all([cluster.nodes[0]], secs, lambda s: s.startswith("T")), (
                f"primary node: expected stopped sleep, "
                f"got {_sleep_states([cluster.nodes[0]], secs)}"
            )
        finally:
            _cleanup(cluster, job_id)


def _squeue_time(cluster, job_id):
    """Return the squeue TIME column (seconds) for a job, or 0 if absent."""
    for line in cluster.squeue_all().splitlines()[1:]:
        fields = line.split()
        if fields and fields[0] == str(job_id):
            for f in fields:
                if ":" in f:
                    return _parse_hms(f)
    return 0


def _parse_hms(s):
    """Parse a Slurm-style elapsed time (MM:SS, HH:MM:SS, or D-HH:MM:SS)."""
    days = 0
    if "-" in s:
        d, s = s.split("-", 1)
        days = int(d)
    parts = [int(p) for p in s.split(":")]
    if len(parts) == 2:
        h, m, sec = 0, parts[0], parts[1]
    else:
        h, m, sec = parts[0], parts[1], parts[2]
    return days * 86400 + h * 3600 + m * 60 + sec


# A workload whose worker thread, not its main thread, starts a ticking helper process.
# Linux lists a child under the thread that forked it, so a signal delivered by walking the
# job's descendants through the main thread's children file never finds the helper.
_THREADED_WORKLOAD = """\
import subprocess
import sys
import threading

ticks, stop = sys.argv[1], sys.argv[2]
helper = (
    "while [ ! -e '%s' ]; do echo tick >> '%s'; sleep 1; done" % (stop, ticks)
)
worker = threading.Thread(target=lambda: subprocess.run(["/bin/sh", "-c", helper]))
worker.start()
worker.join()
"""

# The helper ticks once a second, so a window this long holds several ticks if it is running.
_TICK_WINDOW_SECS = 6
# Time for a stop to land and the helper's in-flight tick to finish.
_STOP_SETTLE_SECS = 3


def _tick_count(node, path):
    """Lines in the ticks file, 0 until it exists. The redirect is inside the group so the
    shell's own error for a missing file stays out of the captured output."""
    out = node.exec_allow_fail(f"{{ wc -l < '{path}'; }} 2>/dev/null || echo 0")
    return int(out.strip() or 0)


class TestSuspendWithoutCgroup:
    """Suspend and resume reach a helper started by a worker thread of the workload when
    the node has no job cgroup to list the job's processes from.

    Run as a regular user spurd the job is its own process group, and as root it runs
    behind an ``unshare`` launcher that a stop must not take with it, so the job's
    processes are found by walking its descendants instead.
    """

    @pytest.fixture(params=[False, True], ids=["user-spurd", "root-spurd"])
    def agent_as_root(self, request):
        return request.param

    @pytest.fixture
    def cluster_config_overrides(self):
        return {"auth": {"allow_root_jobs": True}, "cgroup": {"enabled": False}}

    def test_suspend_stops_and_resume_restarts_a_helper_a_worker_thread_started(self, cluster):
        node = cluster.nodes[0]
        assert node.exec_allow_fail("command -v python3").strip(), (
            "node 0 has no python3, which the multithreaded workload needs"
        )
        ticks = f"{cluster.remote_dir}/thread-ticks.txt"
        stop = f"{cluster.remote_dir}/thread-ticks.stop"
        program = cluster.write_file("thread-workload.py", _THREADED_WORKLOAD, executable=False)
        script = cluster.write_file(
            "thread-suspend.sh",
            f"#!/bin/bash\nexec python3 '{program}' '{ticks}' '{stop}'\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "thread-suspend", "-N", "1", "-w", cluster.node_names[0], script])
        )
        assert job_id is not None, "sbatch failed"
        try:
            assert _wait_state(cluster, job_id, "R", timeout=60), "job never reached RUNNING"
            wait_until(
                lambda: _tick_count(node, ticks) >= 3, timeout_s=30, interval_s=1,
                desc="the helper started by the worker thread to tick",
            )

            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S"), "job did not reach SUSPENDED"
            time.sleep(_STOP_SETTLE_SECS)
            while_stopped = _tick_count(node, ticks)
            time.sleep(_TICK_WINDOW_SECS)
            assert _tick_count(node, ticks) == while_stopped, (
                "the helper kept ticking while its job was suspended"
            )

            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R"), "job did not return to RUNNING"
            wait_until(
                lambda: _tick_count(node, ticks) > while_stopped, timeout_s=30, interval_s=1,
                desc="the helper to tick again after the resume",
            )

            node.exec(f"touch '{stop}'")
            assert wait_job(cluster, job_id, timeout=60) == "CD", (
                "the job must finish cleanly once the helper is told to stop\n"
                f"{cluster.debug_job(job_id)}"
            )
        finally:
            node.exec_allow_fail(f"touch '{stop}'")
            _cleanup(cluster, job_id)
