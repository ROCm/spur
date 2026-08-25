# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests for structured JSON logging.

Runs real spurctld/spurd daemons and asserts they emit the required flat JSON
schema on stderr when ``[logging] format = "json"``, that structured fields keep
their native JSON type (a numeric ``job_id`` stays a number), and that
``format = "text"`` remains human-readable.
"""

import json

from cluster import parse_job_id, wait_job

REQUIRED_KEYS = {"timestamp", "level", "component", "target", "message"}
VALID_LEVELS = {"trace", "debug", "info", "warn", "error"}


def _parse_json_lines(raw: str, source: str) -> list[dict]:
    """Parse every non-empty line as a JSON object, failing loudly otherwise.

    A successful parse of every line is exactly the ``jq``-per-line acceptance
    criterion, checked in-process.
    """
    objs: list[dict] = []
    for i, line in enumerate(raw.splitlines()):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            obj = json.loads(stripped)
        except json.JSONDecodeError as e:
            raise AssertionError(f"{source} line {i} is not valid JSON ({e}): {stripped!r}")
        assert isinstance(obj, dict), f"{source} line {i} is not a JSON object: {stripped!r}"
        objs.append(obj)
    return objs


def _assert_schema(objs: list[dict], source: str, expected_component: str) -> None:
    assert objs, f"{source} produced no log lines"
    for obj in objs:
        missing = REQUIRED_KEYS - obj.keys()
        assert not missing, f"{source} line missing required keys {missing}: {obj}"
        assert obj["level"] in VALID_LEVELS, f"{source} has non-lowercase/invalid level: {obj['level']}"
        assert obj["component"] == expected_component, (
            f"{source} component {obj['component']!r} != {expected_component!r}"
        )
        assert isinstance(obj["target"], str) and obj["target"], f"{source} bad target: {obj}"
        assert isinstance(obj["message"], str), f"{source} message not a string: {obj}"


class TestJsonLogging:
    def test_daemons_emit_valid_json(self, unstarted_cluster):
        c = unstarted_cluster
        c.start(config_overrides={"logging": {"format": "json"}})

        ctrl = _parse_json_lines(
            c.nodes[0].read_file(f"{c.log_dir}/spurctld.log"), "spurctld.log"
        )
        _assert_schema(ctrl, "spurctld.log", "spurctld")
        assert any(o["message"] == "spurctld starting" for o in ctrl), (
            "expected a 'spurctld starting' line"
        )

        agent = _parse_json_lines(c.spurd_log(0), "spurd.log")
        _assert_schema(agent, "spurd.log", "spurd")
        assert any(o["message"] == "spurd starting" for o in agent), (
            "expected a 'spurd starting' line"
        )
        # spurd logs discovered resources with numeric fields at startup.
        discovered = [o for o in agent if o["message"] == "resources discovered"]
        assert discovered, "expected a 'resources discovered' line from spurd"
        assert isinstance(discovered[0]["cpus"], int) and not isinstance(
            discovered[0]["cpus"], bool
        ), f"cpus must be a JSON number, got {discovered[0]!r}"

    def test_job_id_is_flat_numeric_field(self, unstarted_cluster):
        c = unstarted_cluster
        c.start(config_overrides={"logging": {"format": "json"}})

        out_path = f"{c.remote_dir}/jsonlog.out"
        script = c.write_file("jsonlog-job.sh", "#!/bin/bash\necho JSONLOG_OK\n")
        sb = c.sbatch(["-J", "jsonlog", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"no job id in sbatch output:\n{sb}"
        wait_job(c, job_id, timeout=60)

        ctrl = _parse_json_lines(
            c.nodes[0].read_file(f"{c.log_dir}/spurctld.log"), "spurctld.log"
        )
        submitted = [
            o for o in ctrl if o["message"] == "job submitted" and o.get("job_id") == job_id
        ]
        assert submitted, (
            f"expected a 'job submitted' line with job_id={job_id}; "
            f"job_id-bearing lines: {[o for o in ctrl if 'job_id' in o]}"
        )
        # The whole point of the typed visitor: the id is a JSON number, not a string.
        for o in submitted:
            assert isinstance(o["job_id"], int) and not isinstance(o["job_id"], bool), (
                f"job_id must be a JSON number, got {type(o['job_id'])}: {o}"
            )

    def test_text_format_is_human_readable(self, unstarted_cluster):
        c = unstarted_cluster
        c.start(config_overrides={"logging": {"format": "text"}})

        raw = c.nodes[0].read_file(f"{c.log_dir}/spurctld.log")
        lines = [ln for ln in raw.splitlines() if ln.strip()]
        assert lines, "spurctld.log is empty in text mode"

        # No line should be a JSON object in text mode.
        for ln in lines:
            try:
                obj = json.loads(ln)
            except json.JSONDecodeError:
                continue
            assert not isinstance(obj, dict), f"text mode emitted a JSON object: {ln!r}"

        assert "spurctld starting" in raw, "expected human-readable startup line in text mode"
