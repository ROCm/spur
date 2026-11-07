# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for the node epilog's hold on a cancelled job's node.

The agent force-reclaims a job's supervisor a fixed 30s after a cancel. A
supervisor busy in the node epilog was reclaimed the same as a frozen one:
the node went back to the scheduler with the epilog still running, orphaned
from its supervisor, and the epilog's SPANK exit hooks never ran.

A supervisor in its epilog now holds a lease that defers the reclaim, the
epilog is bounded by ``[hooks] epilog_timeout_secs``, and a supervisor that is
lost mid-epilog anyway has its orphaned epilog killed and its node drained.
"""

from cluster import parse_job_id, wait_job, wait_job_state, wait_node_state, wait_until

# Longer than the 30s force-reclaim window, so a reclaim that ignored the
# lease would hand the node back well before the epilog ends.
SLOW_EPILOG_SECS = 45

TIMEOUT_EPILOG_SECS = 5

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

        supervisors = cluster.nodes[0].exec_allow_fail("pgrep -x spurstepd || true").split()
        assert supervisors, "a job in its epilog must still have a supervisor"
        cluster.nodes[0].exec(f"sudo kill -9 {' '.join(supervisors)}")
        assert _surviving(cluster, marker), (
            "killing the supervisor must leave the epilog orphaned, or this test "
            "proves nothing"
        )

        wait_node_state(cluster, node, ("drain",), timeout=90)
        assert not _surviving(cluster, marker), (
            "the agent must kill the epilog its lost supervisor left running"
        )
        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE", "F"), f"the job must still resolve, got {state}"
