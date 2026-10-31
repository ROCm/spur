# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for InactiveLimit reaping of interactive allocations.

A live client must keep its allocation across a controller restart. Under the
native auth plugin every restart changes the credential epoch, so a client that
kept its submit-time credential would fail every later keepalive and have its
attended allocation reaped one limit later.
"""

import threading
import time

INACTIVE_LIMIT_SECS = 60
RUNNING_BOUND = 30


class TestInactiveLimit:
    def test_a_live_salloc_survives_a_controller_restart_under_native_auth(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
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
            config_overrides={
                "auth": {"plugin": "spur", "mode": "required"},
                "scheduler": {"inactive_limit_secs": INACTIVE_LIMIT_SECS},
            },
        )

        job_name = f"inactive-live-{time.time_ns()}"
        result: dict[str, object] = {}

        def run():
            # The shell outlives the restart plus two full limits.
            result["code"], result["out"] = cluster.salloc_run(
                "sleep 200\n",
                salloc_args=["-N", "1", "-t", "10:00", "-J", job_name],
            )

        thread = threading.Thread(target=run)
        thread.start()
        try:
            deadline = time.time() + RUNNING_BOUND
            job_id = None
            while time.time() < deadline:
                ids = cluster.running_job_ids_by_name(job_name)
                if ids:
                    job_id = ids[0]
                    break
                time.sleep(1)
            assert job_id, "the salloc allocation never reached RUNNING"

            cluster.restart_controller()
            # A client still on its pre-restart credential would have been
            # signalled within one limit plus the reaper's tick.
            time.sleep(2 * INACTIVE_LIMIT_SECS + 10)
            show = cluster.scontrol("show", "job", str(job_id))
            assert "JobState=RUNNING" in show, (
                f"an attended salloc was reaped after a controller restart:\n{show}"
            )
        finally:
            thread.join(timeout=300)

        assert result.get("code") == 0, result.get("out")
