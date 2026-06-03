# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Raft HA lifecycle E2E tests for Kubernetes."""

import json

from k8s_cluster import (
    DEFAULT_TIMEOUT,
    HA_TIMEOUT,
    WAIT_INTERVAL,
    assert_eventually,
    assert_leader_elected,
    count_ready_pods,
    delete_pod,
    exec_in_pod,
    read_file_from_pod,
    simple_spurjob,
    wait_pod_ready,
)


def assert_all_pods_ready(namespace: str, count: int) -> None:
    assert_eventually(
        HA_TIMEOUT,
        WAIT_INTERVAL,
        f"expected {count} ready controller pods",
        lambda: count_ready_pods(namespace, "app=spurctld") >= count,
    )


def find_leader_pod(namespace: str) -> str | None:
    for i in range(3):
        pod_name = f"spurctld-{i}"
        vote = read_file_from_pod(
            namespace, pod_name, "/var/spool/spur/raft/vote.json"
        )
        if '"committed":true' not in vote.replace(" ", ""):
            continue
        node_id = _parse_voted_node_id(vote)
        if node_id == i + 1:
            return pod_name
    return None


def _parse_voted_node_id(vote_json: str) -> int | None:
    try:
        data = json.loads(vote_json)
    except json.JSONDecodeError:
        return None
    node_id = data.get("node_id")
    return int(node_id) if node_id is not None else None


class TestRaftHA:
    def test_three_replica_deploy_and_leader_election(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

    def test_state_replication_pvcs_and_logs(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

        pvcs = ha_cluster.core_v1.list_namespaced_persistent_volume_claim(ns)
        bound = sum(
            1 for pvc in pvcs.items if (pvc.status.phase or "") == "Bound"
        )
        assert bound >= 3, f"expected >= 3 Bound PVCs, got {bound}"

        def logs_present() -> bool:
            for i in range(3):
                output = exec_in_pod(
                    ns, f"spurctld-{i}", ["ls", "/var/spool/spur/raft/log/"]
                )
                if not output.strip():
                    return False
            return True

        assert_eventually(
            HA_TIMEOUT, WAIT_INTERVAL, "some nodes missing Raft log entries", logs_present
        )

    def test_failover_recovery_after_pod_kill(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

        delete_pod(ns, "spurctld-0")
        wait_pod_ready(ns, "spurctld-0", HA_TIMEOUT)
        assert_all_pods_ready(ns, 3)

        def restarted_has_vote() -> bool:
            vote = read_file_from_pod(
                ns, "spurctld-0", "/var/spool/spur/raft/vote.json"
            )
            return '"committed":true' in vote.replace(" ", "")

        assert_eventually(
            HA_TIMEOUT, WAIT_INTERVAL, "restarted pod has no committed vote", restarted_has_vote
        )

    def test_state_survives_leader_failover(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

        job = simple_spurjob(
            "it-failover-state",
            ["sh", "-c", "echo FAILOVER_STATE_OK && sleep 5"],
        )
        ha_cluster.create_spurjob(job)

        def job_has_id() -> bool:
            job_obj = ha_cluster.get_spurjob("it-failover-state")
            return (job_obj.get("status") or {}).get("spurJobId") is not None

        assert_eventually(
            DEFAULT_TIMEOUT, WAIT_INTERVAL, "no job ID assigned before failover", job_has_id
        )
        job_before = ha_cluster.get_spurjob("it-failover-state")
        job_id_before = (job_before.get("status") or {}).get("spurJobId")

        leader = find_leader_pod(ns) or "spurctld-0"
        delete_pod(ns, leader)
        for i in range(3):
            wait_pod_ready(ns, f"spurctld-{i}", HA_TIMEOUT)

        job_after = ha_cluster.get_spurjob("it-failover-state")
        job_id_after = (job_after.get("status") or {}).get("spurJobId")
        assert job_id_after == job_id_before, (
            f"job ID changed across failover: before={job_id_before}, "
            f"after={job_id_after}"
        )
        assert_leader_elected(ns)

    def test_new_leader_accepts_writes(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

        pre_job = simple_spurjob(
            "it-pre-failover",
            ["sh", "-c", "echo PRE_FAILOVER && sleep 5"],
        )
        ha_cluster.create_spurjob(pre_job)

        def pre_has_id() -> bool:
            job_obj = ha_cluster.get_spurjob("it-pre-failover")
            return (job_obj.get("status") or {}).get("spurJobId") is not None

        assert_eventually(
            DEFAULT_TIMEOUT, WAIT_INTERVAL, "pre-failover job not accepted", pre_has_id
        )
        job_id_before = (ha_cluster.get_spurjob("it-pre-failover").get("status") or {}).get(
            "spurJobId"
        )

        leader = find_leader_pod(ns) or "spurctld-0"
        delete_pod(ns, leader)
        for i in range(3):
            wait_pod_ready(ns, f"spurctld-{i}", HA_TIMEOUT)
        assert_leader_elected(ns)

        post_job = simple_spurjob(
            "it-post-failover",
            ["sh", "-c", "echo POST_FAILOVER && sleep 2"],
        )
        ha_cluster.create_spurjob(post_job)

        def post_has_id() -> bool:
            job_obj = ha_cluster.get_spurjob("it-post-failover")
            return (job_obj.get("status") or {}).get("spurJobId") is not None

        assert_eventually(
            DEFAULT_TIMEOUT, WAIT_INTERVAL, "new job not accepted after failover", post_has_id
        )
        post_job_id = (ha_cluster.get_spurjob("it-post-failover").get("status") or {}).get(
            "spurJobId"
        )
        assert post_job_id > job_id_before, (
            f"job ID sequence broken: {post_job_id} <= {job_id_before}"
        )

    def test_log_replication_after_node_recovery(self, ha_cluster):
        ns = ha_cluster.namespace
        assert_all_pods_ready(ns, 3)
        assert_leader_elected(ns)

        min_logs = min(
            len(
                [
                    line
                    for line in exec_in_pod(
                        ns, f"spurctld-{i}", ["ls", "/var/spool/spur/raft/log/"]
                    ).splitlines()
                    if line.strip()
                ]
            )
            for i in range(3)
        )
        assert min_logs > 0, f"some nodes have no log entries (min={min_logs})"

        delete_pod(ns, "spurctld-2")
        wait_pod_ready(ns, "spurctld-2", HA_TIMEOUT)

        def recovered_has_logs() -> bool:
            output = exec_in_pod(
                ns, "spurctld-2", ["ls", "/var/spool/spur/raft/log/"]
            )
            return bool(output.strip())

        assert_eventually(
            HA_TIMEOUT, WAIT_INTERVAL, "recovered node has no log entries", recovered_has_logs
        )
