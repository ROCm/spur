# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for ``salloc --interactive-step`` (Slurm ``use_interactive_step``).

The test harness runs salloc over a non-TTY SSH channel, so unless a test wraps
salloc in a terminal (``tty=True``), a shell that sees a TTY can only have come
from the agent's PTY on the allocated node.
"""

import re

import pytest

from cluster import wait_job_state

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

    def test_sigint_before_the_shell_exits_cancels_the_allocation(self, cluster):
        # The harness's SSH exec has no controlling terminal, so the session never
        # enters raw mode and a SIGINT reaches salloc's own cancel handler.
        salloc = cluster.salloc_command(
            "sleep 300\n",
            salloc_args=["-N", "1", "-t", "0:05", "--interactive-step"],
            all_nodes=True,
        )
        # salloc's stdin is a quiet pipe that never reaches EOF. Its writer
        # keeps stderr off the SSH channel, or the harness would wait for it.
        code, out = cluster.nodes[0].exec_with_exit(
            "set -m; log=$(mktemp); "
            "exec 3< <(exec sleep 300 2>/dev/null); writer=$!; "
            f"{salloc} <&3 3<&- > \"$log\" 2>&1 & pid=$!; exec 3<&-; "
            "for _ in $(seq 1 120); do "
            "grep -q 'Granted job allocation' \"$log\" && break; sleep 0.5; done; "
            "sleep 2; kill -INT $pid; wait $pid; echo \"SALLOC_RC=$?\"; "
            "kill $writer 2>/dev/null; cat \"$log\"; rm -f \"$log\""
        )
        assert "SALLOC_RC=130" in out, out
        assert "cancelling job" in out, out
        granted = re.search(r"Granted job allocation (\d+)", out)
        assert granted, out
        wait_job_state(cluster, int(granted.group(1)), "CA", timeout=60)


class TestSallocInteractiveStepSiteDefault:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"salloc": {"use_interactive_step": True}}

    def test_config_enables_the_interactive_step(self, cluster):
        # Under `script` a local shell has a TTY too; only the agent sets the
        # node's rank, so that is what shows the shell ran on the node.
        code, out = cluster.salloc_run(
            _PROBE + 'echo "nodeid=${SLURM_NODEID:-unset}"\n', all_nodes=True, tty=True
        )
        assert code == 0, out
        assert _field(out, "nodeid") == "0", out

    def test_config_keeps_piped_salloc_local(self, cluster):
        code, out = cluster.salloc_run(_PROBE)
        assert code == 0, out
        assert "NO-TTY" in out, out

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
