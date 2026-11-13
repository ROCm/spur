# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Authoritative roster changes through spur.conf and scontrol reconfigure."""

import shlex

import pytest

from cluster import parse_job_id, wait_job_state, wait_node_state, wait_until


@pytest.fixture(params=["open", "token"])
def roster_cluster(unstarted_cluster, request):
    c = unstarted_cluster
    if len(c.nodes) < 2:
        pytest.skip("node roster tests require two nodes")
    c.start_controller(config_overrides={
        "admission": {"mode": request.param, "node_names": list(c.node_names)},
        "auth": {"plugin": "jwt", "jwt_key": "roster-e2e-test-key"},
    })
    token = None
    if request.param == "token":
        token = c.cli(["spur", "token", "create"]).strip().splitlines()[0]
        assert "." in token
    c.start_agents(token=token)
    c.wait_ready()
    return c


def _write_roster(c, names):
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
def test_reconfigure_preserves_busy_job_and_idle_node_then_retries(roster_cluster, suspended):
    c = roster_cluster
    busy = c.node_names[0]
    release = f"{c.remote_dir}/release-job"
    survived = f"{c.remote_dir}/job-survived"
    script = c.write_file(
        "roster-busy.sh",
        "#!/bin/sh\n"
        f"while [ ! -f {shlex.quote(release)} ]; do sleep 0.1; done\n"
        f"echo survived > {shlex.quote(survived)}\n",
    )
    output = c.sbatch([
        "-N", "1", "-w", busy, "-t", "5", "-D", c.remote_dir,
        "-o", f"{c.remote_dir}/roster-busy.out", script,
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

        _write_roster(c, [])
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


def test_reconfigure_preserves_reservation_and_idle_node_then_retries(roster_cluster):
    c = roster_cluster
    reservation = "roster-maintenance"
    c.cli_as_user(
        "root", [
            "scontrol", "create-reservation", f"--name={reservation}",
            "--start-time=now", "--duration=60", f"--nodes={c.node_names[1]}",
            f"--users={c.nodes[0].user}",
        ], check=True,
    )
    keys = ["ReservationName", "Nodes", "StartTime", "EndTime", "Users"]
    before = _fields(c.scontrol("show", "reservation", reservation), keys)
    _write_roster(c, [])

    _assert_rejected(c, "reservation", c.node_names)

    assert _fields(c.scontrol("show", "reservation", reservation), keys) == before
    c.restart_agent(0)
    wait_node_state(c, c.node_names[0], ["idle"])
    c.cli_as_user("root", ["scontrol", "delete-reservation", reservation], check=True)
    c.scontrol("reconfigure")
    assert c.sinfo_nodes() == {}


def test_departed_node_stays_excluded_after_restart_until_readmitted(roster_cluster):
    c = roster_cluster
    removed = c.node_names[1]
    retained = [name for name in c.node_names if name != removed]
    c.signal_agent(1, "TERM")
    wait_node_state(c, removed, ["down"])
    c.nodes[1].exec(
        f"rm -rf {shlex.quote(c.state_dir)} && mkdir -p {shlex.quote(c.state_dir)}"
    )
    _write_roster(c, retained)
    c.scontrol("reconfigure")
    assert set(c.sinfo_nodes()) == set(retained)

    # A stale startup file must not override the roster committed through Raft.
    _write_roster(c, c.node_names)
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


def test_empty_roster_and_clearing_it_restore_dynamic_admission(roster_cluster):
    c = roster_cluster
    c.stop_agents()
    for name in c.node_names:
        wait_node_state(c, name, ["down"])
    _write_roster(c, [])
    c.scontrol("reconfigure")
    assert c.sinfo_nodes() == {}
    c.restart_controller(expected_nodes=[])
    assert c.sinfo_nodes() == {}
    c.restart_agent(1)
    wait_until(
        lambda: "admission.node_names" in c.spurd_log(1),
        timeout_s=30, interval_s=1, desc="empty roster rejects registration",
    )
    assert c.sinfo_nodes() == {}

    _write_roster(c, None)
    c.scontrol("reconfigure")
    c.restart_agent(0)
    c.restart_agent(1)
    for name in c.node_names[:2]:
        wait_node_state(c, name, ["idle"])


def test_omitted_roster_preserves_nodes_when_policy_is_removed(roster_cluster):
    c = roster_cluster
    _write_roster(c, None)
    c.scontrol("reconfigure")
    c.config_overrides["nodes"] = []
    c._write_config()
    c.scontrol("reconfigure")
    assert set(c.sinfo_nodes()) == set(c.node_names)
