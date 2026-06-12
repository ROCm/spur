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
"""

import time

from cluster import parse_job_id, job_state, wait_job


def _wait_state(cluster, job_id, want, timeout=30):
    """Poll squeue until the job reaches `want` (a state code like 'R'/'S').
    Returns True on success, False on timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if job_state(cluster.squeue_all(), job_id) == want:
            return True
        time.sleep(1)
    return False


def _submit_sleep(cluster, name, out_path=None, extra=None):
    """Submit a long-running batch job (bash -> sleep child) and return its id."""
    body = "#!/bin/bash\necho STARTED\nsleep 600\n"
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


class TestSuspendResumeBasics:
    def test_suspend_then_resume_state_transitions(self, cluster):
        job_id = _submit_sleep(cluster, "sr-basic")

        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S"), "job did not reach SUSPENDED"

        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R"), "job did not return to RUNNING"

        cluster.scancel(str(job_id))

    def test_suspend_resume_shown_in_scontrol(self, cluster):
        job_id = _submit_sleep(cluster, "sr-show")
        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")

        show = cluster.scontrol("show", "job", str(job_id))
        assert "JobState=SUSPENDED" in show, f"scontrol show:\n{show}"

        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R")
        cluster.scancel(str(job_id))

    def test_multiple_suspend_resume_cycles(self, cluster):
        job_id = _submit_sleep(cluster, "sr-cycles")
        for _ in range(3):
            cluster.scontrol("suspend", str(job_id))
            assert _wait_state(cluster, job_id, "S")
            cluster.scontrol("resume", str(job_id))
            assert _wait_state(cluster, job_id, "R")
        cluster.scancel(str(job_id))


class TestSuspendProcessSemantics:
    def test_whole_process_tree_freezes_and_thaws(self, cluster):
        """SIGSTOP must reach the batch shell AND its sleep child (state 'T'),
        and SIGCONT must thaw both. Regression test for the managed-executor
        process-group fix."""
        job_id = _submit_sleep(cluster, "sr-tree")
        node = cluster.nodes[0]

        # All `sleep 600` children of the job should be running before suspend.
        assert node.exec("pgrep -x sleep || true").strip(), "no sleep process found"

        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")
        time.sleep(1)

        # Every sleep belonging to the job must be stopped (state starts with 'T').
        states = node.exec(
            "for p in $(pgrep -x sleep); do "
            "awk '{print $3}' /proc/$p/stat 2>/dev/null; done"
        ).split()
        assert states and all(s.startswith("T") for s in states), (
            f"expected all sleep children stopped (T), got {states}"
        )

        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R")
        time.sleep(1)
        states = node.exec(
            "for p in $(pgrep -x sleep); do "
            "awk '{print $3}' /proc/$p/stat 2>/dev/null; done"
        ).split()
        assert states and all(not s.startswith("T") for s in states), (
            f"expected sleep children running after resume, got {states}"
        )

        cluster.scancel(str(job_id))

    def test_cancel_suspended_leaves_no_orphans(self, cluster):
        """Cancelling a suspended job must reap the whole tree (no orphaned
        sleep processes reparented to init)."""
        job_id = _submit_sleep(cluster, "sr-orphan")
        node = cluster.nodes[0]
        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")

        cluster.scancel(str(job_id))
        deadline = time.time() + 15
        while time.time() < deadline:
            remaining = node.exec("pgrep -x sleep | wc -l").strip()
            if remaining == "0":
                break
            time.sleep(2)
        assert remaining == "0", f"{remaining} orphaned sleep process(es) survived cancel"


class TestSuspendAccounting:
    def test_run_time_excludes_suspended_interval(self, cluster):
        """squeue TIME must not advance while the job is suspended."""
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

        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R")
        cluster.scancel(str(job_id))

    def test_time_limit_not_consumed_while_suspended(self, cluster):
        """A job suspended past its wall-clock limit must NOT be killed while
        suspended; it should still be alive (SUSPENDED) after the limit elapses."""
        # 20s limit, suspend at ~5s, hold 30s (> limit), expect still SUSPENDED.
        out_path = f"{cluster.remote_dir}/sr-tl.out"
        job_id = _submit_sleep(cluster, "sr-tl", out_path=out_path, extra=["-t", "0:00:20"])
        time.sleep(5)
        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")

        time.sleep(30)  # well past the 20s limit
        state = job_state(cluster.squeue_all(), job_id)
        assert state == "S", f"suspended job should survive its time limit, got {state}"

        cluster.scontrol("resume", str(job_id))
        cluster.scancel(str(job_id))


