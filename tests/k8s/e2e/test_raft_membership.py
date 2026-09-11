# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Runtime growth and shrink of the controller set on Kubernetes.

Starts from one controller whose seed list names only itself, scales the
StatefulSet to three, adds and promotes the two new replicas through
`spur admin raft`, removes one again, and proves the cluster still elects a
leader and runs a job.
"""

import re
import time

from k8s_cluster import (
    DEFAULT_TIMEOUT,
    HA_TIMEOUT,
    WAIT_INTERVAL,
    assert_eventually,
    delete_pod,
    delete_pvc,
    pod_state,
    scale_controllers,
    service_endpoint_pods,
    simple_spurjob,
    spur_admin,
    start_admin_pod,
    wait_pod_ready,
    wait_spurjob_state,
    wait_until,
)

# The liveness probe in the manifest: initialDelaySeconds 15 plus three
# failed periods of 20 seconds. A waiting pod that survives this long is not
# being killed by it.
LIVENESS_KILL_WINDOW = 15 + 3 * 20

_HEADER = re.compile(
    r"answered by node (\d+) \((\w+)\), leader (\S+), last_log_index (\d+)"
)


def parse_raft_status(text: str) -> dict:
    lines = [line for line in text.splitlines() if line.strip()]
    header = _HEADER.match(lines[0]) if lines else None
    assert header, f"unexpected status output:\n{text}"
    members = {}
    for line in lines[1:]:
        parts = line.split()
        if len(parts) < 4 or not parts[0].isdigit():
            continue
        members[int(parts[0])] = {
            "address": parts[1],
            "role": parts[2],
            "matched": None if parts[3] == "-" else int(parts[3]),
        }
    return {
        "this_node": int(header.group(1)),
        "state": header.group(2),
        "leader": None if header.group(3) == "none" else int(header.group(3)),
        "last_log_index": int(header.group(4)),
        "members": members,
    }


def leader_status(namespace: str) -> dict:
    """Status as the leader sees it: only the leader knows MATCHED."""
    status = parse_raft_status(spur_admin(namespace, "spurctld-0", ["raft", "status"]))
    if status["leader"] is None:
        raise AssertionError("no leader")
    if status["leader"] != status["this_node"]:
        leader_pod = f"spurctld-{status['leader'] - 1}"
        status = parse_raft_status(spur_admin(namespace, leader_pod, ["raft", "status"]))
    return status


def voters(status: dict) -> set[int]:
    return {node_id for node_id, m in status["members"].items() if m["role"] == "voter"}


def raft_address(namespace: str, pod: str) -> str:
    return f"{pod}.spurctld.{namespace}.svc.cluster.local:6821"


class TestRaftMembership:
    def test_grow_to_three_shrink_to_two_and_survive_a_leader_loss(self, seed_cluster):
        ns = seed_cluster.namespace
        start_admin_pod(ns, seed_cluster.config.image)

        status = leader_status(ns)
        assert voters(status) == {1}, status

        # A new replica finds the running cluster, does not bootstrap, and
        # waits. It is alive on the Raft port but not ready, so the client
        # Service must not route to it, and the liveness probe must leave it.
        scale_controllers(ns, 3)
        new_pods = ["spurctld-1", "spurctld-2"]
        wait_until(
            lambda: all(pod_state(ns, p)[0] == "Running" for p in new_pods),
            HA_TIMEOUT,
            "new controller pods not running",
        )
        deadline = time.time() + LIVENESS_KILL_WINDOW
        while time.time() < deadline:
            for pod in new_pods:
                phase, ready, restarts = pod_state(ns, pod)
                assert phase == "Running", f"{pod} is {phase}"
                assert not ready, f"{pod} became ready before it was a member"
                assert restarts == 0, f"{pod} was restarted while waiting to be added"
            time.sleep(WAIT_INTERVAL)
        routed = service_endpoint_pods(ns, "spurctld-client")
        assert routed == {"spurctld-0"}, f"client Service routes to {routed}"

        for node_id, pod in ((2, "spurctld-1"), (3, "spurctld-2")):
            out = spur_admin(ns, "spurctld-0", ["raft", "add-learner", str(node_id), raft_address(ns, pod)])
            assert f"added node {node_id}" in out, out

        # A learner can leave again before it is promoted, and come back.
        out = spur_admin(ns, "spurctld-0", ["raft", "remove", "3"])
        assert "removed node 3" in out, out
        assert 3 not in leader_status(ns)["members"]
        out = spur_admin(ns, "spurctld-0", ["raft", "add-learner", "3", raft_address(ns, "spurctld-2")])
        assert "added node 3" in out, out

        def learners_caught_up() -> bool:
            status = leader_status(ns)
            return all(
                status["members"].get(node_id, {}).get("matched") == status["last_log_index"]
                for node_id in (2, 3)
            )

        assert_eventually(HA_TIMEOUT, WAIT_INTERVAL, "learners did not catch up", learners_caught_up)

        for node_id in (2, 3):
            out = spur_admin(ns, "spurctld-0", ["raft", "promote", str(node_id)])
            assert f"promoted node {node_id}" in out, out
        assert voters(leader_status(ns)) == {1, 2, 3}
        for pod in new_pods:
            wait_pod_ready(ns, pod, HA_TIMEOUT)

        out = spur_admin(ns, "spurctld-0", ["raft", "remove", "3"])
        assert "removed node 3" in out, out
        assert voters(leader_status(ns)) == {1, 2}
        scale_controllers(ns, 2)
        wait_until(
            lambda: pod_state(ns, "spurctld-2")[0] == "Missing",
            HA_TIMEOUT,
            "spurctld-2 still present after scale-down",
        )
        delete_pvc(ns, "spool-spurctld-2")

        leader = f"spurctld-{leader_status(ns)['leader'] - 1}"
        delete_pod(ns, leader)
        for pod in ("spurctld-0", "spurctld-1"):
            wait_pod_ready(ns, pod, HA_TIMEOUT)

        def two_voters_elected() -> bool:
            try:
                status = leader_status(ns)
            except AssertionError:
                return False
            return status["leader"] in (1, 2) and voters(status) == {1, 2}

        assert_eventually(HA_TIMEOUT, WAIT_INTERVAL, "no leader among the two voters", two_voters_elected)

        job = simple_spurjob("it-membership", ["sh", "-c", "echo MEMBERSHIP_OK"])
        seed_cluster.create_spurjob(job)
        completed = wait_spurjob_state(seed_cluster, "it-membership", "Completed", timeout=DEFAULT_TIMEOUT * 2)
        assert (completed.get("status") or {}).get("spurJobId") is not None
