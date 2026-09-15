# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests that the controller's credential to agents lets dispatch through.

The controller presents a signing key to agents on every LaunchJob. If it
presents one the agents cannot verify, dispatch fails for the whole cluster
while every other surface still looks healthy, so each supported way of
configuring that key is exercised end to end here.
"""

from cluster import parse_job_id, wait_job

KEY = "e2e-dispatch-key"


def _assert_a_job_dispatches_and_completes(cluster, tag: str):
    script = cluster.write_file(
        f"auth-dispatch-{tag}.sh", "#!/bin/bash\necho dispatched\n"
    )
    out_path = f"{cluster.remote_dir}/auth-dispatch-{tag}.out"
    sb = cluster.sbatch(["-J", f"auth-{tag}", "-N", "1", "-o", out_path, script])
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch returned no job id:\n{sb}"

    try:
        wait_job(cluster, job_id, timeout=90)
    except TimeoutError:
        raise AssertionError(
            f"job {job_id} never reached a terminal state — the agent likely "
            f"refused the controller's credential:\n{cluster.squeue_all()}"
        )

    show = cluster.scontrol("show", "job", str(job_id))
    assert "JobState=COMPLETED" in show, f"expected COMPLETED:\n{show}"
    assert "dispatched" in cluster.read_output_on_any_node(out_path)


def _assert_the_launch_was_authenticated(cluster):
    """A completed job proves nothing on its own: a permissive agent accepts an
    uncredentialed launch, so a controller signing with the wrong key still
    dispatches. Assert the agent actually verified a credential."""
    anonymous = [
        line
        for index in range(len(cluster.nodes))
        for line in cluster.spurd_log(index).splitlines()
        if "unauthenticated agent request accepted" in line and "LaunchJob" in line
    ]
    assert not anonymous, (
        "the agent accepted the launch anonymously despite a configured key, so "
        "the controller presented no credential:\n" + "\n".join(anonymous)
    )


class TestDispatchCredential:
    def test_a_keyless_cluster_dispatches(self, cluster):
        # The default deployment. A controller with no key must present no
        # credential: signing with a fallback makes every keyless agent refuse.
        _assert_a_job_dispatches_and_completes(cluster, "keyless")

    def test_an_inline_key_cluster_dispatches(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(
            config_overrides={"auth": {"plugin": "jwt", "jwt_key": KEY}},
        )
        _assert_a_job_dispatches_and_completes(cluster, "inline")
        _assert_the_launch_was_authenticated(cluster)

    def test_a_key_file_cluster_dispatches(self, unstarted_cluster):
        cluster = unstarted_cluster
        key_path = cluster.write_file(
            "jwt.key", KEY, all_nodes=True, executable=False
        )
        cluster.start(
            config_overrides={"auth": {"plugin": "jwt", "jwt_key_file": key_path}},
        )
        _assert_a_job_dispatches_and_completes(cluster, "keyfile")
        _assert_the_launch_was_authenticated(cluster)
