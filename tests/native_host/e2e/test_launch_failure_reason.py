# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A supervisor that dies after the node accepts the job, before the script runs.

The batch script must not run, and the job's reason must say the launch failed
rather than looking like the script exited or was killed.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job


TASK_PROLOG_EXITS = """\
#!/bin/bash
echo "supervisor refused this launch" >&2
exit 1
"""


def _reason_line(cluster, job_id: int) -> str:
    show = cluster.scontrol("show", "job", str(job_id))
    for line in show.splitlines():
        if "Reason=" in line:
            print(f"scontrol job {job_id}: {line.strip()}")
            return line
    print(f"scontrol job {job_id}:\n{show}")
    return show


def _stdout_report(cluster, path: str) -> str:
    parts = []
    for index, node in enumerate(cluster.nodes):
        out = node.exec(
            f"if [ -e '{path}' ]; then wc -c < '{path}'; else echo MISSING; fi",
            check=False,
        )
        parts.append(f"node{index}={out.strip()}")
    report = " ".join(parts)
    print(f"stdout {path}: {report}")
    return report


def _marker_report(cluster, path: str) -> str:
    parts = []
    for index, node in enumerate(cluster.nodes):
        out = node.exec(
            f"if [ -e '{path}' ]; then echo PRESENT; else echo MISSING; fi",
            check=False,
        )
        parts.append(f"node{index}={out.strip()}")
    report = " ".join(parts)
    print(f"marker {path}: {report}")
    return report


def _sacct(cluster, job_id: int) -> str:
    if not cluster.accounting_enabled:
        print(f"sacct job {job_id}: accounting off")
        return ""
    deadline = time.time() + 30
    last = ""
    while time.time() < deadline:
        last = cluster.cli(
            [
                "spur",
                "sacct",
                "-j",
                str(job_id),
                "-n",
                "-o",
                "JobID,State,ExitCode,Reason,ReasonDetail",
            ]
        )
        if str(job_id) in last:
            print(f"sacct job {job_id}: {last.strip()}")
            return last
        time.sleep(2)
    print(f"sacct job {job_id}: no row yet\n{last}")
    return last


