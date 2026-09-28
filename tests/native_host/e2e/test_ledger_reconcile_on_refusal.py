# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for reconciling a node's ledger when it refuses a dispatch.

A node that refuses a launch because it already holds the requested GPUs is
describing exactly the drift the admission-ledger reconcile exists to
resolve. Before this fix the refusal only ever reached the controller as a
bare `resource_exhausted` transport error, so it was requeued and retried
blindly — the controller never looked at the node.

This reproduces the drift with a real GPU job: hold one GPU, cancel it while
the controller's route to the node is blocked (so the completing-timeout
force-finish frees the slice in Raft without the agent ever hearing about
it), then unblock and dispatch a second job that requests every GPU on the
node. The second job's first attempt collides with the still-held GPU; the
refusal must be what pulls the node's ledger, resolves the orphaned claim,
and lets the job land cleanly on retry — not a heartbeat, not a restart, not
an operator `Reconcile=yes`.

Requires real GPU hardware (see gpu_preflight) — the overlap this fix
classifies is only reachable through the agent's live `NodeAllocation`, and a
GPU-less bed can never get a launch far enough to populate it.
"""

import re
import time

import pytest

from cluster import block_agent_port, parse_job_id, wait_job, wait_job_state

# Short so the force-finish that creates the drift lands quickly.
COMPLETE_WAIT = 8
# completing timeout is polled on a fixed 10s tick regardless of
# complete_wait_secs; give it a full cycle plus margin.
FORCE_FINISH_BOUND = COMPLETE_WAIT + 20


def _job_fields(cluster, job_id: int) -> dict[str, str]:
    out = cluster.scontrol("show", "job", str(job_id))
    return dict(re.findall(r"(\w+)=(\S+)", out))


def _strip_ansi(text: str) -> str:
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def _controller_log(cluster) -> str:
    return _strip_ansi(cluster.nodes[0].read_file(f"{cluster.log_dir}/spurctld.log"))


def _hold_marker_alive(cluster, node_index: int, marker: str) -> bool:
    # Bracket the first char so pgrep cannot match its own command line.
    out = cluster.nodes[node_index].exec_allow_fail(f"pgrep -f '[s]leep {marker}' || true")
    return bool(out.strip())


def _wait_for_terminal_cancel(cluster, job_id: int, timeout: int) -> str:
    """Wait for the held job to leave RUNNING via the completing-timeout
    force-finish, independent of whether the agent ever confirmed release."""
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        fields = _job_fields(cluster, job_id)
        last = fields.get("JobState", "")
        if last in ("CANCELLED", "FAILED", "COMPLETED", "TIMEOUT", "NODE_FAIL"):
            return last
        time.sleep(1)
    raise TimeoutError(f"job {job_id} never reached a terminal state (last: {last})")


class TestLedgerReconcileOnRefusal:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {"scheduler": {"complete_wait_secs": COMPLETE_WAIT}}

    def test_dispatch_refusal_triggers_reconcile_and_resolves_the_conflict(
        self, gpu_cluster
    ):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        node_name = cluster.node_names[0]
        total_gpus = cluster.node_gpu_count(node_name)
        if total_gpus < 1:
            pytest.skip(f"{node_name} advertises no GPUs: {cluster.scontrol_show_node(node_name)}")

        marker = "8675301"
        hold_script = cluster.write_file("ledger-hold.sh", f"#!/bin/bash\nsleep {marker}\n")
        hold_job = parse_job_id(
            cluster.sbatch(
                ["-J", "ledger-hold", "-N", "1", "-w", node_name, "--gres=gpu:1", hold_script]
            )
        )
        assert hold_job is not None
        wait_job_state(cluster, hold_job, "R", timeout=60)
        assert _hold_marker_alive(cluster, 0, marker), "hold job never actually launched"

        baseline_log = _controller_log(cluster)

        try:
            # The controller's own route to the node is blocked, not the
            # node's inbound (which would just make it look DOWN) — the
            # completing-timeout cancel this races against genuinely fails
            # at the RPC layer, exactly like a dropped network segment would.
            with block_agent_port(cluster, node_index=0):
                cluster.scancel(hold_job)
                final = _wait_for_terminal_cancel(cluster, hold_job, FORCE_FINISH_BOUND)
                assert final == "CANCELLED", (
                    f"expected the completing-timeout to force-finish the hold "
                    f"job to CANCELLED, got {final}\n{cluster.debug_job(hold_job)}"
                )
                # The controller believes the slice is free (force-finish freed
                # it in Raft); the agent was never told, so the process lives on.
                assert _hold_marker_alive(cluster, 0, marker), (
                    "the hold job's process must still be alive here — if it "
                    "died, the cancel reached the agent and there is no drift "
                    "left for a refusal to resolve"
                )

            # Block lifted. Dispatch a job requesting every GPU on the node
            # immediately: the controller believes the node is fully free, so
            # it will collide with the GPU the orphaned hold job still holds.
            overlap_script = cluster.write_file("ledger-overlap.sh", "#!/bin/bash\necho hi\n")
            overlap_job = parse_job_id(
                cluster.sbatch(
                    [
                        "-J", "ledger-overlap", "-N", "1", "-w", node_name,
                        f"--gres=gpu:{total_gpus}", overlap_script,
                    ]
                )
            )
            assert overlap_job is not None

            final2 = wait_job(cluster, overlap_job, timeout=60)
            assert final2 == "CD", (
                f"the overlap job must complete once the refusal reconciles "
                f"the node, got {final2}\n{cluster.debug_job(overlap_job)}"
            )

            fields = _job_fields(cluster, overlap_job)
            assert fields.get("Restarts", "0") != "0", (
                "the overlap job must show at least one restart — its first "
                f"attempt has to be the one that got refused: {fields}"
            )

            new_log = _controller_log(cluster)[len(baseline_log):]
            assert "node holds unaccounted resources" in new_log, (
                f"the refusal must be classified as an unaccounted-resources "
                f"conflict, not a generic rejection\n{new_log}"
            )
            assert 'reason="dispatch refused"' in new_log, (
                f"the refusal itself must be what pulls the node's ledger\n{new_log}"
            )

            # The pull is spawned off the confirm-dispatch path, so its own
            # completion can log after the job is already seen as CD; poll
            # rather than assume it landed in the snapshot taken above.
            deadline = time.time() + 15
            reconciled_log = ""
            while time.time() < deadline:
                reconciled_log = _controller_log(cluster)[len(baseline_log):]
                if "reconciled this node's ledger" in reconciled_log:
                    break
                time.sleep(1)
            agent_log = _strip_ansi(cluster.nodes[0].read_file(f"{cluster.log_dir}/spurd.log"))
            assert "reconciled this node's ledger" in reconciled_log, (
                f"the pull must actually reconcile, not just fire\n{reconciled_log}"
                f"\n---agent log tail---\n{agent_log[-4000:]}"
            )

            # The orphaned hold job's process must be gone: the reconcile
            # resolved the conflict rather than leaving a residual double-book.
            deadline = time.time() + 20
            cleaned = False
            while time.time() < deadline:
                if not _hold_marker_alive(cluster, 0, marker):
                    cleaned = True
                    break
                time.sleep(1)
            assert cleaned, (
                "the orphaned hold job's process must be reaped once the "
                "reconcile resolves it, or the node ends up with a residual "
                "double-count"
            )

            # And the node itself must be back in clean, schedulable state —
            # not administratively drained by the conflict it just resolved.
            show = cluster.scontrol_show_node(node_name)
            assert "State=DRAINED" not in show and "State=DRAIN" not in show, (
                f"resolving the conflict must not leave the node drained\n{show}"
            )
        finally:
            cluster.scancel(str(hold_job))

    def test_unreachable_dispatch_failure_does_not_trigger_reconcile(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        node_name = cluster.node_names[0]

        baseline_log = _controller_log(cluster)
        script = cluster.write_file("ledger-unreachable.sh", "#!/bin/bash\nsleep 30\n")

        job_id = None
        try:
            with block_agent_port(cluster, node_index=0):
                job_id = parse_job_id(
                    cluster.sbatch(
                        ["-J", "ledger-unreachable", "-N", "1", "-w", node_name,
                         "--gres=gpu:1", script]
                    )
                )
                assert job_id is not None

                # `Reason=` in scontrol's output holds free text with spaces,
                # so the raw text is searched directly rather than through the
                # single-token `key=value` field parser _job_fields uses.
                deadline = time.time() + 30
                show = ""
                while time.time() < deadline:
                    show = cluster.scontrol("show", "job", str(job_id))
                    if "agent unreachable" in show:
                        break
                    time.sleep(1)
                assert "agent unreachable" in show, (
                    f"expected an ordinary agent-unreachable rejection\n{show}"
                )

            new_log = _controller_log(cluster)[len(baseline_log):]
            assert "pulling this node's ledger" not in new_log, (
                "an ordinary unreachable/timed-out dispatch failure must "
                f"never trigger a reconcile pull\n{new_log}"
            )
        finally:
            if job_id is not None:
                cluster.scancel(str(job_id))
