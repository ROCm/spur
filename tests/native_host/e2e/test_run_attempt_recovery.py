# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression coverage for a job recovering after an aborted dispatch.

Before the fix, a job's `run_attempt` only advanced on a fully confirmed
dispatch, so an aborted or held attempt re-presented the same epoch on every
retry. Under the native auth plugin that epoch is baked into the signed
execution credential, so every retry carried an already-cancelled (and thus
rejected) credential, and the job could wedge at JobHoldMaxRequeue forever
instead of recovering.

Combines the native-auth harness setup from test_native_plugin.py with the
partial-admission-abort technique from test_dispatch_abort_release.py.
"""

import re
import time

import pytest

from cluster import block_agent_port, parse_job_id, wait_job_state

ADMISSION_ABORT_TIMEOUT = 30
RECOVERY_RUNNING_BOUND = 60


class TestRunAttemptRecovery:
    def test_aborted_dispatch_advances_run_attempt_and_recovers(self, unstarted_cluster):
        cluster = unstarted_cluster
        if len(cluster.nodes) < 2:
            pytest.skip(
                f"multi-node test requires >= 2 nodes in SPUR_TEST_NODES "
                f"(got {len(cluster.nodes)})"
            )

        paths = cluster.install_native_jwks()
        sock = f"{cluster.remote_dir}/auth.sock"
        cluster.cli_env = {
            "SPUR_AUTH_PLUGIN": "spur",
            "SPUR_AUTH_SOCKET": sock,
            "SPUR_CLUSTER_NAME": "e2e-test",
        }
        cluster.daemon_env = {
            "SPUR_AUTH_JWKS": paths["auth"],
            "SPUR_CRED_VERIFICATION_JWKS": paths["cred-verification"],
            "SPUR_CONTROLLER_VERIFICATION_JWKS": paths["controller-verification"],
            "SPUR_AUTH_SOCKET": sock,
            "SPUR_CLUSTER_NAME": "e2e-test",
        }
        cluster.controller_env = {
            "SPUR_CRED_SIGNING_JWKS": paths["cred-signing"],
            "SPUR_CONTROLLER_SIGNING_JWKS": paths["controller-signing"],
            "SPUR_NODE_SIGNING_JWKS": paths["node-signing"],
        }
        cluster.start_native_mint(paths["auth"], sock)
        cluster.start(
            config_overrides={"auth": {"plugin": "spur", "mode": "required"}},
        )

        # Stay alive past a couple of wait_job_state poll intervals so the
        # Running assertion below can't be missed by completing too fast.
        script = cluster.write_file(
            "run-attempt-recovery.sh", "#!/bin/bash\necho run-attempt-ok\nsleep 15\n"
        )
        victim_nodes = ",".join(cluster.node_names[:2])

        with block_agent_port(cluster, node_index=1):
            sb = cluster.sbatch(
                ["-J", "run-attempt-victim", "-N", "2", "-w", victim_nodes, script]
            )
            job_id = parse_job_id(sb)
            assert job_id is not None, f"sbatch failed: {sb}"

            self._wait_for_admission_abort(cluster, job_id, timeout=ADMISSION_ABORT_TIMEOUT)

        try:
            # The port block is gone: the next scheduler tick must retry with a
            # fresh run_attempt and actually succeed, not re-present the same
            # (poisoned) epoch and wedge the job at JobHoldMaxRequeue.
            wait_job_state(cluster, job_id, "R", timeout=RECOVERY_RUNNING_BOUND)
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobHoldMaxRequeue" not in show, (
                f"job {job_id} was held at max requeue instead of recovering:\n{show}"
            )

            attempts = self._dispatch_run_attempts(cluster, job_id)
            assert len(attempts) >= 2, (
                f"expected at least two logged dispatch attempts for job {job_id}, "
                f"got {attempts}\n{cluster.spurd_log(0)}"
            )
            assert len(set(attempts)) >= 2, (
                "two consecutive dispatch attempts for the same job_id must log "
                f"different run_attempt= values, got {attempts}"
            )
        finally:
            cluster.scancel(job_id)

    @staticmethod
    def _wait_for_admission_abort(cluster, job_id: int, timeout: int) -> None:
        """Poll the controller log for this job's abort, not a fixed sleep."""
        needle = f"aborting admission instead of partially running job_id={job_id} "
        deadline = time.time() + timeout
        while time.time() < deadline:
            log = re.sub(
                r"\x1b\[[0-9;]*m", "",
                cluster.nodes[0].read_file(f"{cluster.log_dir}/spurctld.log"),
            )
            if needle in log:
                return
            time.sleep(0.5)
        raise TimeoutError(
            f"job {job_id}'s admission was never aborted within {timeout}s "
            "— the blocked node's dispatch never failed as expected"
        )

    @staticmethod
    def _dispatch_run_attempts(cluster, job_id: int) -> list[int]:
        """Every run_attempt= value logged for this job's dispatch attempts, in order."""
        log = re.sub(
            r"\x1b\[[0-9;]*m", "",
            cluster.nodes[0].read_file(f"{cluster.log_dir}/spurctld.log"),
        )
        needle = f"job_id={job_id} "
        attempts = []
        for line in log.splitlines():
            if "dispatching job to agent" not in line or needle not in line:
                continue
            m = re.search(r"run_attempt=(\d+)", line)
            if m:
                attempts.append(int(m.group(1)))
        return attempts
