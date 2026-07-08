# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Client-side controller failover E2E tests.

Exercises the comma-separated ``SPUR_CONTROLLER_ADDR`` endpoint list: the CLI
must rotate past an unreachable controller endpoint and reach a live one. These
run against the standard single-controller cluster; the dead endpoint is a
closed port on the same host, so no extra topology is needed.

Full multi-controller Raft failover (kill the connected controller, survivor
takes over) is covered separately once the harness grows Raft support.
"""

from cluster import parse_job_id, wait_job

# Port with nothing listening — connections are refused immediately.
DEAD_PORT = 1


def _dead(cluster) -> str:
    return f"http://{cluster.nodes[0].host}:{DEAD_PORT}"


class TestControllerFailover:
    def test_rotates_past_dead_first_endpoint(self, cluster):
        endpoints = f"{_dead(cluster)},{cluster.controller_addr}"
        out = cluster.cli(["sinfo"], controller_addr=endpoints)
        for name in cluster.node_names:
            assert name in out, (
                f"node {name} missing after rotating past dead endpoint:\n{out}"
            )

    def test_live_first_endpoint_still_works(self, cluster):
        endpoints = f"{cluster.controller_addr},{_dead(cluster)}"
        out = cluster.cli(["sinfo"], controller_addr=endpoints)
        for name in cluster.node_names:
            assert name in out, f"node {name} missing with live-first list:\n{out}"

    def test_all_endpoints_down_fails_cleanly(self, cluster):
        endpoints = f"{_dead(cluster)},http://{cluster.nodes[0].host}:2"
        out = cluster.cli_allow_fail(["sinfo"], controller_addr=endpoints)
        assert not any(name in out for name in cluster.node_names), (
            f"expected failure with all endpoints down, got node output:\n{out}"
        )
        assert "connect" in out.lower() or "transport" in out.lower(), (
            f"expected a connection error, got:\n{out}"
        )

    def test_job_submits_through_failover_list(self, cluster):
        out_path = f"{cluster.remote_dir}/failover.out"
        script = cluster.write_file(
            "failover-job.sh", "#!/bin/bash\necho FAILOVER_JOB_OK\n"
        )
        endpoints = f"{_dead(cluster)},{cluster.controller_addr}"
        sb = cluster.cli(
            ["sbatch", "-J", "failover", "-N", "1", "-o", out_path, script],
            controller_addr=endpoints,
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"submit through failover list failed:\n{sb}"

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "FAILOVER_JOB_OK" in content, f"output:\n{content}"
