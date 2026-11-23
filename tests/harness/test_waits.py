# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the job polling helpers.

A fake cluster replays scripted squeue output and a fake clock stands in for
`time`, so the real helpers run without sleeping.
"""

import pytest

import cluster as cluster_module
from cluster import wait_job, wait_job_id_by_name, wait_job_state

HEADER = "JOBID PARTITION NAME USER ST TIME NODES NODELIST(REASON)"


def queue(*rows):
    """squeue -t all text with one `<id> <state>` row per entry."""
    lines = [HEADER] + [f"{job_id} batch job vm {state} 0:01 1 node1" for job_id, state in rows]
    return "\n".join(lines) + "\n"


class FakeClock:
    """Advances virtual time on sleep so deadlines pass without waiting."""

    def __init__(self):
        self.now = 1000.0
        self.sleeps = []

    def time(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


@pytest.fixture(autouse=True)
def clock(monkeypatch):
    fake = FakeClock()
    monkeypatch.setattr(cluster_module, "time", fake)
    return fake


class FakeCluster:
    """Replays squeue_all outputs in order, repeating the last one."""

    def __init__(self, outputs=(), by_name=()):
        self.outputs = list(outputs)
        self.by_name = list(by_name)
        self.squeue_all_calls = 0
        self.squeue_calls = []

    def squeue_all(self):
        index = min(self.squeue_all_calls, len(self.outputs) - 1)
        self.squeue_all_calls += 1
        return self.outputs[index]

    def squeue(self, args):
        self.squeue_calls.append(args)
        index = min(len(self.squeue_calls) - 1, len(self.by_name) - 1)
        return self.by_name[index]


class TestWaitJobState:
    def test_returns_once_state_reached(self):
        fake = FakeCluster([queue((7, "PD")), queue((7, "PD")), queue((7, "R"))])

        assert wait_job_state(fake, 7, "R", timeout=60) is None
        assert fake.squeue_all_calls == 3

    def test_polls_every_two_seconds(self, clock):
        fake = FakeCluster([queue((7, "PD")), queue((7, "R"))])

        wait_job_state(fake, 7, "R", timeout=60)

        assert clock.sleeps == [2]

    def test_timeout_keeps_first_line_and_appends_queue(self):
        pending = queue((7, "PD"), (8, "R"))
        fake = FakeCluster([pending])

        with pytest.raises(TimeoutError) as excinfo:
            wait_job_state(fake, 7, "R", timeout=10)

        message = str(excinfo.value)
        assert message.splitlines()[0] == "Job 7 did not reach state R within 10s"
        assert message.endswith(pending.rstrip("\n"))

    def test_timeout_shows_only_the_last_queue_lines(self):
        rows = [(100 + i, "PD") for i in range(60)]
        fake = FakeCluster([queue(*rows)])

        with pytest.raises(TimeoutError) as excinfo:
            wait_job_state(fake, 7, "R", timeout=4)

        shown = str(excinfo.value).splitlines()[1:]
        assert len(shown) == 40
        assert shown[-1].startswith("159 ")
        assert not any(line.startswith("100 ") for line in shown)

    def test_timeout_reports_the_most_recent_queue(self):
        fake = FakeCluster([queue((7, "PD")), queue((7, "PD"), (9, "R"))])

        with pytest.raises(TimeoutError) as excinfo:
            wait_job_state(fake, 7, "R", timeout=6)

        assert "9 batch" in str(excinfo.value)

    def test_squeue_errors_propagate(self):
        class Broken:
            def squeue_all(self):
                raise RuntimeError("controller unreachable")

        with pytest.raises(RuntimeError, match="controller unreachable"):
            wait_job_state(Broken(), 7, "R", timeout=10)


class TestWaitJob:
    def test_returns_terminal_state(self):
        fake = FakeCluster([queue((7, "R")), queue((7, "CD"))])

        assert wait_job(fake, 7, timeout=60) == "CD"

    @pytest.mark.parametrize("state", ["CD", "F", "CA", "TO", "NF", "DL", "OOM"])
    def test_every_terminal_state_ends_the_wait(self, state):
        fake = FakeCluster([queue((7, state))])

        assert wait_job(fake, 7, timeout=60) == state

    @pytest.mark.parametrize("state", ["PD", "R", "S", "CG", "PR", "RQ"])
    def test_non_terminal_states_keep_waiting(self, state):
        fake = FakeCluster([queue((7, state))])

        with pytest.raises(TimeoutError):
            wait_job(fake, 7, timeout=10)

    def test_job_leaving_queue_returns_last_seen_state(self):
        fake = FakeCluster([queue((7, "R")), queue()])

        assert wait_job(fake, 7, timeout=60) == "R"

    def test_job_not_yet_visible_is_waited_for(self):
        fake = FakeCluster([queue(), queue(), queue((7, "R")), queue((7, "CD"))])

        assert wait_job(fake, 7, timeout=60) == "CD"

    def test_never_visible_job_times_out(self):
        fake = FakeCluster([queue((8, "R"))])

        with pytest.raises(TimeoutError):
            wait_job(fake, 7, timeout=10)

    def test_timeout_keeps_first_line_and_appends_queue(self):
        running = queue((7, "R"))
        fake = FakeCluster([running])

        with pytest.raises(TimeoutError) as excinfo:
            wait_job(fake, 7, timeout=10)

        message = str(excinfo.value)
        assert message.splitlines()[0] == "Job 7 did not finish within 10s (last state: R)"
        assert message.endswith(running.rstrip("\n"))

    def test_timeout_for_unseen_job_reports_empty_last_state(self):
        fake = FakeCluster([queue((8, "R"))])

        with pytest.raises(TimeoutError) as excinfo:
            wait_job(fake, 7, timeout=4)

        assert str(excinfo.value).splitlines()[0] == "Job 7 did not finish within 4s (last state: )"


class TestWaitJobIdByName:
    def test_returns_first_id_once_job_appears(self):
        fake = FakeCluster(by_name=["", "\n", "41\n42\n"])

        assert wait_job_id_by_name(fake, "srun-hold") == 41
        assert fake.squeue_calls[0] == ["-n", "srun-hold", "-t", "all", "-h", "-o", "%i"]
        assert len(fake.squeue_calls) == 3

    def test_ignores_non_numeric_lines(self):
        fake = FakeCluster(by_name=["JOBID\n", "  57  \n"])

        assert wait_job_id_by_name(fake, "x") == 57

    def test_times_out_when_job_never_appears(self):
        fake = FakeCluster(by_name=[""])

        with pytest.raises(TimeoutError, match="srun-hold"):
            wait_job_id_by_name(fake, "srun-hold", timeout=5)
