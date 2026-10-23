# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Run against a disposable simulated deployment through an operator adapter."""

import json
import os
import re
import subprocess
import time

import pytest


class Simulation:
    def __init__(self, command):
        self.command = command
        self.jobs = []

    def call(self, *args, check=True, stdin=None):
        result = subprocess.run([self.command, *map(str, args)], input=stdin,
                                text=True, capture_output=True, timeout=90)
        if check:
            assert result.returncode == 0, result.stdout + result.stderr
        return result

    def cli(self, *args, **kwargs):
        return self.call("cli", *args, **kwargs)

    def wait(self, predicate):
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(0.3)
        pytest.fail("simulated cluster did not reach the expected state")

    def submit(self, expression=None, *, nodes=1, gpus=1, extra=()):
        args = ["submit", "--parsable", "-N", str(nodes), "-n", str(nodes),
                "--gpus", str(gpus), "-t", "00:02:00", "--chdir", "/state"]
        if expression is not None:
            args += ["--constraint", expression]
        job = int(self.cli(*args, *extra, "--wrap", "sleep 120").stdout.strip())
        self.jobs.append(job)
        return job

    def detail(self, job):
        return self.cli("control", "show", "job", job).stdout

    def running(self, job):
        return self.wait(lambda: (text if "JobState=RUNNING" in (text := self.detail(job)) else None))

    def pending(self, job):
        text = self.wait(lambda: (text if "LastSchedEval=N/A" not in (text := self.detail(job)) else None))
        assert "JobState=PENDING" in text, text
        return text

    def selected(self, job):
        text = self.running(job)
        rows = re.findall(r"GpuPropertyNode=(\S+) Generation=(\d+) StableIds=\[([^]]*)\]", text)
        return {node: [int(s) for s in ids.split(",") if s] for node, _, ids in rows}

    def inventory(self):
        return json.loads(self.call("inventory").stdout)

    def cancel(self, job):
        self.cli("cancel", job)
        self.wait(lambda: any(state in self.detail(job) for state in ("JobState=CANCELLED", "JobState=COMPLETED")))
        self.jobs.remove(job)


@pytest.fixture(scope="session")
def simulation_adapter():
    command = os.environ.get("SPUR_SIM_CONTROL")
    if not command:
        pytest.skip("SPUR_SIM_CONTROL must point to an isolated simulation adapter")
    return command


@pytest.fixture
def sim(simulation_adapter):
    simulation = Simulation(simulation_adapter)
    yield simulation
    for job in list(simulation.jobs):
        simulation.cancel(job)
