# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for quarantining a node whose job cgroup cannot be emptied.

``cgroup.kill`` defeats ``setsid`` and ``trap '' TERM``, so a plain escaping
descendant is reaped in time. A root helper that keeps moving fresh processes
into the job's cgroup stands in for a workload the kernel cannot clear.
"""

import re

import pytest

from cluster import parse_job_id, wait_job, wait_job_state, wait_until

CLEANUP_TIMEOUT = 5
# Unique duration so the cleanup pkill matches only this test's processes.
MARKER = 7331
CLEANUP_HOLD = "unfinished job cgroup cleanup"
DRAIN_REASON = "job cgroup cleanup incomplete"


def _node_reason(cluster, name: str) -> str:
    match = re.search(r"Reason=(.*)", cluster.scontrol_show_node(name))
    return match.group(1) if match else ""


class TestCgroupCleanupQuarantine:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {
            "cgroup": {"cleanup_timeout_secs": CLEANUP_TIMEOUT},
            "controller": {
                "dispatch_reject_cooldown_secs": 2,
                "max_launch_backoff_secs": 5,
                "max_batch_requeue": 50,
            },
        }

    def test_stalled_cleanup_drains_and_holds_the_node_until_it_clears(self, cgroup_cluster):
        cluster = cgroup_cluster
        node = cluster.nodes[0]
        name = cluster.node_names[0]
        sudo = cluster._sudo_prefix()

        cg_file = f"{cluster.remote_dir}/stuck-cgroup.path"
        script = cluster.write_file(
            "stuck-cleanup.sh",
            "#!/bin/bash\n"
            "while IFS= read -r line; do\n"
            f'  case "$line" in 0::*) echo "${{line#0::}}" > {cg_file} ;; esac\n'
            "done < /proc/self/cgroup\n"
            "sleep 600\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "stuck-cleanup", "-w", name, script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")
        cgroup = wait_until(
            lambda: node.exec_allow_fail(f"cat {cg_file} 2>/dev/null").strip(),
            timeout_s=30, desc="job cgroup path",
        )
        assert re.fullmatch(rf"/spur/job_{job_id}_\d+/step_\w+", cgroup), cgroup

        stop = f"{cluster.remote_dir}/pinner.stop"
        pinner = cluster.write_file(
            "pinner.sh",
            "#!/bin/bash\n"
            f"procs=/sys/fs/cgroup{cgroup}/cgroup.procs\n"
            f"while [ ! -e {stop} ]; do\n"
            f"  sleep {MARKER} & pid=$!\n"
            '  echo "$pid" > "$procs" 2>/dev/null || kill "$pid"\n'
            "done\n",
        )
        try:
            node.exec(f"{sudo}setsid nohup bash {pinner} > /dev/null 2>&1 &")
            cluster.cli(["scancel", str(job_id)])

            wait_until(
                lambda: DRAIN_REASON in _node_reason(cluster, name),
                timeout_s=CLEANUP_TIMEOUT + 45, desc="cleanup quarantine drain",
            )
            assert wait_job(cluster, job_id, timeout=60) in ("CA", "GONE")

            cluster.scontrol("update", f"NodeName={name}", "State=RESUME")
            probe = cluster.write_file("after-cleanup.sh", "#!/bin/bash\necho CLEANUP_CLEARED\n")
            out_path = f"{cluster.remote_dir}/after-cleanup.out"
            next_id = parse_job_id(
                cluster.sbatch(["-J", "after-cleanup", "-w", name, "-o", out_path, probe])
            )
            assert next_id is not None
            wait_until(
                lambda: CLEANUP_HOLD in cluster.scontrol("show", "job", str(next_id)),
                timeout_s=60, desc="launch refused while cleanup is pending",
            )
            assert DRAIN_REASON not in _node_reason(cluster, name), (
                "an acknowledged quarantine must not re-drain the resumed node"
            )

            node.exec(f"touch {stop}")
            assert wait_job(cluster, next_id, timeout=120) == "CD", cluster.debug_job(next_id)
            assert "CLEANUP_CLEARED" in cluster.read_output_on_any_node(out_path)
        finally:
            node.exec_allow_fail(f"touch {stop}")
            node.exec_allow_fail(f"{sudo}pkill -f '[s]leep {MARKER}' || true")
