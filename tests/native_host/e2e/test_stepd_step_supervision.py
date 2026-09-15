# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A numbered step gets its own supervisor, and a job reclaims what it made."""

import shlex
import time

from cluster import parse_job_id, wait_job, wait_job_state

RESERVED_STEP_MIN = 0xFFFF_FFF0


def _sessions(cluster, node_index: int = 0) -> list[str]:
    node = cluster.nodes[node_index]
    listing = node.exec_allow_fail(
        f"ls '{cluster.state_dir}/runtime' 2>/dev/null || true"
    )
    return [name for name in listing.split() if name[:1].isdigit()]


def _step_ids(sessions: list[str], job_id: int) -> set[int]:
    """Step ids of a job's sessions, named `<job>.<attempt>.<step>`."""
    ids = set()
    for name in sessions:
        parts = name.split(".")
        if len(parts) == 3 and parts[0] == str(job_id):
            ids.add(int(parts[2]))
    return ids


def _supervisor_pids(cluster, node_index: int = 0) -> set[str]:
    node = cluster.nodes[node_index]
    return set(node.exec_allow_fail("pgrep -x spurstepd || true").split())


def _wait_for_numbered_step(cluster, job_id: int, timeout: int = 90) -> set[int]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        numbered = {
            step
            for step in _step_ids(_sessions(cluster), job_id)
            if step < RESERVED_STEP_MIN
        }
        if numbered:
            return numbered
        time.sleep(2)
    return set()


class TestStepStartOrdering:
    def test_a_step_on_the_scripts_first_line_runs(self, cluster):
        # Coverage for the earliest a step can be launched, and for the release
        # RPC the job now waits on. Not a regression test for the start race:
        # the window is a single commit wide, so this passes against the
        # ordering it documents as well as the one it wants.
        node = cluster.node_names[0]
        script = cluster.write_file(
            "first-line-srun.sh",
            "#!/bin/bash\n"
            "scontrol show job $SPUR_JOB_ID | grep -o 'JobState=[A-Z]*' | head -1\n"
            "srun hostname\n"
            'echo "step-rc=$?"\n',
            all_nodes=True,
        )
        jobs = []
        for run in range(3):
            out_path = f"{cluster.remote_dir}/first-line-srun-{run}.out"
            job_id = parse_job_id(
                cluster.sbatch(
                    ["-J", f"first-srun-{run}", "-w", node, "-o", out_path, script]
                )
            )
            assert job_id is not None
            jobs.append((job_id, out_path))

        for job_id, out_path in jobs:
            assert wait_job(cluster, job_id, timeout=120) == "CD", (
                f"job {job_id} did not complete:\n{cluster.scontrol('show', 'job', str(job_id))}"
            )
            content = cluster.read_output_on_any_node(out_path)
            # The invariant, asserted directly rather than by whether the step
            # happened to win: by the time the script runs at all, the cluster
            # already agrees the job is running.
            assert "JobState=RUNNING" in content, (
                f"job {job_id} ran its script before the controller committed "
                f"it Running:\n{content}"
            )
            assert "step-rc=0" in content, (
                f"the first-line step was refused in job {job_id}:\n{content}"
            )


class TestNumberedStepSupervision:
    def test_a_numbered_step_gets_its_own_supervisor(self, cluster):
        # The step's own supervisor is what lets it outlive the agent; before
        # this a step ran as the agent's child and only the job had one.
        node = cluster.node_names[0]
        script = cluster.write_file(
            "step-sup.sh",
            "#!/bin/bash\nsrun sleep 30\n",
            all_nodes=True,
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "step-sup", "-w", node, script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        numbered = _wait_for_numbered_step(cluster, job_id)
        assert numbered, (
            f"job {job_id} never got a supervisor for its numbered step; "
            f"sessions were {_sessions(cluster)}"
        )
        assert _step_ids(_sessions(cluster), job_id) - numbered, (
            "the batch step keeps its own supervisor beside the numbered one"
        )

        cluster.scancel(str(job_id))
        wait_job(cluster, job_id, timeout=120)

    def test_a_numbered_step_survives_an_agent_restart(self, cluster):
        # The point of supervising steps: restarting the agent to upgrade it
        # must not kill the step, and its exit status must still arrive.
        node = cluster.node_names[0]
        out_path = f"{cluster.remote_dir}/step-survive.out"
        script = cluster.write_file(
            "step-survive.sh",
            "#!/bin/bash\nsrun bash -c 'sleep 40; echo STEP-FINISHED'\n"
            "echo \"step-rc=$?\"\n",
            all_nodes=True,
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "step-survive", "-w", node, "-o", out_path, script]
            )
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")
        assert _wait_for_numbered_step(cluster, job_id), (
            f"the step never got a supervisor; sessions: {_sessions(cluster)}"
        )
        # Identity, not count: a supervisor killed with the agent and respawned
        # afterwards would satisfy any "still one running" check.
        before = _supervisor_pids(cluster)

        cluster.restart_agent(0)

        assert before <= _supervisor_pids(cluster), (
            "the step's supervisor must outlive the agent that spawned it: "
            f"{sorted(before)} before, {sorted(_supervisor_pids(cluster))} after"
        )

        assert wait_job(cluster, job_id, timeout=180) == "CD"
        content = cluster.read_output_on_any_node(out_path)
        assert "STEP-FINISHED" in content, (
            f"the step did not run to completion:\n{content}"
        )
        # The agent reconnects to a step it no longer parented, so the status
        # has to come back over that reconnect rather than from a child wait.
        assert "step-rc=0" in content, (
            f"the step's exit status was lost across the restart:\n{content}"
        )


