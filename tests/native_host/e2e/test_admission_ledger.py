# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""A node records the slice each run holds, and only an acknowledged completion
gives it back."""

import json

from cluster import parse_job_id, job_state, wait_job, wait_job_state, wait_until

# The launch step of a batch job, mirroring STEP_BATCH in spur-core.
STEP_BATCH = 0xFFFF_FFFE


def run_dir(cluster, job_id: int, attempt: int = 1) -> str:
    return f"{cluster.state_dir}/admission/{job_id}.{attempt}"


def file_mode(cluster, path: str, node_index: int = 0) -> str:
    return cluster.nodes[node_index].exec_allow_fail(
        f"stat -c %a '{path}' 2>/dev/null || echo MISSING"
    ).strip()


def read_run_record(cluster, job_id: int, node_index: int = 0, attempt: int = 1):
    """The run's admission record as a dict, or None when it is not on disk."""
    raw = cluster.nodes[node_index].exec_allow_fail(
        f"cat '{run_dir(cluster, job_id, attempt)}/run.json' 2>/dev/null"
    ).strip()
    try:
        return json.loads(raw)
    except ValueError:
        return None


def read_participant(cluster, job_id: int, step_id: int, node_index: int = 0,
                     attempt: int = 1):
    raw = cluster.nodes[node_index].exec_allow_fail(
        f"cat '{run_dir(cluster, job_id, attempt)}/participants/{step_id}.json' "
        f"2>/dev/null"
    ).strip()
    try:
        return json.loads(raw)
    except ValueError:
        return None


def wait_run_record(cluster, job_id: int, node_index: int = 0, timeout: int = 60):
    return wait_until(
        lambda: read_run_record(cluster, job_id, node_index),
        f"job {job_id} never wrote an admission record on "
        f"{cluster.node_names[node_index]}",
        timeout=timeout,
    )


def run_supervisors(cluster, job_id: int, node_index: int = 0,
                    attempt: int = 1) -> set[str]:
    """Supervisor PIDs for one run, matched on this cluster's own state dir so a
    supervisor from another cluster sharing the node is never counted here."""
    out = cluster.nodes[node_index].exec_allow_fail(
        "ps -eww -o pid=,args= 2>/dev/null || true"
    )
    pids = set()
    for line in out.splitlines():
        fields = line.split()
        if len(fields) < 5 or not fields[1].endswith("spurstepd"):
            continue
        if fields[2:5] == [cluster.state_dir, str(job_id), str(attempt)]:
            pids.add(fields[0])
    return pids


def submit_holder(cluster, name: str, cpus: int, seconds: int,
                  node_index: int = 0) -> int:
    """Submit a job that holds *cpus* on one node for *seconds*, and wait for it."""
    script = cluster.write_file(
        f"{name}.sh", f"#!/bin/bash\nsleep {seconds}\n", all_nodes=True
    )
    job_id = parse_job_id(
        cluster.sbatch(
            ["-J", name, "-N", "1", "-w", cluster.node_names[node_index],
             "-c", str(cpus), script]
        )
    )
    assert job_id is not None, f"sbatch returned no job id for {name}"
    wait_job_state(cluster, job_id, "R")
    return job_id


