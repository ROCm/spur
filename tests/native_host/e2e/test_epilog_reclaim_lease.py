# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for the node epilog's hold on a cancelled job's node.

The agent force-reclaims a job's supervisor a fixed 30s after a cancel. A
supervisor busy in the node epilog was reclaimed the same as a frozen one:
the node went back to the scheduler with the epilog still running, orphaned
from its supervisor, and the epilog's SPANK exit hooks never ran.

A supervisor in a long cgroup wait or in its epilog now holds a lease that
defers the reclaim, the epilog is bounded by ``[hooks] epilog_timeout_secs``,
and a supervisor lost after the job's exit but before its epilog finished has
any orphaned epilog killed and its node drained.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state, wait_node_state, wait_until

EPILOG_FAILED = "epilog script failed"
TEARDOWN_INTERRUPTED = "job teardown did not complete before its supervisor was lost"

# Longer than the 30s force-reclaim window, so a reclaim that ignored the
# lease would hand the node back well before the epilog ends.
SLOW_EPILOG_SECS = 45

TIMEOUT_EPILOG_SECS = 5

# Past the 30s force-reclaim window, and inside the cleanup timeout below.
CLEANUP_PINNED_SECS = 40
CLEANUP_TIMEOUT_SECS = 90
# Unique sleep duration so the cleanup pkill matches only this suite's processes.
PINNER_MARKER = 161803

SLOW_EPILOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
date +%s%N > "{RD}/hook-out/epilog-start-$SPUR_JOB_ID"
sleep {SECS}
date +%s%N > "{RD}/hook-out/epilog-end-$SPUR_JOB_ID"
"""

# The marker argument is unique to this suite so a stray process is
# attributable, and lets the tests ask whether the epilog's children survive.
HUNG_EPILOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
date +%s%N > "{RD}/hook-out/epilog-start-$SPUR_JOB_ID"
sleep {MARKER} &
wait
"""


QUICK_EPILOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
date +%s%N > "{RD}/hook-out/epilog-start-$SPUR_JOB_ID"
"""


# Refills the job's cgroup faster than the reap's poll can empty it, so the reap
# outlasts the force-reclaim window. Reaping without blocking keeps the refill
# rate steady; a shell loop slows as its background jobs pile up.
PINNER = f"""\
import os, signal
while not os.path.exists("{{stop}}"):
    pid = os.posix_spawn("/bin/sleep", ["sleep", "{PINNER_MARKER}"], os.environ)
    try:
        with open("{{procs}}", "w") as procs:
            procs.write(str(pid))
    except OSError:
        os.kill(pid, signal.SIGKILL)
    try:
        while os.waitpid(-1, os.WNOHANG)[0]:
            pass
    except ChildProcessError:
        pass