def _job_session_name(cluster, job_id: int) -> str | None:
    """The session of the step that owns the job's lifetime — the one that
    exists for as long as the job does, unlike a numbered step's."""
    for name in _sessions(cluster):
        parts = name.split(".")
        if len(parts) == 3 and parts[0] == str(job_id):
            if int(parts[2]) >= RESERVED_STEP_MIN:
                return name
    return None


def _stat_mode(cluster, path: str, node_index: int = 0) -> str:
    return cluster.nodes[node_index].exec_allow_fail(
        f"stat -c %a '{path}' 2>/dev/null"
    ).strip()


def _wait_for_record(cluster, session_dir: str, record: str, timeout: int = 90) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _stat_mode(cluster, f"{session_dir}/{record}"):
            return True
        time.sleep(2)
    return False


class TestSessionRecordPermissions:
    """A session record carries the capability the supervisor compares on every
    connection, plus the job's environment. Anyone who can read one can speak
    for the step, so the directory and its records are owner-only.
    """

    def test_a_sessions_records_are_owner_only(self, cluster):
        node = cluster.node_names[0]
        script = cluster.write_file(
            "session-perms.sh", "#!/bin/bash\nsleep 15\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "session-perms", "-w", node, script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        name = _job_session_name(cluster, job_id)
        assert name, f"job {job_id} has no session: {_sessions(cluster)}"
        session_dir = f"{cluster.state_dir}/runtime/{name}"

        assert _stat_mode(cluster, session_dir) == "700", (
            f"{session_dir} must be owner-only, got "
            f"{_stat_mode(cluster, session_dir)!r}"
        )
        for record in ("descriptor.json", "launch.json"):
            path = f"{session_dir}/{record}"
            assert _stat_mode(cluster, path) == "600", (
                f"{path} must be owner-only, got {_stat_mode(cluster, path)!r}"
            )
        # The modes only mean anything on a record that really holds the
        # credential, so confirm this one does.
        assert '"capability"' in cluster.nodes[0].read_file(
            f"{session_dir}/descriptor.json"
        ), f"{session_dir}/descriptor.json carries no capability to protect"

        # The obligation log is written when the payload exits, and a session
        # whose completion nothing acknowledged is kept rather than pruned.
        cluster.stop_agents()
        try:
            assert _wait_for_record(cluster, session_dir, "obligations.jsonl"), (
                f"the supervisor recorded no obligation under {session_dir}"
            )
            path = f"{session_dir}/obligations.jsonl"
            assert _stat_mode(cluster, path) == "600", (
                f"{path} must be owner-only, got {_stat_mode(cluster, path)!r}"
            )
        finally:
            # Bring the agents back so the orphaned supervisor is adopted and
            # reaped instead of outliving the test.
            cluster.start_agents()

    def test_a_settled_steps_retained_records_stay_owner_only(self, cluster):
        """A settled step's session is kept so a late caller still gets its
        exit — holding the job's environment for that whole window."""
        node = cluster.node_names[0]
        script = cluster.write_file(
            "settled-perms.sh",
            "#!/bin/bash\nsrun /bin/true\nsleep 20\n",
            all_nodes=True,
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "settled-perms", "-w", node, script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")
        numbered = _wait_for_numbered_step(cluster, job_id)
        assert numbered, f"the step never got a supervisor: {_sessions(cluster)}"

        step = sorted(numbered)[0]
        attempt = next(
            name.split(".")[1]
            for name in _sessions(cluster)
            if name.startswith(f"{job_id}.") and name.endswith(f".{step}")
        )
        session_dir = f"{cluster.state_dir}/runtime/{job_id}.{attempt}.{step}"

        assert _wait_for_record(cluster, session_dir, "obligations.jsonl"), (
            f"the step recorded no obligation under {session_dir}"
        )
        assert _stat_mode(cluster, session_dir) == "700", (
            f"{session_dir} must stay owner-only once settled, got "
            f"{_stat_mode(cluster, session_dir)!r}"
        )
        for record in ("descriptor.json", "launch.json"):
            path = f"{session_dir}/{record}"
            assert _stat_mode(cluster, path) == "600", (
                f"{path} must stay owner-only once settled, got "
                f"{_stat_mode(cluster, path)!r}"
            )

        cluster.scancel(str(job_id))
        wait_job(cluster, job_id, timeout=120)


def _job_cgroups(cluster, job_id: int, node_index: int = 0) -> list[str]:
    """This job's cgroups only. `/sys/fs/cgroup/spur` outlives any one cluster
    and job ids restart at 1, so an earlier run's leftovers are not ours."""
    node = cluster.nodes[node_index]
    listing = node.exec_allow_fail(
        f"ls -d /sys/fs/cgroup/spur/job_{job_id}_* 2>/dev/null || true"
    )
    return listing.split()


def _clear_stale_cgroups(cluster, node_index: int = 0) -> None:
    """`/sys/fs/cgroup/spur` is global while job ids restart at 1 per cluster,
    so an earlier run's leftovers are indistinguishable from this job's."""
    cluster.nodes[node_index].exec_allow_fail(
        "sudo rmdir /sys/fs/cgroup/spur/job_*/step_* /sys/fs/cgroup/spur/job_* "
        "2>/dev/null || true"
    )


class TestSupervisedJobReclaim:
    # cgroup_cluster, not cluster: an unprivileged agent creates no cgroups, so
    # these would compare two empty sets and pass whatever the code did.
    def test_a_finished_job_leaves_no_cgroup(self, cgroup_cluster):
        cluster = cgroup_cluster
        _clear_stale_cgroups(cluster)
        node = cluster.node_names[0]
        script = cluster.write_file(
            "reclaim.sh", "#!/bin/bash\nsleep 10\n", all_nodes=True
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "reclaim", "-w", node, script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        during = _job_cgroups(cluster, job_id)
        assert during, "a rootful agent must place the job in a cgroup"

        assert wait_job(cluster, job_id, timeout=120) == "CD"
        # Teardown runs after the job reaches a terminal state, so poll for it.
        deadline = time.time() + 60
        remaining = _job_cgroups(cluster, job_id)
        while remaining and time.time() < deadline:
            time.sleep(2)
            remaining = _job_cgroups(cluster, job_id)
        assert remaining == [], f"job {job_id} left cgroups behind: {remaining}"

    def test_an_allocation_leaves_no_cgroup(self, cgroup_cluster):
        # An allocation never reaches the completion teardown — it holds no
        # process to exit — so its cgroup was reclaimed by nothing.
        cluster = cgroup_cluster
        _clear_stale_cgroups(cluster)
        node = cluster.node_names[0]
        srun = " ".join(
            [
                f"SPUR_CONTROLLER_ADDR={shlex.quote(cluster.controller_addr)}",
                "nohup",
                shlex.quote(f"{cluster.bin_dir}/srun"),
                "-J", "alloc-reclaim", "-w", node, "-n", "1", "sleep", "10",
                ">/dev/null", "2>&1", "&", "echo", "$!",
            ]
        )
        assert cluster.nodes[0].exec(srun).strip().isdigit()

        # Identified while it still holds the allocation: a look-up afterwards
        # finds nothing to wait on.
        job_ids = []
        deadline = time.time() + 60
        while time.time() < deadline and not job_ids:
            job_ids = cluster.running_job_ids_by_name("alloc-reclaim")
            if not job_ids:
                time.sleep(1)
        assert job_ids, f"allocation never ran:\n{cluster.squeue_all()}"

        during = _job_cgroups(cluster, job_ids[0])
        assert during, "a rootful agent must place the allocation in a cgroup"

        wait_job(cluster, job_ids[0], timeout=120)
        deadline = time.time() + 60
        remaining = _job_cgroups(cluster, job_ids[0])
        while remaining and time.time() < deadline:
            time.sleep(2)
            remaining = _job_cgroups(cluster, job_ids[0])
        assert remaining == [], f"the allocation left cgroups behind: {remaining}"
