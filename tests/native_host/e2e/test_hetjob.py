# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for heterogeneous job components.

Only `sbatch --het-group=N` exists today: there is no `:` multi-component
submit, no het environment variables, and no het fields in `squeue`/`scontrol`.
Component linking is also broken through the CLI, because group 0 is sent as
the proto default and lands as "unset" on the controller, so the anchor a
later component looks for never exists.

These tests therefore pin the behaviour that is real — the flag is accepted
from both the command line and a `#SBATCH` directive, and a component runs like
any other job — plus the one property that must hold regardless of how linking
is eventually fixed: a component must never be silently dropped.
"""

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def submit_component(cluster, name: str, group: int, body: str, out_path: str) -> int:
    script = cluster.write_file(f"{name}.sh", f"#!/bin/bash\n{body}\n")
    job_id = parse_job_id(
        cluster.sbatch(
            ["-J", name, f"--het-group={group}", "-o", out_path, script]
        )
    )
    assert job_id is not None
    return job_id


class TestHetGroupFlag:
    def test_the_anchor_component_runs(self, cluster):
        out_path = f"{cluster.remote_dir}/het0.out"
        job_id = submit_component(cluster, "het0", 0, "echo HET0_OK", out_path)
        assert wait_job(cluster, job_id, timeout=120) in ("CD", "GONE"), (
            cluster.debug_job(job_id)
        )
        assert "HET0_OK" in cluster.read_output_on_any_node(out_path)

    def test_a_later_component_runs(self, cluster):
        out_path = f"{cluster.remote_dir}/het1.out"
        job_id = submit_component(cluster, "het1", 1, "echo HET1_OK", out_path)
        assert wait_job(cluster, job_id, timeout=120) in ("CD", "GONE"), (
            cluster.debug_job(job_id)
        )
        assert "HET1_OK" in cluster.read_output_on_any_node(out_path)

    def test_the_sbatch_directive_form_is_accepted(self, cluster):
        out_path = f"{cluster.remote_dir}/het-directive.out"
        script = cluster.write_file(
            "het-directive.sh",
            "#!/bin/bash\n#SBATCH --het-group=1\necho HET_DIRECTIVE_OK\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "het-directive", "-o", out_path, script])
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=120) in ("CD", "GONE"), (
            cluster.debug_job(job_id)
        )
        assert "HET_DIRECTIVE_OK" in cluster.read_output_on_any_node(out_path)

    def test_a_non_numeric_group_is_rejected(self, cluster):
        script = cluster.write_file("het-bad.sh", "#!/bin/bash\ntrue\n")
        code, out = cluster.sbatch_with_exit(
            ["-J", "het-bad", "--het-group=abc", script]
        )
        assert code != 0, f"a non-numeric --het-group must be rejected:\n{out}"


class TestComponentSubmission:
    def test_each_component_gets_its_own_job_id(self, cluster):
        """Components are independent jobs today. Reusing an id would break
        cancel and accounting, so the ids must stay distinct."""
        first = submit_component(
            cluster, "het-a", 0, "sleep 30", f"{cluster.remote_dir}/het-a.out"
        )
        second = submit_component(
            cluster, "het-b", 1, "sleep 30", f"{cluster.remote_dir}/het-b.out"
        )
        try:
            assert first != second
        finally:
            for job_id in (first, second):
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_every_component_reaches_the_queue(self, cluster):
        """The gap that matters most while linking is unimplemented: a
        component must not be swallowed on the way to the scheduler."""
        ids = [
            submit_component(
                cluster, f"het-q{g}", g, "sleep 30", f"{cluster.remote_dir}/het-q{g}.out"
            )
            for g in range(3)
        ]
        try:
            listed = cluster.squeue(["-h", "-o", "%i"])
            queued = {int(ln.strip()) for ln in listed.splitlines() if ln.strip().isdigit()}
            assert set(ids) <= queued, (
                f"components {sorted(set(ids) - queued)} never reached the queue:\n"
                f"{cluster.squeue_all()}"
            )
        finally:
            for job_id in ids:
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_cancelling_one_component_leaves_the_others(self, cluster):
        """Without linking, scancel is per-component. Pinning that keeps a
        future group-cancel change from going unnoticed."""
        first = submit_component(
            cluster, "het-c0", 0, "sleep 120", f"{cluster.remote_dir}/het-c0.out"
        )
        second = submit_component(
            cluster, "het-c1", 1, "sleep 120", f"{cluster.remote_dir}/het-c1.out"
        )
        try:
            wait_job_state(cluster, first, "R", timeout=90)
            wait_job_state(cluster, second, "R", timeout=90)
            cluster.scancel(str(first))
            wait_job_state(cluster, second, "R", timeout=30)
        finally:
            for job_id in (first, second):
                cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_components_can_request_different_shapes(self, cluster):
        """The whole point of a het job is asymmetric components, so differing
        resource requests must at least submit and run independently."""
        if len(cluster.nodes) < 2:
            pytest.skip("differing component shapes need at least 2 nodes")

        script = cluster.write_file("het-shape.sh", "#!/bin/bash\nsleep 60\n")
        small = parse_job_id(
            cluster.sbatch(["-J", "het-small", "--het-group=0", "-N", "1", script])
        )
        large = parse_job_id(
            cluster.sbatch(["-J", "het-large", "--het-group=1", "-N", "2", script])
        )
        assert small is not None and large is not None
        try:
            wait_job_state(cluster, small, "R", timeout=90)
        finally:
            for job_id in (small, large):
                cluster.cli_allow_fail(["scancel", str(job_id)])
