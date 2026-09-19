# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for release fences during terminal job transitions.

Every test holds the node epilog after the workload has exited but before the
agent has released its local allocation. The terminal transition has already
released ordinary controller capacity. A replacement must stay out of the
agent until the epilog completes and the agent confirms the release.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}


def _install_blocking_epilog(cluster, node_index: int) -> tuple[str, str]:
    """Install an epilog that blocks once, only on ``node_index``."""
    hook = f"{cluster.remote_dir}/hooks/blocking-epilog.sh"
    entered = f"{cluster.remote_dir}/epilog-entered"
    release = f"{cluster.remote_dir}/epilog-release"
    for index, node in enumerate(cluster.nodes):
        node.exec(f"mkdir -p '{cluster.remote_dir}/hooks'")
        if index == node_index:
            node.write_file(
                hook,
                "#!/bin/bash\n"
                f"if [ ! -e '{entered}' ]; then\n"
                f"  touch '{entered}'\n"
                f"  while [ ! -e '{release}' ]; do sleep 0.1; done\n"
                "fi\n",
                mode=0o755,
            )
        else:
            node.write_file(hook, "#!/bin/bash\n", mode=0o755)
    return entered, release


def _wait_for_file(cluster, node_index: int, path: str, timeout: int = 90):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if (
            cluster.nodes[node_index]
            .exec_allow_fail(f"test -e '{path}' && printf present")
            .strip()
            == "present"
        ):
            return
        time.sleep(1)
    raise AssertionError(f"timed out waiting for {path} on {cluster.node_names[node_index]}")


def _assert_not_dispatched_while_fenced(cluster, node_index: int, job_id: int):
    """Prove several scheduler passes did not send a replacement to the agent."""
    deadline = time.time() + 7
    launch_request = f"job_id={job_id}"
    while time.time() < deadline:
        log = cluster.spurd_log(node_index)
        if any(
            launch_request in line and "received job launch request" in line
            for line in log.splitlines()
        ):
            raise AssertionError(
                "replacement was sent to the agent before the old allocation "
                "was released"
            )
        time.sleep(1)


def _assert_none_dispatched_while_fenced(cluster, node_index: int, job_ids: list[int]):
    """Prove competing replacements never reach the agent before release."""
    deadline = time.time() + 7
    launch_requests = {f"job_id={job_id}" for job_id in job_ids}
    while time.time() < deadline:
        log = cluster.spurd_log(node_index)
        for line in log.splitlines():
            if "received job launch request" not in line:
                continue
            if any(request in line for request in launch_requests):
                raise AssertionError(
                    "a GPU replacement was sent to the agent before the old "
                    "allocation was released"
                )
        time.sleep(1)


def _submit_replacement(cluster, node_name: str, name: str) -> int:
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\necho REPLACED\n")
    job_id = parse_job_id(
        cluster.sbatch(
            ["-J", name, "-N", "1", "--exclusive", f"--nodelist={node_name}", script]
        )
    )
    assert job_id is not None, f"replacement submission {name} failed"
    cluster.scontrol("update", f"JobId={job_id}", "Priority=1000000")
    return job_id


def _assert_replacement_waits_then_runs(
    cluster, node_index: int, release: str, replacement_id: int
):
    wait_job_state(cluster, replacement_id, "PD", timeout=30)
    _assert_not_dispatched_while_fenced(cluster, node_index, replacement_id)
    cluster.nodes[node_index].exec(f"touch '{release}'")
    state = wait_job(cluster, replacement_id, timeout=90)
    assert state in ("CD", "GONE"), f"replacement did not run after release: {state}"


