# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for parsing the agent's pending-cleanup records read over SSH."""

from cluster import parse_cleanup_records

RECORD = (
    '{"job_id":7,"run_attempt":1,"step_id":4294967291,'
    '"cgroup":"/sys/fs/cgroup/spur/job_7_1","deadline_unix_ms":0,"quarantine_acknowledged":true}'
)


def test_no_records_parses_to_an_empty_list():
    assert parse_cleanup_records("") == []
    assert parse_cleanup_records("\n") == []


def test_one_record_per_line():
    records = parse_cleanup_records(RECORD + "\n")

    assert len(records) == 1
    assert records[0]["job_id"] == 7
    assert records[0]["quarantine_acknowledged"] is True


def test_several_records_parse_in_order():
    records = parse_cleanup_records(f"{RECORD}\n{RECORD}\n")

    assert [r["step_id"] for r in records] == [4294967291, 4294967291]
