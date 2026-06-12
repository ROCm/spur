# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for job suspend/resume (issue #270).

Minimal set chosen to cover each distinct scenario once:
  - happy-path state transitions (RUNNING -> SUSPENDED -> RUNNING)
  - whole process-tree freeze/thaw (batch shell AND its children)
  - run-time accounting excludes the suspended interval
  - time-limit is not consumed while suspended
  - node allocation is retained while suspended
  - squeue/scontrol display
  - CLI guard/error behavior (suspend pending, bad id)
  - cancel of a suspended job leaves no orphaned processes
  - suspend state survives a controller restart
  - multi-node suspend reaches every allocated node

Every test cancels its job in a finally block. A suspended job left behind
would hold its node allocation and wedge later tests sharing the cluster, so
cleanup must run even when an assertion fails.
"""

import time

from cluster import parse_job_id, job_state


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
        f"| grep -q 'sleep {sleep_secs} ' && echo $p; done"
    )


def _sleep_states(node, sleep_secs):
    """Process-state chars of THIS job's sleep procs (matched by unique secs)."""
    out = node.exec(
        f"for p in $({_job_sleep_pids_cmd(sleep_secs)}); do "
        "awk '{print $3}' /proc/$p/stat 2>/dev/null; done"
    )
    return out.split()


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
            node = cluster.nodes[0]
            assert _sleep_states(node, secs), "no sleep process found for this job"

            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            time.sleep(1)
            states = _sleep_states(node, secs)
            assert states and all(s.startswith("T") for s in states), (
                f"expected this job's sleep stopped (T), got {states}"
            )

            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R")
            time.sleep(1)
            states = _sleep_states(node, secs)
            assert states and all(not s.startswith("T") for s in states), (
                f"expected this job's sleep running after resume, got {states}"
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
            node = cluster.nodes[0]
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")

            cluster.scancel(str(job_id))
            job_id = None  # cancelled; nothing left for _cleanup
            deadline = time.time() + 15
            remaining = None
            while time.time() < deadline:
                remaining = node.exec(
                    f"({_job_sleep_pids_cmd(secs)}) | wc -l"
                ).strip()
                if remaining == "0":
                    break
                time.sleep(2)
            assert remaining == "0", (
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
            info = cluster.sinfo()
            assert ("alloc" in info or "mix" in info), (
                f"node should remain allocated while job suspended:\n{info}"
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

            states = _sleep_states(cluster.nodes[0], secs)
            assert states and all(s.startswith("T") for s in states), (
                f"primary node: expected stopped sleep, got {states}"
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
