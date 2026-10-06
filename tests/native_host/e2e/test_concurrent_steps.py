# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Concurrent `srun` steps inside one allocation each get their own session."""

import time

RESERVED_STEP_MIN = 0xFFFF_FFF0
CONCURRENT_STEPS = 6


def _session_names(cluster) -> list[str]:
    names = []
    for node in cluster.nodes:
        listing = node.exec_allow_fail(
            f"ls '{cluster.state_dir}/runtime' 2>/dev/null || true"
        )
        names.extend(name for name in listing.split() if name[:1].isdigit())
    return names


def _numbered_step_ids(cluster, job_id: int) -> set[int]:
    ids = set()
    for name in _session_names(cluster):
        parts = name.split(".")
        if len(parts) != 3 or parts[0] != str(job_id):
            continue
        step_id = int(parts[2])
        if step_id < RESERVED_STEP_MIN:
            ids.add(step_id)
    return ids


class TestConcurrentStepLaunch:
    def test_concurrent_steps_each_run_exactly_once(self, cluster):
        marker = f"{cluster.remote_dir}/concurrent-steps-{time.time_ns()}.txt"
        job_file = f"{cluster.remote_dir}/concurrent-steps-job-{time.time_ns()}.txt"
        # Each child is waited on by pid: bare `wait` always returns 0, so a
        # step that died would not fail the allocation shell.
        body = (
            f'echo "$SPUR_JOB_ID" > {job_file}\n'
            "pids=()\n"
            f"for i in $(seq 1 {CONCURRENT_STEPS}); do\n"
            f'  srun -n1 bash -c "echo step-$i >> {marker}" &\n'
            "  pids+=($!)\n"
            "done\n"
            'for pid in "${pids[@]}"; do wait "$pid"; done\n'
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
