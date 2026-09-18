# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E: ``[auth] plugin = "spur"`` mint, submit, launch, cancel."""

from cluster import parse_job_id, wait_job


def _assert_launch_was_authenticated(cluster):
    anonymous = [
        line
        for index in range(len(cluster.nodes))
        for line in cluster.spurd_log(index).splitlines()
        if "unauthenticated agent request accepted" in line and "LaunchJob" in line
    ]
    assert not anonymous, (
        "the agent accepted LaunchJob anonymously; native execution credentials "
        "were not presented:\n" + "\n".join(anonymous)
    )


class TestNativePluginJobLifecycle:
    def test_sbatch_completes_under_native_plugin(self, unstarted_cluster):
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
            config_overrides={"auth": {"plugin": "spur", "mode": "required"}},
        )

        script = cluster.write_file(
            "native-plugin.sh", "#!/bin/bash\necho native-ok\n"
        )
        out_path = f"{cluster.remote_dir}/native-plugin.out"
        sb = cluster.sbatch(["-J", "native-plugin", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch returned no job id:\n{sb}"

        try:
            wait_job(cluster, job_id, timeout=90)
        except TimeoutError:
            raise AssertionError(
                f"job {job_id} never reached a terminal state:\n"
                f"{cluster.squeue_all()}\n"
                f"{cluster.debug_job(job_id)}"
            )

        show = cluster.scontrol("show", "job", str(job_id))
        assert "JobState=COMPLETED" in show, f"expected COMPLETED:\n{show}"
        assert "native-ok" in cluster.read_output_on_any_node(out_path)
        _assert_launch_was_authenticated(cluster)

        hold = cluster.write_file("native-cancel.sh", "#!/bin/bash\nsleep 120\n")
        sb = cluster.sbatch(["-J", "native-cancel", "-N", "1", "-t", "5", hold])
        cancel_id = parse_job_id(sb)
        assert cancel_id is not None, f"sbatch returned no job id:\n{sb}"
        cluster.scancel(str(cancel_id))
        wait_job(cluster, cancel_id, timeout=90)
        show = cluster.scontrol("show", "job", str(cancel_id))
        assert "JobState=CANCELLED" in show, f"expected CANCELLED:\n{show}"