class TestLaunchFailureReason:
    def test_supervisor_death_is_not_a_script_failure(self, unstarted_cluster):
        cluster = unstarted_cluster
        try:
            cluster.enable_accounting()
        except RuntimeError as error:
            print(f"accounting unavailable: {error}")
        prolog = cluster.write_file(
            "hooks/task-prolog-refused.sh", TASK_PROLOG_EXITS, all_nodes=True
        )
        cluster.start({"hooks": {"task_prolog": prolog}})

        out_path = f"{cluster.remote_dir}/launch-refused.out"
        script = cluster.write_file(
            "never-runs.sh",
            "#!/bin/bash\n"
            "echo SCRIPT_RAN\n"
            "echo SCRIPT_RAN > /tmp/spur-launch-marker-$SPUR_JOB_ID\n",
        )
        submitted = cluster.sbatch(
            ["-J", "launch-refused", "-N", "1", "-o", out_path, script]
        )
        job_id = parse_job_id(submitted)
        assert job_id is not None
        state = wait_job(cluster, job_id, timeout=90)
        reason = _reason_line(cluster, job_id)
        stdout = _stdout_report(cluster, out_path)
        marker = _marker_report(cluster, f"/tmp/spur-launch-marker-{job_id}")
        accounting = _sacct(cluster, job_id)
        body = cluster.read_output_on_any_node(out_path)
        print(f"state={state} stdout_sizes={stdout} stdout_body={body!r}")

        assert state == "F", f"expected FAILED, got {state}"
        assert "SCRIPT_RAN" not in body
        assert "PRESENT" not in marker
        assert "JobLaunchFailure" in reason
        assert "RaisedSignal" not in reason
        if cluster.accounting_enabled:
            assert "JobLaunchFailure" in accounting
            assert "NonZeroExitCode" not in accounting

    def test_a_script_that_exits_stays_a_script_exit(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start()
        out_path = f"{cluster.remote_dir}/script-exit.out"
        script = cluster.write_file(
            "script-exits.sh", "#!/bin/bash\necho SCRIPT_RAN\nexit 1\n"
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "script-exit", "-N", "1", "-o", out_path, script])
        )
        assert job_id is not None
        state = wait_job(cluster, job_id, timeout=60)
        reason = _reason_line(cluster, job_id)
        body = cluster.read_output_on_any_node(out_path)
        print(f"state={state} stdout_body={body!r}")
        assert state == "F"
        assert "SCRIPT_RAN" in body
        assert "NonZeroExitCode" in reason
        assert "JobLaunchFailure" not in reason

    def test_a_cancelled_script_is_not_a_launch_failure(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start()
        script = cluster.write_file("script-sleeps.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "script-cancel", "-N", "1", script]))
        assert job_id is not None
        deadline = time.time() + 60
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) == "R":
                break
            time.sleep(1)
        cluster.scancel(str(job_id))
        state = wait_job(cluster, job_id, timeout=60)
        reason = _reason_line(cluster, job_id)
        print(f"state={state}")
        assert state == "CA"
        assert "JobLaunchFailure" not in reason

    def test_unwritable_output_is_a_launch_failure(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start()
        out_path = "/root/spur-denied-out.txt"
        marker_path = "/tmp/spur-denied-output-marker"
        for node in cluster.nodes:
            node.exec(f"rm -f '{marker_path}'", check=False)
        script = cluster.write_file(
            "denied-out.sh",
            "#!/bin/bash\n"
            "echo SCRIPT_RAN\n"
            f"echo SCRIPT_RAN > {marker_path}\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "denied-out", "-N", "1", "-o", out_path, script])
        )
        assert job_id is not None
        state = wait_job(cluster, job_id, timeout=90)
        reason = _reason_line(cluster, job_id)
        marker = _marker_report(cluster, marker_path)
        body = cluster.read_output_on_any_node(out_path)
        print(f"state={state} stdout_body={body!r}")
        assert state == "F"
        assert "SCRIPT_RAN" not in body
        assert "PRESENT" not in marker
        assert "JobLaunchFailure" in reason
        assert "RaisedSignal" not in reason

    def test_a_companion_launch_failure_does_not_relabel_the_script(self, unstarted_cluster):
        cluster = unstarted_cluster
        if len(cluster.nodes) < 2:
            pytest.skip("needs two nodes")
        prolog = cluster.write_file(
            "hooks/task-prolog-companion.sh",
            "#!/bin/bash\n"
            "if [ \"${SPUR_NODEID:-0}\" != \"0\" ]; then\n"
            "  echo \"companion refused this launch\" >&2\n"
            "  exit 1\n"
            "fi\n"
            "exit 0\n",
            all_nodes=True,
        )
        cluster.start({"hooks": {"task_prolog": prolog}})
        script = cluster.write_file(
            "companion-runs.sh",
            "#!/bin/bash\n"
            "echo SCRIPT_RAN\n"
            "echo SCRIPT_RAN > /tmp/spur-launch-marker-$SPUR_JOB_ID\n"
            "exit 1\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J",
                    "companion-fail",
                    "-N",
                    "2",
                    "-o",
                    f"{cluster.remote_dir}/companion.out",
                    script,
                ]
            )
        )
        assert job_id is not None
        state = wait_job(cluster, job_id, timeout=90)
        reason = _reason_line(cluster, job_id)
        show = cluster.scontrol("show", "job", str(job_id))
        print(show)
        marker = _marker_report(cluster, f"/tmp/spur-launch-marker-{job_id}")
        body = cluster.read_output_on_any_node(f"{cluster.remote_dir}/companion.out")
        print(f"state={state} stdout_body={body!r}")
        assert state == "F"
        assert "SCRIPT_RAN" in body
        assert "PRESENT" in marker
        assert "NonZeroExitCode" in reason
        assert "JobLaunchFailure" not in reason
