# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""admission.node_names changes through spur.conf and scontrol reconfigure."""

import shlex

import pytest

from cluster import parse_job_id, wait_job_state, wait_node_state, wait_until


@pytest.fixture(params=["open", "token"])
def node_names_cluster(unstarted_cluster, request):
    c = unstarted_cluster
    if len(c.nodes) < 2:
        pytest.skip("node_names tests require two nodes")
    c.start_controller(config_overrides={
        "admission": {"mode": request.param, "node_names": list(c.node_names)},
        "auth": {"plugin": "jwt", "jwt_key": "node-names-e2e-test-key"},
    })
    token = None
    if request.param == "token":
        token = c.cli(["spur", "token", "create"]).strip().splitlines()[0]
        assert "." in token
    c.start_agents(token=token)
    c.wait_ready()
    return c


def _write_node_names(c, names):
    admission = c.config_overrides["admission"]
    if names is None:
        admission.pop("node_names", None)
    else:
        admission["node_names"] = list(names)
    c._write_config()


def _fields(output, keys):
    fields = dict(part.split("=", 1) for part in output.split() if "=" in part)
    assert set(keys) <= fields.keys(), output
    return {key: fields[key] for key in keys}


def _assert_rejected(c, reason, names):
    code, output = c.cli_with_exit(["scontrol", "reconfigure"])
    assert code != 0, output
    assert reason in output, output
    assert set(c.sinfo_nodes()) == set(names)


@pytest.mark.parametrize("suspended", [False, True], ids=["running", "suspended"])
def test_reconfigure_preserves_busy_job_and_idle_node_then_retries(node_names_cluster, suspended):
    c = node_names_cluster
    busy = c.node_names[0]
    release = f"{c.remote_dir}/release-job"
    survived = f"{c.remote_dir}/job-survived"
    script = c.write_file(
        "names-busy.sh",
        "#!/bin/sh\n"
        f"while [ ! -f {shlex.quote(release)} ]; do sleep 0.1; done\n"
        f"echo survived > {shlex.quote(survived)}\n",
    )
    output = c.sbatch([
        "-N", "1", "-w", busy, "-t", "5", "-D", c.remote_dir,
        "-o", f"{c.remote_dir}/names-busy.out", script,
    ])
    job_id = parse_job_id(output)
    assert job_id is not None, output
    try:
        wait_job_state(c, job_id, "R")
        if suspended:
            c.scontrol("suspend", str(job_id))
            wait_job_state(c, job_id, "S")
        job_keys = ["JobState", "NodeList", "NumNodes", "NumTasks", "CPUs/Task", "Restarts"]
        job_before = _fields(c.scontrol("show", "job", str(job_id)), job_keys)
        node_keys = ["CPUAlloc", "CPUTot"]
        node_before = _fields(c.scontrol("show", "node", busy), node_keys)

        _write_node_names(c, [])
        _assert_rejected(c, "job allocations", c.node_names)

        assert _fields(c.scontrol("show", "job", str(job_id)), job_keys) == job_before
        assert _fields(c.scontrol("show", "node", busy), node_keys) == node_before
        c.restart_agent(1)
        wait_node_state(c, c.node_names[1], ["idle"])
        if suspended:
            c.scontrol("resume", str(job_id))
        c.nodes[0].exec(f"touch {shlex.quote(release)}")
        wait_job_state(c, job_id, "CD")
        assert c.nodes[0].exec(f"cat {shlex.quote(survived)}").strip() == "survived"

        c.scontrol("reconfigure")
        assert c.sinfo_nodes() == {}
        assert "JobState=COMPLETED" in c.scontrol("show", "job", str(job_id))
    finally:
        c.cli_allow_fail(["scancel", str(job_id)])


def test_reconfigure_preserves_reservation_and_idle_node_then_retries(node_names_cluster):
    c = node_names_cluster
    reservation = "names-maintenance"
    c.cli_as_user(
        "root", [
            "scontrol", "create-reservation", f"--name={reservation}",
            "--start-time=now", "--duration=60", f"--nodes={c.node_names[1]}",
            f"--users={c.nodes[0].user}",
        ], check=True,
    )
    keys = ["ReservationName", "Nodes", "StartTime", "EndTime", "Users"]
    before = _fields(c.scontrol("show", "reservation", reservation), keys)
    _write_node_names(c, [])

    _assert_rejected(c, "reservation", c.node_names)

    assert _fields(c.scontrol("show", "reservation", reservation), keys) == before
    c.restart_agent(0)
    wait_node_state(c, c.node_names[0], ["idle"])
    c.cli_as_user("root", ["scontrol", "delete-reservation", reservation], check=True)
    c.scontrol("reconfigure")
    assert c.sinfo_nodes() == {}


