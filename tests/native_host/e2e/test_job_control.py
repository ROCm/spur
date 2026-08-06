# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for job control surfaces: requeue, scheduling windows, array
throttling, output modes, stdin, and feature constraints.

Array submission returns a parent placeholder id P and the task jobs get ids
P+1, P+2, ..., each carrying array_job_id = P. Tests address tasks by their
own job id, which is what scancel and squeue accept.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state


def _running_count(cluster, job_ids: list[int]) -> int:
    sq = cluster.squeue_all()
    return sum(1 for jid in job_ids if job_state(sq, jid) == "R")


class TestRequeue:
    def test_requeue_of_a_running_job_ends_it(self, cluster):
        """`scontrol requeue` today is cancel-for-resubmission, so the original
        run must actually stop."""
        script = cluster.write_file("rq-run.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "rq-run", script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=90)

        out = cluster.scontrol("requeue", str(job_id))
        assert "requeued" in out.lower(), f"unexpected requeue output:\n{out}"
        assert wait_job(cluster, job_id, timeout=90) in ("CA", "GONE"), (
            f"job {job_id} kept running after requeue:\n{cluster.squeue_all()}"
        )

    def test_requeue_of_a_terminal_job_is_rejected(self, cluster):
        script = cluster.write_file("rq-done.sh", "#!/bin/bash\ntrue\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "rq-done", script]))
        assert job_id is not None
        wait_job(cluster, job_id, timeout=90)

        out = cluster.cli_allow_fail(["scontrol", "requeue", str(job_id)])
        assert "already" in out.lower() or "error" in out.lower(), (
            f"requeueing a finished job must fail:\n{out}"
        )

    def test_requeue_of_an_unknown_job_is_rejected(self, cluster):
        out = cluster.cli_allow_fail(["scontrol", "requeue", "99999999"])
        assert "requeued" not in out.lower(), (
            f"an unknown job must not report success:\n{out}"
        )


class TestBeginTime:
    def test_future_begin_time_holds_the_job(self, cluster):
        script = cluster.write_file("begin.sh", "#!/bin/bash\necho BEGIN_OK\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "begin", "--begin", "now+1hours", script])
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "PD", timeout=30)
            time.sleep(10)
            assert job_state(cluster.squeue_all(), job_id) == "PD", (
                f"a job with a future begin time must stay pending:\n"
                f"{cluster.squeue_all()}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_begin_now_runs_immediately(self, cluster):
        out_path = f"{cluster.remote_dir}/begin-now.out"
        script = cluster.write_file("begin-now.sh", "#!/bin/bash\necho BEGIN_NOW_OK\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "begin-now", "--begin", "now", "-o", out_path, script]
            )
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=90) in ("CD", "GONE")
        assert "BEGIN_NOW_OK" in cluster.read_output_on_any_node(out_path)

    def test_begin_time_is_shown_by_scontrol(self, cluster):
        script = cluster.write_file("begin-show.sh", "#!/bin/bash\ntrue\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "begin-show", "--begin", "now+2hours", script])
        )
        assert job_id is not None
        try:
            detail = cluster.scontrol("show", "job", str(job_id))
            assert "BeginTime" in detail or "StartTime" in detail, (
                f"scontrol must surface the scheduling window:\n{detail}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_unparseable_begin_time_is_rejected(self, cluster):
        script = cluster.write_file("begin-bad.sh", "#!/bin/bash\ntrue\n")
        out = cluster.cli_allow_fail(
            ["sbatch", "-J", "begin-bad", "--begin", "teatime", script]
        )
        assert parse_job_id(out) is None, f"expected a rejection, got:\n{out}"
        assert "datetime" in out.lower() or "time" in out.lower(), (
            f"expected a time-format error:\n{out}"
        )


class TestDeadline:
    def test_past_deadline_moves_a_pending_job_to_deadline(self, cluster):
        """Deadline enforcement only applies to pending jobs, so the job is
        held first."""
        script = cluster.write_file("dl.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "dl", "-H", "--deadline", "2020-01-01T00:00:00Z", script]
            )
        )
        assert job_id is not None

        deadline = time.time() + 120
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) == "DL":
                return
            time.sleep(3)
        raise AssertionError(
            f"job {job_id} never reached DEADLINE:\n{cluster.squeue_all()}"
        )

    def test_deadline_state_is_reported_by_scontrol(self, cluster):
        script = cluster.write_file("dl-show.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "dl-show", "-H", "--deadline", "2020-01-01T00:00:00Z", script]
            )
        )
        assert job_id is not None

        stop = time.time() + 120
        while time.time() < stop:
            if job_state(cluster.squeue_all(), job_id) == "DL":
                break
            time.sleep(3)
        else:
            pytest.fail(f"job {job_id} never reached DEADLINE")

        detail = cluster.scontrol("show", "job", str(job_id))
        assert "DEADLINE" in detail or "DeadLine" in detail, (
            f"scontrol must report the deadline outcome:\n{detail}"
        )

    def test_generous_deadline_lets_the_job_run(self, cluster):
        out_path = f"{cluster.remote_dir}/dl-ok.out"
        script = cluster.write_file("dl-ok.sh", "#!/bin/bash\necho DEADLINE_OK\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "dl-ok", "--deadline", "now+4hours", "-o", out_path, script]
            )
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=90) in ("CD", "GONE")
        assert "DEADLINE_OK" in cluster.read_output_on_any_node(out_path)


