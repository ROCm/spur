# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Concurrent `srun` steps inside one allocation.

Steps launched at the same instant must each get their own id and their own
supervisor session. Two steps sharing one session overwrite each other's
launch spec, descriptor and socket, which kills the whole job.
"""

import time

RESERVED_STEP_MIN = 0xFFFF_FFF0
CONCURRENT_STEPS = 6


def _session_names(cluster, node_index: int = 0) -> list[str]:
    node = cluster.nodes[node_index]
    listing = node.exec_allow_fail(
        f"ls '{cluster.state_dir}/runtime' 2>/dev/null || true"
    )
    return [name for name in listing.split() if name[:1].isdigit()]


def _numbered_step_ids(cluster, job_id: int) -> set[int]:
    ids = set()
    for name in _session_names(cluster):
        parts = name.split(".")
        if len(parts) == 3 and parts[0] == str(job_id) and int(parts[2]) < RESERVED_STEP_MIN:
            ids.add(int(parts[2]))
    return ids


class TestConcurrentStepLaunch:
    def test_concurrent_steps_each_run_exactly_once(self, cluster):
        marker = f"{cluster.remote_dir}/concurrent-steps-{time.time_ns()}.txt"
        job_file = f"{cluster.remote_dir}/concurrent-steps-job-{time.time_ns()}.txt"
        body = (
            f'echo "$SPUR_JOB_ID" > {job_file}\n'
            f"for i in $(seq 1 {CONCURRENT_STEPS}); do\n"
            f"  srun -n1 bash -c \"echo step-\\$i >> {marker}\" &\n"
            "done\n"
            "wait\n"
        )

        code, out = cluster.salloc_run(body, salloc_args=["-N", "1", "-t", "0:05"])
        assert code == 0, out

        content = cluster.read_output_on_any_node(marker)
        for index in range(1, CONCURRENT_STEPS + 1):
            assert content.count(f"step-{index}\n") == 1, (
                f"step-{index} must run exactly once, not zero or twice:\n{content}"
            )

        job_id = int(cluster.read_output_on_any_node(job_file).strip())
        step_ids = _numbered_step_ids(cluster, job_id)
        assert len(step_ids) == CONCURRENT_STEPS, (
            "every concurrent step needs its own supervisor session; "
            f"got {sorted(step_ids)} for {CONCURRENT_STEPS} steps"
        )

        job_info = cluster.scontrol("show", "job", str(job_id))
        assert "JobState=FAILED" not in job_info, (
            f"a duplicate step launch must not take the job down:\n{job_info}"
        )
        assert "JobState=NODE_FAIL" not in job_info, job_info