class TestAdmissionRecord:
    def test_a_running_job_holds_a_private_record_of_the_slice_it_took(self, cluster):
        node = cluster.node_names[0]
        job_id = submit_holder(cluster, "adm-record", cpus=2, seconds=8)

        record = wait_run_record(cluster, job_id)
        assert file_mode(cluster, f"{cluster.state_dir}/admission") == "700"
        assert file_mode(cluster, run_dir(cluster, job_id)) == "700"
        assert file_mode(cluster, f"{run_dir(cluster, job_id)}/run.json") == "600"

        assert record["job_id"] == job_id, record
        assert record["run_attempt"] == 1, record
        assert record["node"] == node, record
        assert len(record["allocation"]["cpu_ids"]) == 2, record
        assert record["slice_released"] is False, record

        participant = read_participant(cluster, job_id, STEP_BATCH)
        assert participant is not None, (
            f"the launch step left no participant record under "
            f"{run_dir(cluster, job_id)}/participants"
        )
        assert file_mode(
            cluster, f"{run_dir(cluster, job_id)}/participants/{STEP_BATCH}.json"
        ) == "600"
        assert participant["job_id"] == job_id, participant
        assert participant["allocation_subset"]["cpu_ids"] == \
            record["allocation"]["cpu_ids"], participant

        assert wait_job(cluster, job_id, timeout=90) == "CD"
        wait_until(
            lambda: read_run_record(cluster, job_id) is None,
            f"job {job_id} completed but its admission record was never dropped",
        )
        assert cluster.node_cpu_alloc(node) == 0

    def test_no_two_running_jobs_are_given_the_same_cpu(self, cluster):
        node = cluster.node_names[0]
        if cluster.node_cpu_alloc(node) != 0:
            raise AssertionError(f"node {node} is not idle at the start of the test")
        total = int(cluster.scontrol_show_node(node).split("CPUTot=")[1].split()[0])
        wanted = min(4, total)
        assert wanted >= 2, f"node {node} has too few CPUs to contend for: {total}"

        job_ids = [
            submit_holder(cluster, f"adm-share-{i}", cpus=1, seconds=15)
            for i in range(wanted)
        ]
        assignments = {}
        for job_id in job_ids:
            record = wait_run_record(cluster, job_id)
            cpu_ids = record["allocation"]["cpu_ids"]
            assert len(cpu_ids) == 1, record
            assignments[job_id] = set(cpu_ids)

        assert cluster.node_cpu_alloc(node) == wanted
        for job_id, cpus in assignments.items():
            others = set().union(
                *(c for other, c in assignments.items() if other != job_id)
            )
            assert not (cpus & others), (
                f"job {job_id} was given CPUs another job already holds: {assignments}"
            )

        for job_id in job_ids:
            cluster.scancel(str(job_id))

    def test_every_node_of_a_multi_node_job_records_its_own_slice(
        self, multi_node_cluster
    ):
        cluster = multi_node_cluster
        script = cluster.write_file(
            "adm-multi.sh", "#!/bin/bash\nsleep 12\n", all_nodes=True
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "adm-multi", "-N", "2", "-c", "1", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        holders = []
        for index, name in enumerate(cluster.node_names):
            if cluster.node_cpu_alloc(name) == 0:
                continue
            record = wait_run_record(cluster, job_id, node_index=index)
            assert record["node"] == name, record
            assert record["allocation"]["cpu_ids"], record
            holders.append(name)

        assert len(holders) >= 2, (
            f"a two-node job must leave a record on both nodes, found {holders}"
        )
        cluster.scancel(str(job_id))


class TestRecordSurvivesAnAgentRestart:
    def test_a_restarted_agent_keeps_the_record_and_the_allocation(self, cluster):
        node = cluster.node_names[0]
        job_id = submit_holder(cluster, "adm-restart", cpus=3, seconds=40)
        before = wait_run_record(cluster, job_id)
        assert cluster.node_cpu_alloc(node) == 3
        supervisors = run_supervisors(cluster, job_id)
        assert supervisors, "a running job must have a supervisor"

        cluster.restart_agent(0)
        cluster.wait_agent_serving(0)

        after = read_run_record(cluster, job_id)
        assert after is not None, (
            "the restarted agent dropped the record of a run it is still holding"
        )
        # A record rebuilt from the surviving processes would carry a new stamp.
        assert after["created_at_unix_ms"] == before["created_at_unix_ms"], (
            f"the record was rewritten across the restart: {before} -> {after}"
        )
        assert after["allocation"]["cpu_ids"] == before["allocation"]["cpu_ids"]
        assert after["slice_released"] is False, after
        assert cluster.node_cpu_alloc(node) == 3, (
            f"the node gave up its slice across an agent restart:\n"
            f"{cluster.scontrol_show_node(node)}"
        )
        assert job_state(cluster.squeue_all(), job_id) == "R"
        assert supervisors <= run_supervisors(cluster, job_id), (
            "the job must be adopted rather than relaunched"
        )

        cluster.scancel(str(job_id))
        assert wait_job(cluster, job_id, timeout=90) in ("CA", "CD")
        wait_until(
            lambda: cluster.node_cpu_alloc(node) == 0,
            f"node {node} never got its CPUs back after the job ended",
        )


class TestReleaseAfterAcknowledgement:
    def test_a_node_holds_its_cores_until_the_controller_takes_the_completion(
        self, cluster
    ):
        node = cluster.node_names[0]
        job_id = submit_holder(cluster, "adm-ack", cpus=3, seconds=6)
        wait_run_record(cluster, job_id)
        assert cluster.node_cpu_alloc(node) == 3

        cluster.stop_controller()
        wait_until(
            lambda: not run_supervisors(cluster, job_id),
            f"job {job_id} never finished while the controller was down",
        )

        # The payload is gone and nothing can acknowledge it, so the slice is the
        # node's to keep: releasing it here is what would double-book the cores.
        for _ in range(6):
            record = read_run_record(cluster, job_id)
            assert record is not None, (
                "the record was dropped with no acknowledgement behind it"
            )
            assert record["slice_released"] is False, record
            assert record["controller_ack"]["release_raft_index"] is None, record
            assert record["controller_ack"]["settled_unrecorded_claim"] is False, record

        cluster.restart_controller()

        # A swept record is a released one: it leaves the spool only once the
        # slice has gone back.
        wait_until(
            lambda: (read_run_record(cluster, job_id) or {"slice_released": True})[
                "slice_released"
            ],
            f"job {job_id}'s slice was never released after the controller returned",
        )
        wait_until(
            lambda: cluster.node_cpu_alloc(node) == 0,
            f"node {node} never got its CPUs back after the controller returned",
        )


class TestEpilogHold:
    def test_a_cancelled_runs_cores_stay_held_until_the_epilog_ends(
        self, unstarted_cluster
    ):
        cluster = unstarted_cluster
        marker = f"{cluster.remote_dir}/epilog-finished"
        hook = cluster.write_file(
            "hooks/epilog.sh",
            f'#!/bin/bash\nsleep 5\ntouch "{marker}.$SPUR_JOB_ID"\n',
            all_nodes=True,
        )
        cluster.start(config_overrides={"hooks": {"epilog": hook}})

        node = cluster.node_names[0]
        job_id = submit_holder(cluster, "adm-epilog", cpus=3, seconds=120)
        assert cluster.node_cpu_alloc(node) == 3

        cluster.scancel(str(job_id))
        held_samples = 0

        def epilog_finished():
            nonlocal held_samples
            done = cluster.nodes[0].exec_allow_fail(
                f"test -f '{marker}.{job_id}' && echo YES || echo NO"
            ).strip()
            if done == "YES":
                return True
            assert cluster.node_cpu_alloc(node) == 3, (
                f"node {node} released the cancelled job's cores while its epilog "
                f"was still running:\n{cluster.scontrol_show_node(node)}"
            )
            held_samples += 1
            return False

        wait_until(epilog_finished, f"the epilog for job {job_id} never finished")
        assert held_samples >= 3, (
            f"the epilog ended too quickly to prove a hold ({held_samples} samples)"
        )
        wait_until(
            lambda: cluster.node_cpu_alloc(node) == 0,
            f"node {node} never got its CPUs back once the epilog ended",
        )