class TestArrayThrottling:
    def test_percent_limit_caps_concurrent_tasks(self, cluster):
        """`%2` must hold the array to two running tasks even when the cluster
        could run all of them."""
        script = cluster.write_file(
            "arr-throttle.sh", "#!/bin/bash\nsleep 25\n", all_nodes=True
        )
        parent = parse_job_id(
            cluster.sbatch(["-J", "arr-throttle", "-N", "1", "-a", "0-5%2", script])
        )
        assert parent is not None
        tasks = [parent + 1 + i for i in range(6)]

        try:
            peak = 0
            deadline = time.time() + 60
            while time.time() < deadline:
                peak = max(peak, _running_count(cluster, tasks))
                if peak >= 2:
                    break
                time.sleep(2)
            assert peak >= 1, (
                f"no array task ever started:\n{cluster.squeue_all()}"
            )

            stop = time.time() + 45
            while time.time() < stop:
                running = _running_count(cluster, tasks)
                assert running <= 2, (
                    f"array throttle allowed {running} concurrent tasks:\n"
                    f"{cluster.squeue_all()}"
                )
                time.sleep(2)
        finally:
            for jid in tasks:
                cluster.cli_allow_fail(["scancel", str(jid)])

    def test_unthrottled_array_runs_tasks_in_parallel(self, cluster):
        """Control for the throttle test: without `%`, more than two tasks may
        run at once."""
        script = cluster.write_file(
            "arr-free.sh", "#!/bin/bash\nsleep 20\n", all_nodes=True
        )
        parent = parse_job_id(
            cluster.sbatch(["-J", "arr-free", "-N", "1", "-a", "0-3", script])
        )
        assert parent is not None
        tasks = [parent + 1 + i for i in range(4)]

        try:
            peak = 0
            deadline = time.time() + 60
            while time.time() < deadline:
                peak = max(peak, _running_count(cluster, tasks))
                if peak >= 3:
                    return
                time.sleep(2)
            pytest.skip(
                f"cluster only ran {peak} tasks concurrently; too small to "
                "distinguish throttled from unthrottled"
            )
        finally:
            for jid in tasks:
                cluster.cli_allow_fail(["scancel", str(jid)])

    def test_cancelling_one_element_leaves_the_others(self, cluster):
        script = cluster.write_file(
            "arr-cancel.sh", "#!/bin/bash\nsleep 45\n", all_nodes=True
        )
        parent = parse_job_id(
            cluster.sbatch(["-J", "arr-cancel", "-N", "1", "-a", "0-2", script])
        )
        assert parent is not None
        tasks = [parent + 1 + i for i in range(3)]

        try:
            wait_job_state(cluster, tasks[1], "R", timeout=90)
            cluster.cli(["scancel", str(tasks[1])])

            deadline = time.time() + 60
            while time.time() < deadline:
                if job_state(cluster.squeue_all(), tasks[1]) in ("CA", None):
                    break
                time.sleep(2)
            else:
                raise AssertionError(f"array element {tasks[1]} was not cancelled")

            sq = cluster.squeue_all()
            for other in (tasks[0], tasks[2]):
                assert job_state(sq, other) != "CA", (
                    f"cancelling {tasks[1]} must not touch {other}:\n{sq}"
                )
        finally:
            for jid in tasks:
                cluster.cli_allow_fail(["scancel", str(jid)])

    def test_invalid_array_limit_is_rejected(self, cluster):
        script = cluster.write_file("arr-bad.sh", "#!/bin/bash\ntrue\n")
        out = cluster.cli_allow_fail(
            ["sbatch", "-J", "arr-bad", "-a", "0-3%abc", script]
        )
        assert parse_job_id(out) is None, f"expected a rejection, got:\n{out}"
        assert "limit" in out.lower() or "invalid" in out.lower(), (
            f"expected an array-spec error:\n{out}"
        )


