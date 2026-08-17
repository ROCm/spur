# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for cluster-wide software licenses.

Licenses are a cluster-wide counted resource declared in `[licenses]`. Unlike
node resources they are not attached to any node, and the pool total is never
decremented — usage is derived from the set of running jobs. That derivation is
what these tests exercise: a job holding the pool blocks others with
`Reason=Licenses`, and finishing releases the count without drift.
"""

import re
import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

pytestmark = pytest.mark.suite_policy

LICENSES = {"fluent": 2, "matlab": 1}


@pytest.fixture
def license_cluster(unstarted_cluster):
    """A cluster with a small, exhaustible license pool."""
    unstarted_cluster.start(config_overrides={"licenses": LICENSES})
    return unstarted_cluster


def submit_holder(cluster, name: str, licenses: str, seconds: int = 120) -> int:
    script = cluster.write_file(f"{name}.sh", f"#!/bin/bash\nsleep {seconds}\n")
    job_id = parse_job_id(cluster.sbatch(["-J", name, "-L", licenses, script]))
    assert job_id is not None
    return job_id


def pending_reason(cluster, job_id: int) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    match = re.search(r"Reason=(\S+)", out)
    return match.group(1) if match else ""


def wait_reason(cluster, job_id: int, reason: str, timeout: int = 60) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pending_reason(cluster, job_id) == reason:
            return
        time.sleep(2)
    raise AssertionError(
        f"job {job_id} never reported Reason={reason}\n"
        f"{cluster.scontrol('show', 'job', str(job_id))}"
    )


class TestLicenseAvailability:
    def test_a_job_within_the_pool_runs(self, license_cluster):
        job_id = submit_holder(license_cluster, "lic-fit", "fluent:2")
        try:
            wait_job_state(license_cluster, job_id, "R", timeout=90)
        finally:
            license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_a_job_exceeding_the_pool_total_never_starts(self, license_cluster):
        """The request can never be satisfied, so it must stay pending on
        Licenses rather than being rejected at submit or starved silently."""
        job_id = submit_holder(license_cluster, "lic-too-big", "fluent:5")
        try:
            wait_reason(license_cluster, job_id, "Licenses")
        finally:
            license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_an_unconfigured_license_blocks_the_job(self, license_cluster):
        """An unknown name has an implicit pool of zero. Treating it as
        unlimited would let jobs bypass licensing entirely by typo."""
        job_id = submit_holder(license_cluster, "lic-unknown", "nosuchlic:1")
        try:
            wait_reason(license_cluster, job_id, "Licenses")
        finally:
            license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_separate_pools_do_not_interfere(self, license_cluster):
        holder = submit_holder(license_cluster, "lic-fluent", "fluent:2")
        other = submit_holder(license_cluster, "lic-matlab", "matlab:1")
        try:
            wait_job_state(license_cluster, holder, "R", timeout=90)
            wait_job_state(license_cluster, other, "R", timeout=90)
        finally:
            for job_id in (holder, other):
                license_cluster.cli_allow_fail(["scancel", str(job_id)])


class TestLicenseContention:
    def test_a_second_job_waits_on_licenses(self, license_cluster):
        holder = submit_holder(license_cluster, "lic-hold", "fluent:2")
        wait_job_state(license_cluster, holder, "R", timeout=90)

        waiter = submit_holder(license_cluster, "lic-wait", "fluent:1")
        try:
            wait_reason(license_cluster, waiter, "Licenses")
        finally:
            for job_id in (holder, waiter):
                license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_squeue_shows_the_reason_in_the_nodelist_column(self, license_cluster):
        holder = submit_holder(license_cluster, "lic-hold2", "fluent:2")
        wait_job_state(license_cluster, holder, "R", timeout=90)
        waiter = submit_holder(license_cluster, "lic-wait2", "fluent:1")
        try:
            wait_reason(license_cluster, waiter, "Licenses")
            out = license_cluster.squeue(["-h", "-o", "%i %R"])
            row = next(
                (ln for ln in out.splitlines() if ln.split()[:1] == [str(waiter)]), ""
            )
            assert "(Licenses)" in row, f"expected (Licenses) in squeue row: {out}"
        finally:
            for job_id in (holder, waiter):
                license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_licenses_are_released_when_the_holder_ends(self, license_cluster):
        """Usage is derived from the running set, so a terminal job has to free
        its licenses with no explicit release call."""
        holder = submit_holder(license_cluster, "lic-release", "fluent:2")
        wait_job_state(license_cluster, holder, "R", timeout=90)

        waiter = submit_holder(license_cluster, "lic-next", "fluent:2", seconds=5)
        wait_reason(license_cluster, waiter, "Licenses")

        license_cluster.cli_allow_fail(["scancel", str(holder)])
        try:
            wait_job_state(license_cluster, waiter, "R", timeout=90)
        finally:
            license_cluster.cli_allow_fail(["scancel", str(waiter)])

    def test_the_pool_does_not_drift_across_generations(self, license_cluster):
        """A pool total decremented at allocation instead of derived would
        shrink a little on every job; three full cycles would expose it."""
        for i in range(3):
            script = license_cluster.write_file(
                f"lic-cycle{i}.sh", "#!/bin/bash\necho cycle\n"
            )
            job_id = parse_job_id(
                license_cluster.sbatch(["-J", f"lic-cycle{i}", "-L", "fluent:2", script])
            )
            assert job_id is not None
            assert wait_job(license_cluster, job_id, timeout=90) in ("CD", "GONE"), (
                license_cluster.debug_job(job_id)
            )


class TestLicenseRequestForms:
    def test_gres_license_syntax_is_equivalent(self, license_cluster):
        """`--gres=license:...` is the older spelling and folds into the same
        pool, so both forms must contend with each other."""
        script = license_cluster.write_file("lic-gres.sh", "#!/bin/bash\nsleep 120\n")
        holder = parse_job_id(
            license_cluster.sbatch(
                ["-J", "lic-gres", "--gres=license:fluent:2", script]
            )
        )
        assert holder is not None
        waiter = submit_holder(license_cluster, "lic-gres-wait", "fluent:1")
        try:
            wait_job_state(license_cluster, holder, "R", timeout=90)
            wait_reason(license_cluster, waiter, "Licenses")
        finally:
            for job_id in (holder, waiter):
                license_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_repeated_flags_accumulate(self, license_cluster):
        job_id = submit_holder(license_cluster, "lic-multi", "fluent:1")
        # A second -L adds to the request rather than replacing it.
        script = license_cluster.write_file("lic-both.sh", "#!/bin/bash\nsleep 60\n")
        both = parse_job_id(
            license_cluster.sbatch(
                ["-J", "lic-both", "-L", "fluent:1", "-L", "matlab:1", script]
            )
        )
        assert both is not None
        try:
            wait_job_state(license_cluster, job_id, "R", timeout=90)
            wait_job_state(license_cluster, both, "R", timeout=90)
        finally:
            for jid in (job_id, both):
                license_cluster.cli_allow_fail(["scancel", str(jid)])

    def test_srun_accepts_the_licenses_flag(self, license_cluster):
        code, out = license_cluster.srun_with_exit(
            ["-L", "fluent:1", "echo", "SRUN_LIC_OK"]
        )
        assert code == 0, out
        assert "SRUN_LIC_OK" in out, out
