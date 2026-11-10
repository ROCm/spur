# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Controller log recovery preserves a job's original event times."""

import re
import shlex

from cluster import parse_job_id, wait_job, wait_job_state


def _job_fields(cluster, job_id):
    output = cluster.scontrol("show", "job", str(job_id))
    fields = dict(re.findall(r"\b(\w+)=(\S+)", output))
    for name in ("SubmitTime", "StartTime", "EndTime", "RunTime", "JobState", "Restarts"):
        assert name in fields, output
    return fields


def _submit(cluster, name, *args):
    output = cluster.sbatch(["-J", name, "-w", cluster.node_names[0], *args])
    job_id = parse_job_id(output)
    assert job_id is not None, output
    return job_id


def test_job_timestamps_survive_controller_log_recovery(cluster):
    completed = _submit(cluster, "timestamps-completed", "--wrap", "exit 0")
    assert wait_job(cluster, completed) == "CD"
    pending = _submit(cluster, "timestamps-pending", "--hold", "--wrap", "exit 0")
    release = f"{cluster.remote_dir}/release-timestamp-job"
    output = f"{cluster.remote_dir}/timestamp-job.out"
    script = cluster.write_file(
        "timestamp-job.sh",
        "#!/bin/bash\nset -eu\necho READY\n"
        f"while [ ! -f {shlex.quote(release)} ]; do sleep 0.1; done\n"
        "echo DONE\n",
    )
    running = _submit(cluster, "timestamps-running", "-t", "00:05:00", "-o", output, script)
    wait_job_state(cluster, running, "R")
    cluster.wait_output(output, "READY")
    jobs = (pending, running, completed)
    before = {job_id: _job_fields(cluster, job_id) for job_id in jobs}
    assert [before[job_id]["JobState"] for job_id in jobs] == [
        "PENDING", "RUNNING", "COMPLETED",
    ]

    # A snapshot could preserve times even when the log replay path loses them.
    cluster.nodes[0].exec(f"test ! -f {shlex.quote(cluster.state_dir + '/raft/snapshot.json')}")
    cluster.restart_controller()
    for job_id, state in ((pending, "PD"), (running, "R"), (completed, "CD")):
        wait_job_state(cluster, job_id, state)

    for job_id in jobs:
        after = _job_fields(cluster, job_id)
        for field in ("SubmitTime", "StartTime", "JobState", "Restarts"):
            assert after[field] == before[job_id][field], (job_id, field, before[job_id], after)
        assert after["Restarts"] == "0", after
        if job_id == completed:
            for field in ("EndTime", "RunTime"):
                assert after[field] == before[job_id][field], (field, before[job_id], after)

    cluster.nodes[0].exec(f"touch {shlex.quote(release)}")
    assert wait_job(cluster, running) == "CD"
    cluster.scontrol("release", str(pending))
    assert wait_job(cluster, pending) == "CD"
    finished = {job_id: _job_fields(cluster, job_id) for job_id in jobs}
    for job_id in jobs:
        assert finished[job_id]["SubmitTime"] == before[job_id]["SubmitTime"]
    assert finished[running]["StartTime"] == before[running]["StartTime"]

    cluster.nodes[0].exec(f"test ! -f {shlex.quote(cluster.state_dir + '/raft/snapshot.json')}")
    cluster.restart_controller()
    for job_id in jobs:
        assert wait_job(cluster, job_id) == "CD"
        after = _job_fields(cluster, job_id)
        for field in ("SubmitTime", "StartTime", "EndTime", "RunTime", "Restarts"):
            assert after[field] == finished[job_id][field], (job_id, field, finished[job_id], after)
        assert after["JobState"] == "COMPLETED", after
    content = cluster.nodes[0].read_file(output)
    assert content.splitlines() == ["READY", "DONE"], content