"""


def _install_epilog(cluster, body: str, **hooks) -> dict:
    """Write the epilog on every node; returns config overrides for ``start``."""
    rd = cluster.remote_dir
    cluster.write_file("hooks/epilog.sh", body.replace("{RD}", rd), all_nodes=True)
    return {"hooks": {"epilog": f"{rd}/hooks/epilog.sh", **hooks}}


def _read_marker(cluster, name: str) -> str:
    return cluster.nodes[0].read_file(f"{cluster.remote_dir}/hook-out/{name}").strip()


def _wait_marker(cluster, name: str, timeout_s: int = 60) -> str:
    return wait_until(
        lambda: _read_marker(cluster, name),
        timeout_s=timeout_s,
        interval_s=1,
        desc=f"marker {name}",
    )


def _node_reason(cluster, name: str) -> str:
    match = re.search(r"Reason=(.*)", cluster.scontrol_show_node(name))
    return match.group(1) if match else ""


def _surviving(cluster, marker: str) -> str:
    # `[s]` keeps pgrep from matching the shell running this very command.
    return cluster.nodes[0].exec_allow_fail(f"pgrep -f '[s]leep {marker}' || true").strip()


class TestEpilogHoldsTheNode:
    def test_a_cancelled_jobs_node_is_not_released_before_its_slow_epilog_ends(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        cluster.start(
            _install_epilog(cluster, SLOW_EPILOG.replace("{SECS}", str(SLOW_EPILOG_SECS)))
        )
        rd = cluster.remote_dir
        node = cluster.node_names[0]
        victim = cluster.write_file("lease-victim.sh", "#!/bin/bash\nsleep 300\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "lease-victim", "-w", node, "--exclusive", victim])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        cluster.scancel(job_id)
        _wait_marker(cluster, f"epilog-start-{job_id}")

        probe_script = cluster.write_file(
            "lease-probe.sh",
            f'#!/bin/bash\nmkdir -p "{rd}/hook-out"\ndate +%s%N > "{rd}/hook-out/probe-start"\n',
        )
        probe = parse_job_id(
            cluster.sbatch(["-J", "lease-probe", "-w", node, "--exclusive", probe_script])
        )
        assert probe is not None
        wait_job(cluster, probe, timeout=SLOW_EPILOG_SECS + 120)

        epilog_end = int(_read_marker(cluster, f"epilog-end-{job_id}"))
        probe_start = int(_read_marker(cluster, "probe-start"))
        assert epilog_end <= probe_start, (
            "the node was handed to the next job while the cancelled job's epilog "
            f"was still running (epilog ended {(epilog_end - probe_start) / 1e9:.1f}s "
            "after the next job started)"
        )
        state = cluster.sinfo_nodes()[node]
        assert not state.startswith("drain"), (
            f"a slow epilog that finishes within its timeout must not drain the node: {state}"
        )


class TestEpilogTimeout:
    def test_an_epilog_past_its_timeout_is_killed_whole_and_the_node_drained(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        marker = "314159"
        cluster.start(
            _install_epilog(
                cluster,
                HUNG_EPILOG.replace("{MARKER}", marker),
                epilog_timeout_secs=TIMEOUT_EPILOG_SECS,
            )
        )
        node = cluster.node_names[0]
        script = cluster.write_file("epilog-timeout.sh", "#!/bin/bash\nexit 0\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "epilog-timeout", "-w", node, "--exclusive", script])
        )
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=120)
        assert state in ("CD", "GONE"), f"expected the job to complete, got {state}"

        wait_node_state(cluster, node, ("drain",), timeout=60)
        assert EPILOG_FAILED in _node_reason(cluster, node)
        assert not _surviving(cluster, marker), (
            "the timed-out epilog's children must die with its process group"
        )


class TestSupervisorLostMidEpilog:
    def test_the_orphaned_epilog_is_killed_and_the_node_drained(self, unstarted_cluster):
        cluster = unstarted_cluster
        marker = "271828"
        cluster.start(_install_epilog(cluster, HUNG_EPILOG.replace("{MARKER}", marker)))
        node = cluster.node_names[0]
        script = cluster.write_file("epilog-orphan.sh", "#!/bin/bash\nexit 0\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "epilog-orphan", "-w", node, "--exclusive", script])
        )
        assert job_id is not None
        _wait_marker(cluster, f"epilog-start-{job_id}")
        # Await the epilog's child first: once the supervisor dies, the agent
        # may kill it at once, leaving no window to observe the orphan.
        wait_until(
            lambda: _surviving(cluster, marker),
            timeout_s=30,
            interval_s=1,
            desc="the epilog's child",
        )

        supervisors = cluster.nodes[0].exec_allow_fail("pgrep -x spurstepd || true").split()
        assert supervisors, "a job in its epilog must still have a supervisor"
        cluster.nodes[0].exec(f"{cluster._sudo_prefix()}kill -9 {' '.join(supervisors)}")

        wait_node_state(cluster, node, ("drain",), timeout=90)
        assert TEARDOWN_INTERRUPTED in _node_reason(cluster, node)
        wait_until(
            lambda: not _surviving(cluster, marker),
            timeout_s=30,
            interval_s=1,
            desc="the agent to kill the epilog its lost supervisor left running",
        )
        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE", "F"), f"the job must still resolve, got {state}"


class TestCleanupHoldsTheNode:
    def test_a_supervisor_in_a_long_cgroup_wait_still_runs_its_epilog(self, unstarted_cluster):
        cluster = unstarted_cluster
        node = cluster.nodes[0]
        name = cluster.node_names[0]
        fstype = node.exec_allow_fail("stat -fc %T /sys/fs/cgroup").strip()
        if "cgroup2fs" not in fstype:
            pytest.skip(f"node 0 is not running cgroup v2 (/sys/fs/cgroup is {fstype!r})")
        if not node.exec_allow_fail("command -v python3").strip():
            pytest.skip("node 0 has no python3 to run the cgroup pinner")
        sudo = cluster._sudo_prefix()
        overrides = _install_epilog(cluster, QUICK_EPILOG)
        # A root agent only runs a root-owned hook.
        for each in cluster.nodes:
            each.exec(f"{sudo}chown root:root {cluster.remote_dir}/hooks/epilog.sh")
        overrides["cgroup"] = {"cleanup_timeout_secs": CLEANUP_TIMEOUT_SECS}
        cluster.start(overrides, agent_as_root=True)

        cg_file = f"{cluster.remote_dir}/pinned-cgroup.path"
        script = cluster.write_file(
            "pinned-cleanup.sh",
            "#!/bin/bash\n"
            "while IFS= read -r line; do\n"
            f'  case "$line" in 0::*) echo "${{line#0::}}" > {cg_file} ;; esac\n'
            "done < /proc/self/cgroup\n"
            "sleep 600\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "pinned-cleanup", "-w", name, script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")
        cgroup = wait_until(
            lambda: node.exec_allow_fail(f"cat {cg_file} 2>/dev/null").strip(),
            timeout_s=30, desc="job cgroup path",
        )

        stop = f"{cluster.remote_dir}/pinner.stop"
        pinner = cluster.write_file(
            "pinner.py",
            PINNER.format(procs=f"/sys/fs/cgroup{cgroup}/cgroup.procs", stop=stop),
        )
        try:
            node.exec(f"{sudo}setsid nohup python3 {pinner} > /dev/null 2>&1 &")
            cluster.scancel(job_id)
            time.sleep(CLEANUP_PINNED_SECS)
            assert node.exec_allow_fail("pgrep -x spurstepd || true").strip(), (
                "a supervisor still inside its cleanup timeout must not be reclaimed"
            )
            node.exec(f"touch {stop}")

            _wait_marker(cluster, f"epilog-start-{job_id}", timeout_s=60)
            assert wait_job(cluster, job_id, timeout=60) in ("CA", "GONE")
            state = cluster.sinfo_nodes()[name]
            assert not state.startswith("drain"), (
                "a cleanup that finishes within its timeout must not drain the node: "
                f"{state} ({_node_reason(cluster, name)})"
            )
        finally:
            node.exec_allow_fail(f"touch {stop}")
            node.exec_allow_fail(f"{sudo}pkill -f '[s]leep {PINNER_MARKER}' || true")