def test_departed_node_stays_excluded_after_restart_until_readmitted(node_names_cluster):
    c = node_names_cluster
    removed = c.node_names[1]
    retained = [name for name in c.node_names if name != removed]
    c.signal_agent(1, "TERM")
    wait_node_state(c, removed, ["down"])
    c.nodes[1].exec(
        f"rm -rf {shlex.quote(c.state_dir)} && mkdir -p {shlex.quote(c.state_dir)}"
    )
    _write_node_names(c, retained)
    c.scontrol("reconfigure")
    assert set(c.sinfo_nodes()) == set(retained)

    # A stale startup file must not override the list saved in Raft.
    _write_node_names(c, c.node_names)
    c.restart_controller(expected_nodes=retained)
    assert set(c.sinfo_nodes()) == set(retained)
    c.restart_agent(1)
    wait_until(
        lambda: "admission.node_names" in c.spurd_log(1),
        timeout_s=30, interval_s=1, desc="excluded fresh agent registration rejected",
    )
    assert set(c.sinfo_nodes()) == set(retained)

    c.scontrol("reconfigure")
    c.restart_agent(1)
    wait_node_state(c, removed, ["idle"])
    assert set(c.sinfo_nodes()) == set(c.node_names)


def test_empty_list_and_clearing_it_restore_dynamic_admission(node_names_cluster):
    c = node_names_cluster
    c.stop_agents()
    for name in c.node_names:
        wait_node_state(c, name, ["down"])
    _write_node_names(c, [])
    c.scontrol("reconfigure")
    assert c.sinfo_nodes() == {}
    c.restart_controller(expected_nodes=[])
    assert c.sinfo_nodes() == {}
    c.restart_agent(1)
    wait_until(
        lambda: "admission.node_names" in c.spurd_log(1),
        timeout_s=30, interval_s=1, desc="empty list rejects registration",
    )
    assert c.sinfo_nodes() == {}

    _write_node_names(c, None)
    c.scontrol("reconfigure")
    c.restart_agent(0)
    c.restart_agent(1)
    for name in c.node_names[:2]:
        wait_node_state(c, name, ["idle"])


def test_omitted_list_preserves_nodes_when_policy_is_removed(node_names_cluster):
    c = node_names_cluster
    _write_node_names(c, None)
    c.scontrol("reconfigure")
    c.config_overrides["nodes"] = []
    c._write_config()
    c.scontrol("reconfigure")
    assert set(c.sinfo_nodes()) == set(c.node_names)


def test_startup_node_names_never_block_registration_or_delete(unstarted_cluster):
    c = unstarted_cluster
    if len(c.nodes) < 2:
        pytest.skip("node_names tests require two nodes")
    c.start_controller(config_overrides={"admission": {"mode": "open"}})
    c.start_agents()
    c.wait_ready()
    listed, busy = c.node_names[0], c.node_names[1]
    release = f"{c.remote_dir}/release-unlisted"
    script = c.write_file(
        "unlisted-busy.sh",
        f"#!/bin/sh\nwhile [ ! -f {shlex.quote(release)} ]; do sleep 0.1; done\n",
    )
    output = c.sbatch([
        "-N", "1", "-w", busy, "-t", "5", "-D", c.remote_dir,
        "-o", f"{c.remote_dir}/unlisted-busy.out", script,
    ])
    job_id = parse_job_id(output)
    assert job_id is not None, output
    try:
        wait_job_state(c, job_id, "R")
        _write_node_names(c, [listed])
        c.restart_controller()
        for index in (0, 1):
            c.restart_agent(index)
            c.wait_agent_serving(index)
            assert "admission.node_names" not in c.spurd_log(index)
        assert set(c.sinfo_nodes()) == set(c.node_names)
        assert "JobState=RUNNING" in c.scontrol("show", "job", str(job_id))

        _assert_rejected(c, "job allocations", c.node_names)
        c.nodes[1].exec(f"touch {shlex.quote(release)}")
        wait_job_state(c, job_id, "CD")
        c.scontrol("reconfigure")
        assert set(c.sinfo_nodes()) == {listed}
    finally:
        c.cli_allow_fail(["scancel", str(job_id)])


def test_edited_node_names_take_effect_only_through_reconfigure(node_names_cluster):
    c = node_names_cluster
    kept, dropped = c.node_names[0], c.node_names[1]
    c.scontrol("reconfigure")
    _write_node_names(c, [kept])
    c.restart_controller()
    c.restart_agent(1)
    c.wait_agent_serving(1)
    assert "differs from the saved list" in c.spurctld_log(0)
    assert "admission.node_names" not in c.spurd_log(1)
    wait_node_state(c, dropped, ["idle"])
    assert set(c.sinfo_nodes()) == set(c.node_names)

    c.scontrol("reconfigure")
    assert set(c.sinfo_nodes()) == {kept}
    assert dropped not in c.sinfo_nodes()


def _saved_list_writes(c):
    log = shlex.quote(f"{c.state_dir}/raft/log")
    output = c.nodes[0].exec(f"grep -l NodeInventoryUpdate {log}/*.json | wc -l")
    return int(output.strip())


def test_reconfigure_writes_node_names_only_when_the_list_changes(node_names_cluster):
    c = node_names_cluster
    c.scontrol("reconfigure")
    saved = _saved_list_writes(c)
    assert saved == 1
    c.scontrol("reconfigure")
    assert _saved_list_writes(c) == saved

    _write_node_names(c, None)
    c.scontrol("reconfigure")
    c.scontrol("reconfigure")
    assert _saved_list_writes(c) == saved + 1
    assert set(c.sinfo_nodes()) == set(c.node_names)