class TestOpenMode:
    def test_append_preserves_previous_output(self, cluster):
        out_path = f"{cluster.remote_dir}/open-append.out"
        script = cluster.write_file("open-append.sh", "#!/bin/bash\necho RUN\n")

        for _ in range(2):
            job_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-J",
                        "open-append",
                        "-o",
                        out_path,
                        "--open-mode=append",
                        script,
                    ]
                )
            )
            assert job_id is not None
            wait_job(cluster, job_id, timeout=90)

        content = cluster.read_output_on_any_node(out_path)
        assert content.count("RUN") == 2, (
            f"append mode must keep the first run's output:\n{content}"
        )

    def test_default_open_mode_truncates(self, cluster):
        out_path = f"{cluster.remote_dir}/open-trunc.out"
        script = cluster.write_file("open-trunc.sh", "#!/bin/bash\necho RUN\n")

        for _ in range(2):
            job_id = parse_job_id(
                cluster.sbatch(["-J", "open-trunc", "-o", out_path, script])
            )
            assert job_id is not None
            wait_job(cluster, job_id, timeout=90)

        content = cluster.read_output_on_any_node(out_path)
        assert content.count("RUN") == 1, (
            f"the default open mode must truncate:\n{content}"
        )


class TestStdinRedirection:
    def test_srun_input_file_is_piped_to_the_task(self, cluster):
        in_path = cluster.write_file(
            "stdin-input.txt", "STDIN_PAYLOAD\n", all_nodes=True, executable=False
        )
        code, out = cluster.srun_with_exit(["-N", "1", "-i", in_path, "cat"])
        assert code == 0, f"srun -i failed:\n{out}"
        assert "STDIN_PAYLOAD" in out, f"stdin was not forwarded:\n{out}"

    def test_srun_rejects_an_unreadable_input_file(self, cluster):
        code, out = cluster.srun_with_exit(
            ["-N", "1", "-i", f"{cluster.remote_dir}/no-such-input.txt", "cat"]
        )
        assert code != 0, f"a missing stdin file must fail:\n{out}"


class TestConstraints:
    @pytest.fixture
    def featured_cluster(self, unstarted_cluster):
        """Node 0 gets a distinguishing feature so -C can be seen to select."""
        cluster = unstarted_cluster
        cluster.start(
            config_overrides={
                "nodes": [
                    {
                        "names": name,
                        "cpus": 64,
                        "memory_mb": 262144,
                        "features": ["mi300x"] if i == 0 else ["cpuonly"],
                    }
                    for i, name in enumerate(cluster.node_names)
                ],
            }
        )
        return cluster

    def test_constraint_selects_a_matching_node(self, featured_cluster):
        cluster = featured_cluster
        out_path = f"{cluster.remote_dir}/constraint.out"
        script = cluster.write_file(
            "constraint.sh",
            '#!/bin/bash\necho "HOST=$(hostname)"\necho CONSTRAINT_OK\n',
            all_nodes=True,
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "constraint", "-C", "mi300x", "-o", out_path, script]
            )
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=90) in ("CD", "GONE")

        content = cluster.read_output_on_any_node(out_path)
        assert "CONSTRAINT_OK" in content, f"job produced no output:\n{content}"
        assert cluster.node_names[0] in content, (
            f"-C mi300x must land on the only matching node:\n{content}"
        )

    def test_unsatisfiable_constraint_reports_bad_constraints(self, featured_cluster):
        cluster = featured_cluster
        script = cluster.write_file("constraint-bad.sh", "#!/bin/bash\ntrue\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "constraint-bad", "-C", "nosuchfeature", script])
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "PD", timeout=60)
            reason = cluster.cli(["squeue", "-j", str(job_id), "-o", "%r", "-h"])
            assert "BadConstraints" in reason, (
                f"expected BadConstraints, got {reason!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_multiple_constraints_must_all_match(self, featured_cluster):
        cluster = featured_cluster
        script = cluster.write_file("constraint-and.sh", "#!/bin/bash\ntrue\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "constraint-and", "-C", "mi300x,cpuonly", script])
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "PD", timeout=60)
            reason = cluster.cli(["squeue", "-j", str(job_id), "-o", "%r", "-h"])
            assert "BadConstraints" in reason, (
                f"no node carries both features, expected BadConstraints, got {reason!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])
