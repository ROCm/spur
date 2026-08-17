# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for read-side CLI filtering, sorting, and formatting.

squeue ordering, sinfo state/feature reporting, and the filter-based form of
scancel, which selects jobs by user/partition/name rather than by job ID.
"""

import time

from cluster import job_state, parse_job_id, wait_job, wait_job_state
import pytest

pytestmark = pytest.mark.suite_api


def _held_job_ids(cluster, name: str, count: int) -> list[int]:
    """Submit *count* held jobs so they sit Pending for the whole test."""
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\nsleep 1\n")
    ids = []
    for _ in range(count):
        job_id = parse_job_id(cluster.sbatch(["-J", name, "-H", script]))
        assert job_id is not None
        ids.append(job_id)
    return ids


def _squeue_ids(cluster, args: list[str]) -> list[int]:
    out = cluster.squeue(args + ["-h", "-o", "%i"])
    return [int(line.strip()) for line in out.splitlines() if line.strip().isdigit()]


class TestSqueueSorting:
    def test_default_sort_is_ascending_by_job_id(self, cluster):
        """Equal partition, state and priority fall through to jobid ascending."""
        ids = _held_job_ids(cluster, "sort-default", 3)
        try:
            listed = _squeue_ids(cluster, ["-n", "sort-default"])
            assert listed == sorted(ids), (
                f"default squeue order must be ascending by job id: {listed} vs {sorted(ids)}"
            )
        finally:
            for job_id in ids:
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_sort_flag_reverses_job_id_order(self, cluster):
        ids = _held_job_ids(cluster, "sort-desc", 3)
        try:
            listed = _squeue_ids(cluster, ["-n", "sort-desc", "-S", "-i"])
            assert listed == sorted(ids, reverse=True), (
                f"-S -i must sort descending by job id: {listed}"
            )
        finally:
            for job_id in ids:
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_invalid_sort_spec_is_rejected(self, cluster):
        out = cluster.cli_allow_fail(["squeue", "-S", "zz"])
        assert "Invalid sort specification" in out, (
            f"an unknown sort spec must be rejected, got:\n{out}"
        )


class TestSinfoFiltering:
    def test_state_filter_selects_idle_nodes(self, cluster):
        shown = cluster.sinfo_node_names(["-t", "idle"])
        assert shown == set(cluster.node_names), (
            f"`sinfo -t idle` should list every idle node, got {sorted(shown)}"
        )

    def test_state_filter_excludes_non_matching_states(self, cluster):
        shown = cluster.sinfo_node_names(["-t", "down"])
        assert shown.isdisjoint(cluster.node_names), (
            f"no idle node may appear under `sinfo -t down`, got {sorted(shown)}"
        )

    def test_state_filter_all_disables_filtering(self, cluster):
        shown = cluster.sinfo_node_names(["-t", "all"])
        assert shown == set(cluster.node_names), (
            f"`sinfo -t all` should list every node, got {sorted(shown)}"
        )

    def test_invalid_state_is_rejected(self, cluster):
        out = cluster.cli_allow_fail(["sinfo", "-t", "notastate"])
        assert "Invalid node state" in out, (
            f"an unknown node state must be rejected, got:\n{out}"
        )

    def test_drained_node_moves_between_state_filters(self, cluster):
        node = cluster.node_names[0]
        cluster.cli(["scontrol", "update", f"NodeName={node}", "State=DRAIN",
                     "Reason=sinfo-filter-test"])
        try:
            deadline = time.time() + 30
            while time.time() < deadline:
                if node in cluster.sinfo_node_names(["-t", "drain"]):
                    break
                time.sleep(2)
            else:
                raise AssertionError(
                    f"drained node {node} never appeared under `sinfo -t drain`:\n"
                    f"{cluster.sinfo()}"
                )
            assert node not in cluster.sinfo_node_names(["-t", "idle"]), (
                f"drained node {node} must leave the idle filter:\n{cluster.sinfo()}"
            )
        finally:
            cluster.cli_allow_fail(
                ["scontrol", "update", f"NodeName={node}", "State=RESUME"]
            )


class TestSinfoFeatures:
    def test_node_features_are_displayed(self, unstarted_cluster):
        """Configured [[nodes]] features must reach the sinfo %f column."""
        cluster = unstarted_cluster
        cluster.start(
            config_overrides={
                "nodes": [
                    {
                        "names": name,
                        "cpus": 64,
                        "memory_mb": 262144,
                        "features": ["mi300x", "fastnet"],
                    }
                    for name in cluster.node_names
                ],
            }
        )

        out = cluster.cli(["sinfo", "-N", "-o", "%n|%f", "-h"])
        rows = [line for line in out.splitlines() if line.strip()]
        assert rows, f"sinfo -N produced no node rows:\n{out}"
        for row in rows:
            name, _, features = row.partition("|")
            assert name.strip() in cluster.node_names, f"unexpected node row: {row}"
            assert "mi300x" in features and "fastnet" in features, (
                f"node {name} must report its configured features, got: {row}"
            )

    def test_missing_features_render_as_null(self, cluster):
        out = cluster.cli(["sinfo", "-N", "-o", "%n|%f", "-h"])
        rows = [line for line in out.splitlines() if line.strip()]
        assert rows, f"sinfo -N produced no node rows:\n{out}"
        for row in rows:
            assert row.endswith("(null)"), (
                f"a node with no configured features must render (null), got: {row}"
            )


class TestFilterScancel:
    def test_scancel_by_name_skips_terminal_jobs(self, cluster):
        """A finished job matched by the filter must be skipped silently.

        Sending it to cancel_job would produce a spurious per-job error, so the
        client drops terminal jobs before dispatching.
        """
        name = "filter-cancel"
        finished_script = cluster.write_file(
            "filter-done.sh", "#!/bin/bash\necho done\n"
        )
        finished_id = parse_job_id(cluster.sbatch(["-J", name, finished_script]))
        assert finished_id is not None
        assert wait_job(cluster, finished_id, timeout=90) == "CD", (
            cluster.debug_job(finished_id)
        )

        running_script = cluster.write_file(
            "filter-running.sh", "#!/bin/bash\nsleep 120\n"
        )
        running_id = parse_job_id(cluster.sbatch(["-J", name, running_script]))
        assert running_id is not None
        wait_job_state(cluster, running_id, "R", timeout=60)

        out = cluster.cli_allow_fail(["scancel", "-n", name])
        assert "error" not in out.lower(), (
            f"scancel -n must not error on the already-finished job:\n{out}"
        )

        deadline = time.time() + 60
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), running_id) == "CA":
                break
            time.sleep(2)
        else:
            raise AssertionError(
                f"running job {running_id} was not cancelled by `scancel -n {name}`:\n"
                f"{cluster.squeue_all()}"
            )

        assert job_state(cluster.squeue_all(), finished_id) == "CD", (
            f"the finished job must stay COMPLETED, not flip to CANCELLED:\n"
            f"{cluster.squeue_all()}"
        )

    def test_scancel_by_partition_cancels_active_jobs(self, cluster):
        script = cluster.write_file("part-cancel.sh", "#!/bin/bash\nsleep 120\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "part-cancel", script]))
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=60)

        cluster.cli_allow_fail(["scancel", "-p", "default"])

        deadline = time.time() + 60
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) == "CA":
                return
            time.sleep(2)
        raise AssertionError(
            f"job {job_id} was not cancelled by `scancel -p default`:\n"
            f"{cluster.squeue_all()}"
        )
