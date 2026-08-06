# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the Slurm-compatible C FFI (libspur_compat.so).

A C driver (fixtures/ffi_smoke.c) is compiled against the shipped shared
library on the controller node and invoked per subcommand. The FFI reads the
controller endpoint from SLURM_CONTROLLER_ADDR -- the Slurm-prefixed name,
not SPUR_ -- so linked Slurm applications need no source change.
"""

import shlex

import pytest

from cluster import parse_job_id, wait_job, wait_job_state

# proto JobState values as surfaced by job_info_t.job_state.
JOB_PENDING = 0
JOB_RUNNING = 1
JOB_CANCELLED = 5


def _parse_kv(output: str) -> dict[str, str]:
    fields = {}
    for line in output.splitlines():
        key, sep, value = line.partition("=")
        if sep and " " not in key:
            fields[key.strip()] = value.strip()
    return fields


def _records(output: str, prefix: str) -> list[dict[str, str]]:
    """Parse `<prefix> k=v k=v` lines into dicts."""
    out = []
    for line in output.splitlines():
        if not line.startswith(f"{prefix} "):
            continue
        out.append(
            dict(
                token.split("=", 1)
                for token in line[len(prefix) + 1 :].split(" ")
                if "=" in token
            )
        )
    return out


class FfiDriver:
    """Runs the compiled C driver on the controller node."""

    def __init__(self, cluster, binary: str, lib_dir: str):
        self.cluster = cluster
        self.binary = binary
        self.lib_dir = lib_dir

    def run(self, args: list[str]) -> str:
        cmd = (
            f"SLURM_CONTROLLER_ADDR={shlex.quote(self.cluster.controller_addr)} "
            f"LD_LIBRARY_PATH={shlex.quote(self.lib_dir)} "
            + " ".join(shlex.quote(a) for a in [self.binary] + args)
            + " 2>&1"
        )
        return self.cluster.nodes[0].exec_allow_fail(cmd)

    def fields(self, args: list[str]) -> dict[str, str]:
        return _parse_kv(self.run(args))


@pytest.fixture
def ffi(cluster):
    lib = cluster.ship_ffi_library()
    lib_dir = lib.rsplit("/", 1)[0]
    binary = cluster.compile_c_fixture(
        "ffi_smoke.c",
        extra_flags=[
            f"-L{lib_dir}",
            "-lspur_compat",
            f"-Wl,-rpath,{lib_dir}",
            "-pthread",
        ],
        all_nodes=False,
    )
    return FfiDriver(cluster, binary, lib_dir)


class TestJobDescDefaults:
    def test_init_job_desc_msg_clears_the_struct(self, ffi):
        """A caller passes in uninitialised stack memory; init must overwrite
        every field, not just the ones it cares about."""
        fields = ffi.fields(["defaults"])
        assert fields["name_null"] == "1", f"name was not cleared: {fields}"
        assert fields["script_null"] == "1", f"script was not cleared: {fields}"
        assert fields["min_nodes"] == "1"
        assert fields["max_nodes"] == "1"
        assert fields["cpus_per_task"] == "1"
        assert fields["time_limit"] == "0"
        assert fields["num_tasks_is_no_val"] == "1", (
            f"num_tasks must default to NO_VAL: {fields}"
        )


class TestSubmitAndQuery:
    def test_submit_then_load_and_kill(self, ffi):
        out = ffi.run(
            ["submit", "ffi-job", "default", "#!/bin/bash\nsleep 120\n"]
        )
        fields = _parse_kv(out)
        assert fields.get("rc") == "0", f"submit failed:\n{out}"
        job_id = int(fields["job_id"])
        assert job_id > 0, f"submit returned no job id:\n{out}"

        try:
            wait_job_state(ffi.cluster, job_id, "R", timeout=90)

            listing = ffi.run(["jobs", str(job_id)])
            assert _parse_kv(listing).get("rc") == "0", f"load_jobs failed:\n{listing}"
            rows = _records(listing, "job")
            assert len(rows) == 1, f"expected exactly one match:\n{listing}"
            row = rows[0]
            assert row["name"] == "ffi-job"
            assert row["partition"] == "default"
            assert int(row["state"]) == JOB_RUNNING, (
                f"FFI must report the live state: {row}"
            )
            assert row["nodelist"], f"a running job must carry a nodelist: {row}"
        finally:
            killed = ffi.run(["kill", str(job_id), "9"])

        assert _parse_kv(killed).get("rc") == "0", f"kill_job failed:\n{killed}"
        assert wait_job(ffi.cluster, job_id, timeout=90) in ("CA", "GONE"), (
            f"job {job_id} was not cancelled by slurm_kill_job"
        )

    def test_load_jobs_sees_a_cli_submitted_job(self, ffi):
        """The FFI and the CLI must share one view of the queue."""
        script = ffi.cluster.write_file("ffi-cli.sh", "#!/bin/bash\nsleep 60\n")
        job_id = parse_job_id(ffi.cluster.sbatch(["-J", "ffi-cli", script]))
        assert job_id is not None

        try:
            listing = ffi.run(["jobs", str(job_id)])
            rows = _records(listing, "job")
            assert len(rows) == 1, f"CLI job {job_id} not visible over FFI:\n{listing}"
            assert rows[0]["name"] == "ffi-cli"
        finally:
            ffi.cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_submit_defaults_to_one_node(self, ffi):
        out = ffi.run(["submit", "ffi-default", "", "#!/bin/bash\ntrue\n"])
        job_id = int(_parse_kv(out)["job_id"])

        try:
            rows = _records(ffi.run(["jobs", str(job_id)]), "job")
            assert rows[0]["nodes"] == "1", f"expected a one-node job: {rows[0]}"
            assert rows[0]["partition"] == "default", (
                f"an unset partition must fall back to the default: {rows[0]}"
            )
        finally:
            ffi.cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_kill_of_a_pending_job_cancels_it(self, ffi):
        script = ffi.cluster.write_file("ffi-held.sh", "#!/bin/bash\nsleep 60\n")
        job_id = parse_job_id(ffi.cluster.sbatch(["-J", "ffi-held", "-H", script]))
        assert job_id is not None
        wait_job_state(ffi.cluster, job_id, "PD", timeout=30)

        rows = _records(ffi.run(["jobs", str(job_id)]), "job")
        assert int(rows[0]["state"]) == JOB_PENDING, (
            f"held job must read back as pending: {rows[0]}"
        )

        assert _parse_kv(ffi.run(["kill", str(job_id), "9"])).get("rc") == "0"
        assert wait_job(ffi.cluster, job_id, timeout=60) in ("CA", "GONE")


class TestClusterQueries:
    def test_load_node_reports_every_node(self, ffi):
        out = ffi.run(["nodes"])
        assert _parse_kv(out).get("rc") == "0", f"load_node failed:\n{out}"
        rows = {r["name"]: r for r in _records(out, "node")}
        assert set(ffi.cluster.node_names) <= set(rows), (
            f"expected {ffi.cluster.node_names}, got {sorted(rows)}"
        )
        first = rows[ffi.cluster.node_names[0]]
        assert int(first["cpus"]) > 0, f"node must report CPUs: {first}"
        assert int(first["memory"]) > 0, f"node must report memory: {first}"

    def test_load_partitions_reports_the_default_partition(self, ffi):
        out = ffi.run(["partitions"])
        assert _parse_kv(out).get("rc") == "0", f"load_partitions failed:\n{out}"
        rows = {r["name"]: r for r in _records(out, "partition")}
        assert "default" in rows, f"expected the default partition: {sorted(rows)}"
        assert int(rows["default"]["total_nodes"]) >= len(ffi.cluster.node_names)


class TestErrorPaths:
    def test_calls_fail_cleanly_when_the_controller_is_unreachable(self, ffi):
        """A wrong address must return -1, not hang or abort the process."""
        cmd = (
            "SLURM_CONTROLLER_ADDR=http://127.0.0.1:1 "
            f"LD_LIBRARY_PATH={shlex.quote(ffi.lib_dir)} "
            f"{shlex.quote(ffi.binary)} nodes 2>&1"
        )
        out = ffi.cluster.nodes[0].exec_allow_fail(cmd)
        assert _parse_kv(out).get("rc") == "-1", (
            f"an unreachable controller must return -1:\n{out}"
        )

    def test_kill_of_an_unknown_job_reports_failure(self, ffi):
        out = ffi.run(["kill", "99999999", "9"])
        assert _parse_kv(out).get("rc") == "-1", (
            f"killing an unknown job must return -1:\n{out}"
        )

    def test_strerror_maps_known_codes(self, ffi):
        assert ffi.fields(["strerror", "0"])["message"] == "Success"
        assert ffi.fields(["strerror", "-2"])["message"] == "Invalid job id"
        unknown = ffi.fields(["strerror", "-9999"])["message"]
        assert unknown, "strerror must never return an empty string"
