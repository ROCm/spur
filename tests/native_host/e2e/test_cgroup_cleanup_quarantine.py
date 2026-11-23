# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E test for quarantining a node whose job cgroup cannot be removed.

``cgroup.kill`` reaps every process, so no workload can hold the cgroup without
racing the reaper. Bind-mounting the step cgroup onto itself instead makes its
``rmdir`` fail with EBUSY on every attempt until it is unmounted.
"""

import re
import shlex

import pytest

from cluster import parse_cleanup_records, parse_job_id, wait_job, wait_job_state, wait_until

CLEANUP_TIMEOUT = 5
DRAIN_REASON = "job cgroup cleanup incomplete"
LAUNCH_REFUSED = "refusing new work until pending job cleanup completes"


def _node_reason(cluster, name: str) -> str:
    match = re.search(r"Reason=(.*)", cluster.scontrol_show_node(name))
    return match.group(1) if match else ""


def _pending_cleanup(cluster, node, job_id: int) -> list[dict]:
    """The agent's persisted cleanup obligations for *job_id*, one per step."""
    directory = f"{cluster.state_dir}/runtime/cleanup"
    # An unmatched glob stays literal, and a record can vanish mid-loop once reaped.
    script = (
        f'for f in {shlex.quote(directory)}/{job_id}.*.json; do '
        '[ -e "$f" ] || continue; cat "$f"; echo; done'
    )
    return parse_cleanup_records(node.exec(f"{cluster._sudo_prefix()}sh -c {shlex.quote(script)}"))


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

        pinned = shlex.quote(f"/sys/fs/cgroup{cgroup}")
        mounted = True
        try:
            node.exec(f"{sudo}mount --bind {pinned} {pinned}")
            cluster.cli(["scancel", str(job_id)])

            wait_until(
                lambda: DRAIN_REASON in _node_reason(cluster, name),
                timeout_s=CLEANUP_TIMEOUT + 45, desc="cleanup quarantine drain",
            )
            # The agent persists the acknowledgment only after the controller accepted the drain.
            wait_until(
                lambda: any(r["quarantine_acknowledged"] for r in _pending_cleanup(cluster, node, job_id)),
                timeout_s=30, desc="acknowledged pending-cleanup record",
            )
            assert wait_job(cluster, job_id, timeout=60) in ("CA", "GONE")

            cluster.scontrol("update", f"NodeName={name}", "State=RESUME")
            log_offset = len(cluster.spurd_log(0))
            probe = cluster.write_file("after-cleanup.sh", "#!/bin/bash\necho CLEANUP_CLEARED\n")
            out_path = f"{cluster.remote_dir}/after-cleanup.out"
            next_id = parse_job_id(
                cluster.sbatch(["-J", "after-cleanup", "-w", name, "-o", out_path, probe])
            )
            assert next_id is not None
            wait_until(
                lambda: LAUNCH_REFUSED in cluster.spurd_log(0)[log_offset:],
                timeout_s=60, desc="launch refused while cleanup is pending",
            )
            assert _pending_cleanup(cluster, node, job_id), "cleanup record cleared while still pinned"
            assert DRAIN_REASON not in _node_reason(cluster, name), (
                "an acknowledged quarantine must not re-drain the resumed node"
            )

            node.exec(f"{sudo}umount {pinned}")
            mounted = False
            assert wait_job(cluster, next_id, timeout=120) == "CD", cluster.debug_job(next_id)
            assert "CLEANUP_CLEARED" in cluster.read_output_on_any_node(out_path)
            assert not _pending_cleanup(cluster, node, job_id)
        finally:
            if mounted:
                node.exec_allow_fail(f"{sudo}umount {pinned}")
