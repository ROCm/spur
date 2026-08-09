# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node selection and placement.

Covers partition OR-lists, the additive form of --nodelist, --nodefile, and
how equal-weight jobs distribute across idle nodes.
"""

import time

from cluster import job_node_names, parse_job_id, wait_job, wait_job_state


def _job_nodes(cluster, job_id: int) -> list[str]:
    return job_node_names(cluster, job_id)


def _split_partitions(cluster) -> dict:
    """One partition per node, so partition choice is observable in placement."""
    return {
        "partitions": [
            {
                "name": "default",
                "state": "UP",
                "default": True,
                "nodes": ",".join(cluster.node_names),
                "max_time": "24:00:00",
                "default_time": "10:00",
            },
            {
                "name": "pa",
                "state": "UP",
                "nodes": cluster.node_names[0],
                "max_time": "24:00:00",
                "default_time": "10:00",
            },
            {
                "name": "pb",
                "state": "UP",
                "nodes": cluster.node_names[1],
                "max_time": "24:00:00",
                "default_time": "10:00",
            },
        ],
    }


class TestPartitionOrLists:
    def test_or_list_spans_both_partitions(self, unstarted_cluster):
        """`-p pa,pb` must draw nodes from the union, not just the first name."""
        cluster = unstarted_cluster
        cluster.require_nodes(2)
        cluster.start(config_overrides=_split_partitions(cluster))

        script = cluster.write_file("or-list.sh", "#!/bin/bash\necho OR_LIST_OK\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "or-list", "-p", "pa,pb", "-N", "2", script])
        )
        assert job_id is not None

        assert wait_job(cluster, job_id, timeout=120) == "CD", (
            f"a 2-node job across pa,pb must schedule; neither partition alone "
            f"holds 2 nodes\n{cluster.debug_job(job_id)}"
        )

    def test_or_list_falls_back_to_the_available_partition(self, unstarted_cluster):
        """With pa's node busy, the job must still land via pb."""
        cluster = unstarted_cluster
        cluster.require_nodes(2)
        cluster.start(config_overrides=_split_partitions(cluster))

        blocker_script = cluster.write_file(
            "or-blocker.sh", "#!/bin/bash\nsleep 300\n"
        )
        blocker_id = parse_job_id(
            cluster.sbatch(
                ["-J", "or-blocker", "-p", "pa", "-N", "1", "--exclusive", blocker_script]
            )
        )
        assert blocker_id is not None
        wait_job_state(cluster, blocker_id, "R", timeout=60)

        try:
            script = cluster.write_file(
                "or-fallback.sh", "#!/bin/bash\necho FALLBACK_OK\n"
            )
            job_id = parse_job_id(
                cluster.sbatch(
                    ["-J", "or-fallback", "-p", "pa,pb", "-N", "1", "--exclusive", script]
                )
            )
            assert job_id is not None
            wait_job_state(cluster, job_id, "R", timeout=90)

            assert _job_nodes(cluster, job_id) == [cluster.node_names[1]], (
                f"the job must fall back to pb's node\n{cluster.debug_job(job_id)}"
            )
            wait_job(cluster, job_id, timeout=90)
        finally:
            cluster.cli_allow_fail(["scancel", str(blocker_id)])

    def test_or_list_rejects_an_unknown_partition(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.require_nodes(2)
        cluster.start(config_overrides=_split_partitions(cluster))

        script = cluster.write_file("or-bad.sh", "#!/bin/bash\necho x\n")
        out = cluster.cli_allow_fail(
            ["sbatch", "-p", "pa,nosuchpart", "-N", "1", script]
        )
        assert "not found" in out.lower(), (
            f"an unknown name anywhere in the list must be rejected, got:\n{out}"
        )


class TestNodelistSelection:
    def test_additive_nodelist_adds_nodes_to_the_listed_one(self, multi_node_cluster):
        """`-w nodeA -N 2` pins nodeA and lets the scheduler pick the rest."""
        cluster = multi_node_cluster
        pinned = cluster.node_names[0]

        script = cluster.write_file("additive.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "additive", "-w", pinned, "-N", "2", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=90)

        try:
            nodes = _job_nodes(cluster, job_id)
            assert pinned in nodes, (
                f"the listed node {pinned} must be in the allocation, got {nodes}"
            )
            assert len(nodes) == 2, (
                f"-N 2 with a one-node list must allocate 2 nodes, got {nodes}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_exact_nodelist_is_not_additive(self, multi_node_cluster):
        """A list that already covers -N stays an exact pin."""
        cluster = multi_node_cluster
        target = cluster.node_names[1]

        script = cluster.write_file("exact.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "exact", "-w", target, "-N", "1", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=90)

        try:
            assert _job_nodes(cluster, job_id) == [target], (
                f"an exact nodelist must pin exactly {target}\n"
                f"{cluster.debug_job(job_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_nodefile_selects_the_listed_nodes(self, multi_node_cluster):
        """-F reads the same selection from a file instead of the command line."""
        cluster = multi_node_cluster
        target = cluster.node_names[1]
        nodefile = cluster.write_file(
            "nodes.txt", f"{target}\n", executable=False
        )

        script = cluster.write_file("nodefile.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "nodefile", "-F", nodefile, "-N", "1", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=90)

        try:
            assert _job_nodes(cluster, job_id) == [target], (
                f"--nodefile must select {target}\n{cluster.debug_job(job_id)}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestJobSpreading:
    def test_equal_weight_jobs_spread_across_idle_nodes(self, multi_node_cluster):
        """Two identical one-node jobs must not both pack onto the first node."""
        cluster = multi_node_cluster
        script = cluster.write_file("spread.sh", "#!/bin/bash\nsleep 60\n")

        ids = []
        for i in range(2):
            job_id = parse_job_id(
                cluster.sbatch(["-J", f"spread-{i}", "-N", "1", script])
            )
            assert job_id is not None
            ids.append(job_id)

        try:
            for job_id in ids:
                wait_job_state(cluster, job_id, "R", timeout=90)
            placements = [_job_nodes(cluster, job_id)[0] for job_id in ids]
            assert len(set(placements)) == 2, (
                f"equal-weight jobs must spread across idle nodes, both landed "
                f"on {placements}"
            )
        finally:
            for job_id in ids:
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_spread_job_flag_is_accepted(self, multi_node_cluster):
        cluster = multi_node_cluster
        script = cluster.write_file(
            "spread-flag.sh", "#!/bin/bash\necho SPREAD_OK\n"
        )
        out_path = f"{cluster.remote_dir}/spread-flag.out"
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "spread-flag", "-N", "2", "--spread-job", "-o", out_path, script]
            )
        )
        assert job_id is not None

        assert wait_job(cluster, job_id, timeout=120) == "CD", (
            cluster.debug_job(job_id)
        )
        content = cluster.read_output_all_nodes(out_path)
        assert content.count("SPREAD_OK") == 2, (
            f"--spread-job must still run the script on both nodes:\n{content}"
        )


class TestExcludeSelection:
    def test_exclude_keeps_the_job_off_a_node(self, multi_node_cluster):
        cluster = multi_node_cluster
        excluded = cluster.node_names[0]

        script = cluster.write_file("exclude.sh", "#!/bin/bash\nsleep 30\n")
        job_id = parse_job_id(
            cluster.sbatch(["-J", "exclude", "-x", excluded, "-N", "1", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=90)

        try:
            nodes = _job_nodes(cluster, job_id)
            assert excluded not in nodes, (
                f"-x {excluded} must keep the job off that node, got {nodes}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestPendingReasons:
    def test_unavailable_pinned_node_reports_req_node_not_avail(
        self, multi_node_cluster
    ):
        """An exact pin to a busy node reports why, rather than a bare Resources."""
        cluster = multi_node_cluster
        target = cluster.node_names[0]

        blocker = cluster.write_file("pin-blocker.sh", "#!/bin/bash\nsleep 300\n")
        blocker_id = parse_job_id(
            cluster.sbatch(
                ["-J", "pin-blocker", "-w", target, "-N", "1", "--exclusive", blocker]
            )
        )
        assert blocker_id is not None
        wait_job_state(cluster, blocker_id, "R", timeout=60)

        script = cluster.write_file("pin-wait.sh", "#!/bin/bash\nsleep 5\n")
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "pin-wait", "-w", target, "-N", "1", "--exclusive", script]
            )
        )
        assert job_id is not None

        try:
            wait_job_state(cluster, job_id, "PD", timeout=30)
            # Give the controller a cycle to tag the reason.
            time.sleep(5)
            reason = cluster.squeue(["-j", str(job_id), "-h", "-o", "%r"]).strip()
            assert "ReqNodeNotAvail" in reason or "Resources" in reason, (
                f"expected a node-availability reason, got {reason!r}"
            )
        finally:
            for jid in (job_id, blocker_id):
                cluster.cli_allow_fail(["scancel", str(jid)])