class TestSuspendScheduling:
    def test_allocation_retained_while_suspended(self, cluster):
        """A suspended job keeps its node; sinfo must not show the node idle."""
        job_id = _submit_sleep(cluster, "sr-alloc")
        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")

        info = cluster.sinfo()
        assert ("alloc" in info or "mix" in info), (
            f"node should remain allocated while job suspended:\n{info}"
        )

        cluster.scontrol("resume", str(job_id))
        cluster.scancel(str(job_id))


class TestSuspendCliGuards:
    def test_suspend_pending_job_rejected(self, cluster):
        """Suspending a held (PENDING) job is rejected and leaves it PENDING."""
        script = cluster.write_file("sr-pending.sh", "#!/bin/bash\necho PD\n")
        sb = cluster.sbatch(["-J", "sr-pending", "-N", "1", "-H", script])
        job_id = parse_job_id(sb)
        assert job_id is not None
        assert _wait_state(cluster, job_id, "PD", timeout=15)

        out = cluster.cli_allow_fail(["scontrol", "suspend", str(job_id)])
        assert job_state(cluster.squeue_all(), job_id) == "PD", (
            f"job should still be PENDING; cli said:\n{out}"
        )
        cluster.scontrol("release", str(job_id))
        cluster.scancel(str(job_id))

    def test_suspend_unknown_job_errors(self, cluster):
        out = cluster.cli_allow_fail(["scontrol", "suspend", "999999"])
        assert out.strip(), "expected an error message for unknown job id"


class TestSuspendPersistence:
    def test_suspend_survives_controller_restart(self, cluster):
        """After the controller restarts, a suspended job is still SUSPENDED
        (Raft log replay of JobSuspend)."""
        job_id = _submit_sleep(cluster, "sr-restart")
        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")

        cluster.restart_controller()

        assert _wait_state(cluster, job_id, "S", timeout=30), (
            "job should still be SUSPENDED after controller restart"
        )
        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R")
        cluster.scancel(str(job_id))


class TestSuspendMultiNode:
    def test_multi_node_suspend_dispatches_to_all_agents(self, multi_node_cluster):
        """A 2-node job suspended must dispatch SuspendJob to EVERY allocated
        agent (controller fan-out), and the running workload must freeze.

        Note: a plain `sbatch -N 2` batch body runs only on the primary node
        (work distribution needs `srun`, a separate Spur gap), so we assert the
        *dispatch* reaches both agents — that is the multi-node behavior under
        test — plus that the primary node's process actually freezes.
        """
        cluster = multi_node_cluster
        script = cluster.write_file(
            "sr-mn.sh", "#!/bin/bash\necho STARTED\nsleep 600\n", all_nodes=True
        )
        sb = cluster.sbatch(["-J", "sr-mn", "-N", "2", script])
        job_id = parse_job_id(sb)
        assert job_id is not None
        assert _wait_state(cluster, job_id, "R", timeout=60)

        show = cluster.scontrol("show", "job", str(job_id))
        assert "NumNodes=2" in show, f"expected 2-node allocation:\n{show}"

        cluster.scontrol("suspend", str(job_id))
        assert _wait_state(cluster, job_id, "S")
        time.sleep(2)

        # Controller must have dispatched SuspendJob to BOTH allocated agents.
        ctrl_log = cluster.nodes[0].exec(
            f"grep 'sent SuspendJob' '{cluster.log_dir}/spurctld.log' || true"
        )
        for node in cluster.nodes:
            assert node.host in ctrl_log, (
                f"controller did not dispatch SuspendJob to {node.host}:\n{ctrl_log}"
            )

        # The actual workload (on the primary node) must be frozen.
        states = cluster.nodes[0].exec(
            "for p in $(pgrep -x sleep); do "
            "awk '{print $3}' /proc/$p/stat 2>/dev/null; done"
        ).split()
        assert states and all(s.startswith("T") for s in states), (
            f"primary node: expected stopped sleep, got {states}"
        )

        cluster.scontrol("resume", str(job_id))
        assert _wait_state(cluster, job_id, "R")
        cluster.scancel(str(job_id))


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
