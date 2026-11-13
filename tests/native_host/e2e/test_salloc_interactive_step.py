# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for ``salloc --interactive-step`` (Slurm ``use_interactive_step``).

The test harness runs salloc over a non-TTY SSH channel, so a shell that sees a
TTY can only have come from the agent's PTY on the allocated node.
"""

import re

import pytest

_PROBE = """
test -t 1 && echo IS-TTY || echo NO-TTY
echo "jobid=${SPUR_JOB_ID:-unset}"
echo "submit=${SLURM_SUBMIT_DIR:-unset} pwd=$(pwd)"
"""


def _field(out: str, name: str) -> str:
    match = re.search(rf"{name}=(\S+)", out)
    assert match, f"no {name}= in output:\n{out}"
    return match.group(1)


class TestSallocInteractiveStep:
    def test_shell_runs_on_the_node_in_the_allocation(self, cluster):
        code, out = cluster.salloc_run(
            _PROBE,
            salloc_args=["-N", "1", "-t", "0:05", "--interactive-step"],
            all_nodes=True,
        )
        assert code == 0, out
        assert "IS-TTY" in out, out
        assert _field(out, "jobid") != "unset", out
        submit_dir = _field(out, "submit")
        assert submit_dir != "unset", out
        assert _field(out, "pwd") == submit_dir, out

    def test_salloc_exits_with_the_shell_exit_code(self, cluster):
        code, out = cluster.salloc_run(
            "exit 7\n",
            salloc_args=["-N", "1", "-t", "0:05", "--interactive-step"],
            all_nodes=True,
        )
        assert code == 7, out

    def test_default_shell_stays_on_the_submit_host(self, cluster):
        code, out = cluster.salloc_run(_PROBE)
        assert code == 0, out
        assert "NO-TTY" in out, out
        assert _field(out, "submit") != "unset", out


class TestSallocInteractiveStepSiteDefault:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"salloc": {"use_interactive_step": True}}

    def test_config_enables_the_interactive_step(self, cluster):
        code, out = cluster.salloc_run(_PROBE, all_nodes=True)
        assert code == 0, out
        assert "IS-TTY" in out, out

    def test_flag_keeps_the_shell_local(self, cluster):
        code, out = cluster.salloc_run(
            _PROBE,
            salloc_args=["-N", "1", "-t", "0:05", "--no-interactive-step"],
        )
        assert code == 0, out
        assert "NO-TTY" in out, out


class TestSallocInteractiveStepCgroup:
    def test_shell_runs_inside_the_job_cgroup(self, cgroup_cluster):
        code, out = cgroup_cluster.salloc_run(
            'grep -q "/spur/job_${SPUR_JOB_ID}_" /proc/self/cgroup '
            "&& echo IN-JOB-CGROUP || echo OUTSIDE-JOB-CGROUP\n",
            salloc_args=["-N", "1", "-t", "0:05", "--interactive-step"],
            all_nodes=True,
        )
        assert code == 0, out
        assert "IN-JOB-CGROUP" in out, out
