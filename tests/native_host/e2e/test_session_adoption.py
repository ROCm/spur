# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test: `spur adopt` pam_exec helper (issue #782).

Adoption places a session belonging to a user who holds an allocation into the
job cgroup and emits the job environment; admission (opt-in) refuses a user who
holds none.
"""

import threading
import time

import pytest

from conftest import _deploy_cluster

pytestmark = pytest.mark.rootful


@pytest.fixture
def rootful_cluster(ssh_nodes, remote_bin_dir, cluster_config_overrides):
    fstype = ssh_nodes[0].exec_allow_fail("stat -fc %T /sys/fs/cgroup").strip()
    if "cgroup2fs" not in fstype:
        pytest.skip("node 0 is not cgroup v2")
    c = _deploy_cluster(ssh_nodes, remote_bin_dir, agent_as_root=True,
                        config_overrides=cluster_config_overrides)
    try:
        yield c
    finally:
        c.teardown()


class TestSessionAdoption:
    def test_adopt_admission_and_environment(self, rootful_cluster):
        c = rootful_cluster
        node = c.nodes[0]
        owner_uid = node.exec_allow_fail("id -u").strip()
        spur = f"{c.bin_dir}/spur"
        job_name = "adopt-test"

        def hold():
            c.salloc_run("sleep 30\n",
                         salloc_args=["-N", "1", "-J", job_name, "-t", "0:05"])

        t = threading.Thread(target=hold, daemon=True)
        t.start()
        try:
            jid = None
            deadline = time.time() + 30
            while time.time() < deadline:
                ids = c.running_job_ids_by_name(job_name)
                if ids:
                    jid = ids[0]
                    break
                time.sleep(1)
            assert jid is not None, "allocation never reached running"

            # Admission: the owner holds an allocation -> permitted (exit 0).
            rc = node.exec_allow_fail(
                f"PAM_TYPE=account PAM_USER={owner_uid} SPUR_REQUIRE_ALLOCATION=1 "
                f"{spur} adopt; echo RC=$?"
            )
            assert "RC=0" in rc, rc

            # Admission: a uid with no allocation -> refused (exit 1).
            rc = node.exec_allow_fail(
                f"PAM_TYPE=account PAM_USER=99991 SPUR_REQUIRE_ALLOCATION=1 "
                f"{spur} adopt; echo RC=$?"
            )
            assert "RC=1" in rc, rc

            # Adoption: open_session emits the job env.
            env_out = node.exec_allow_fail(
                f"PAM_TYPE=open_session PAM_USER={owner_uid} {spur} adopt 2>/dev/null"
            )
            assert f"SPUR_JOB_ID={jid}" in env_out, env_out

            # Adoption: the session process joins the job cgroup. pam_exec's
            # session hook runs as root, so simulate that with sudo; the sudo
            # shell is the helper's parent and must land in /spur/job_<id>.
            cg_out = node.exec_allow_fail(
                f"sudo bash -c 'PAM_TYPE=open_session PAM_USER={owner_uid} "
                f"{spur} adopt >/dev/null 2>&1; cat /proc/$$/cgroup'"
            )
            assert f"/spur/job_{jid}" in cg_out, cg_out
        finally:
            if jid is not None:
                c.scancel(str(jid))
            t.join(timeout=10)