class TestReleaseQuarantine:
    def test_gpu_cancel_fences_competing_replacements_until_agent_release(
        self, unstarted_gpu_cluster
    ):
        """A stale GPU ledger must not turn competing successors into failures."""
        cluster = unstarted_gpu_cluster
        cluster.gpu_preflight(1)
        target_index = next(
            (
                index
                for index, node in enumerate(cluster.nodes)
                if "HAS_GPU"
                in node.exec_allow_fail(
                    "{ test -e /dev/kfd && echo HAS_GPU; } || "
                    "{ nvidia-smi -L 2>/dev/null && echo HAS_GPU; }"
                )
            ),
            None,
        )
        assert target_index is not None, "GPU preflight did not identify a target node"
        entered, release = _install_blocking_epilog(cluster, target_index)
        cluster.start(
            {
                **_AUTH_ROOT,
                "hooks": {"epilog": f"{cluster.remote_dir}/hooks/blocking-epilog.sh"},
            }
        )
        target = cluster.node_names[target_index]
        gpu_count = cluster.node_gpu_count(target)
        assert gpu_count > 0, "GPU hardware was not registered with the scheduler"

        original_id = None
        replacement_ids: list[int] = []
        try:
            original = cluster.write_file("gpu-cancel-original.sh", "#!/bin/bash\nsleep 600\n")
            original_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-J",
                        "gpu-cancel-original",
                        "-N",
                        "1",
                        "--exclusive",
                        f"--nodelist={target}",
                        f"--gres=gpu:{gpu_count}",
                        original,
                    ]
                )
            )
            assert original_id is not None
            wait_job_state(cluster, original_id, "R", timeout=60)

            cluster.cli(["scancel", str(original_id)])
            _wait_for_file(cluster, target_index, entered)

            for sequence in range(3):
                script = cluster.write_file(
                    f"gpu-cancel-replacement-{sequence}.sh", "#!/bin/bash\necho REPLACED\n"
                )
                replacement_id = parse_job_id(
                    cluster.sbatch(
                        [
                            "-J",
                            f"gpu-cancel-replacement-{sequence}",
                            "-N",
                            "1",
                            "--exclusive",
                            f"--nodelist={target}",
                            f"--gres=gpu:{gpu_count}",
                            script,
                        ]
                    )
                )
                assert replacement_id is not None
                cluster.scontrol("update", f"JobId={replacement_id}", "Priority=1000000")
                replacement_ids.append(replacement_id)

            for replacement_id in replacement_ids:
                wait_job_state(cluster, replacement_id, "PD", timeout=30)
            _assert_none_dispatched_while_fenced(
                cluster, target_index, replacement_ids
            )

            cluster.nodes[target_index].exec(f"touch '{release}'")
            state = wait_job(cluster, replacement_ids[0], timeout=90)
            assert state in ("CD", "GONE"), (
                "GPU replacement did not run after the stale allocation was released: "
                f"{state}"
            )
        finally:
            cluster.nodes[target_index].exec_allow_fail(f"touch '{release}'")
            for job_id in (original_id, *replacement_ids):
                if job_id is not None:
                    cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_user_cancel_keeps_replacement_out_until_agent_release(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        target_index = 0
        target = cluster.node_names[target_index]
        entered, release = _install_blocking_epilog(cluster, target_index)
        cluster.start(
            {
                **_AUTH_ROOT,
                "hooks": {"epilog": f"{cluster.remote_dir}/hooks/blocking-epilog.sh"},
            }
        )

        original_id = replacement_id = None
        try:
            original = cluster.write_file("cancel-original.sh", "#!/bin/bash\nsleep 600\n")
            original_id = parse_job_id(
                cluster.sbatch(
                    ["-J", "cancel-original", "-N", "1", "--exclusive", f"--nodelist={target}", original]
                )
            )
            assert original_id is not None
            wait_job_state(cluster, original_id, "R", timeout=60)

            cluster.cli(["scancel", str(original_id)])
            _wait_for_file(cluster, target_index, entered)
            replacement_id = _submit_replacement(cluster, target, "cancel-replacement")
            _assert_replacement_waits_then_runs(cluster, target_index, release, replacement_id)
        finally:
            cluster.nodes[target_index].exec_allow_fail(f"touch '{release}'")
            for job_id in (original_id, replacement_id):
                if job_id is not None:
                    cluster.cli_allow_fail(["scancel", str(job_id)])

    @pytest.mark.parametrize("preempt_mode", ("cancel", "requeue"))
    def test_preemption_keeps_preemptor_out_until_agent_release(
        self, unstarted_cluster, preempt_mode
    ):
        cluster = unstarted_cluster
        target_index = 0
        target = cluster.node_names[target_index]
        entered, release = _install_blocking_epilog(cluster, target_index)
        cluster.start(
            {
                **_AUTH_ROOT,
                "hooks": {"epilog": f"{cluster.remote_dir}/hooks/blocking-epilog.sh"},
                "partitions": [
                    {
                        "name": "default",
                        "state": "UP",
                        "default": True,
                        "nodes": "ALL",
                        "max_time": "24:00:00",
                        "default_time": "10:00",
                        "preempt_mode": preempt_mode,
                    }
                ],
            }
        )

        victim_id = preemptor_id = None
        try:
            victim = cluster.write_file("preempt-victim.sh", "#!/bin/bash\nsleep 600\n")
            victim_id = parse_job_id(
                cluster.sbatch(
                    ["-J", "preempt-victim", "-N", "1", "--exclusive", f"--nodelist={target}", victim]
                )
            )
            assert victim_id is not None
            wait_job_state(cluster, victim_id, "R", timeout=60)

            preemptor_id = _submit_replacement(
                cluster, target, f"{preempt_mode}-preemptor"
            )
            _wait_for_file(cluster, target_index, entered)
            _assert_replacement_waits_then_runs(cluster, target_index, release, preemptor_id)
        finally:
            cluster.nodes[target_index].exec_allow_fail(f"touch '{release}'")
            for job_id in (victim_id, preemptor_id):
                if job_id is not None:
                    cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_user_requeue_keeps_competing_dispatch_out_until_agent_release(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        target_index = 0
        target = cluster.node_names[target_index]
        entered, release = _install_blocking_epilog(cluster, target_index)
        cluster.start(
            {
                **_AUTH_ROOT,
                "hooks": {"epilog": f"{cluster.remote_dir}/hooks/blocking-epilog.sh"},
            }
        )

        original_id = replacement_id = None
        try:
            original = cluster.write_file("requeue-original.sh", "#!/bin/bash\nsleep 600\n")
            original_id = parse_job_id(
                cluster.sbatch(
                    ["-J", "requeue-original", "-N", "1", "--exclusive", f"--nodelist={target}", original]
                )
            )
            assert original_id is not None
            wait_job_state(cluster, original_id, "R", timeout=60)

            cluster.scontrol("requeue", str(original_id))
            _wait_for_file(cluster, target_index, entered)
            replacement_id = _submit_replacement(cluster, target, "requeue-replacement")
            _assert_replacement_waits_then_runs(cluster, target_index, release, replacement_id)
        finally:
            cluster.nodes[target_index].exec_allow_fail(f"touch '{release}'")
            for job_id in (original_id, replacement_id):
                if job_id is not None:
                    cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_forced_time_limit_keeps_replacement_out_until_agent_release(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        target_index = 0
        target = cluster.node_names[target_index]
        entered, release = _install_blocking_epilog(cluster, target_index)
        cluster.start(
            {
                **_AUTH_ROOT,
                "hooks": {"epilog": f"{cluster.remote_dir}/hooks/blocking-epilog.sh"},
            }
        )

        original_id = replacement_id = None
        try:
            original = cluster.write_file(
                "timeout-original.sh", "#!/bin/bash\ntrap '' TERM\nsleep 300\n"
            )
            original_id = parse_job_id(
                cluster.sbatch(
                    [
                        "-J",
                        "timeout-original",
                        "-N",
                        "1",
                        "--exclusive",
                        f"--nodelist={target}",
                        "--time=00:00:20",
                        original,
                    ]
                )
            )
            assert original_id is not None
            wait_job_state(cluster, original_id, "R", timeout=60)

            _wait_for_file(cluster, target_index, entered, timeout=120)
            replacement_id = _submit_replacement(cluster, target, "timeout-replacement")
            _assert_replacement_waits_then_runs(cluster, target_index, release, replacement_id)
        finally:
            cluster.nodes[target_index].exec_allow_fail(f"touch '{release}'")
            for job_id in (original_id, replacement_id):
                if job_id is not None:
                    cluster.cli_allow_fail(["scancel", str(job_id)])
