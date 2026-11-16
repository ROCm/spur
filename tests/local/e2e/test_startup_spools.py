# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Startup spool cleanup through real daemons, their RPCs, and private /proc."""

import json
import os
from pathlib import Path

import pytest


def _submit_held(cluster):
    return int(cluster.cli("submit", "--parsable", "--hold", "--export", "NONE", "--wrap", "true").strip())


@pytest.mark.parametrize("terminal_state", ["CANCELLED", "COMPLETED"])
def test_startup_removes_terminal_and_absent_spools_but_keeps_pending(local_cluster, terminal_state):
    cluster = local_cluster
    if terminal_state == "CANCELLED":
        terminal = _submit_held(cluster)
        cluster.cli("cancel", terminal)
    else:
        cluster.cgroups(False)
        cluster.start_agent()
        release = cluster.root / "completed-release"
        os.mkfifo(release)
        output = cluster.root / "completed.out"
        arguments = ["submit", "--parsable", "--export", "NONE", "-o", output,
                     "--wrap", f"echo READY; read token < {release}; echo DONE"]
        terminal = int(cluster.cli(*arguments).strip())
        cluster.wait_file(output, "READY")
        session, = (cluster.root / "agent/runtime").glob(f"{terminal}.*.*/descriptor.json")
        supervisor_pid = json.loads(session.read_text())["pid"]
        channel = os.open(release, os.O_WRONLY | os.O_NONBLOCK)
        try:
            os.write(channel, b"release\n")
        finally:
            os.close(channel)
        cluster.wait_file(output, "DONE")
        cluster.call("wait_pid_exit", pid=supervisor_pid)
        cluster.stop_agent()
        cluster.cgroups(True)
    assert f"JobState={terminal_state}" in cluster.cli("control", "show", "job", terminal)
    pending = _submit_held(cluster)
    stale = cluster.make_spool(terminal)
    absent = cluster.make_spool(900001)
    held = cluster.make_spool(pending)
    log = cluster.start_agent()
    assert not stale.exists(), log
    assert not absent.exists(), log
    assert (held / "sentinel").read_text() == f"job {pending}\n"
    assert "deleted=2" in log and "retained=1" in log, log
    assert len(cluster.call("queries")) == 1


def test_startup_deletion_does_not_follow_symlinks_or_noncanonical_names(local_cluster):
    cluster = local_cluster
    target = cluster.root / "symlink-target"
    target.mkdir()
    (target / "sentinel").write_text("outside spool\n")
    orphan = cluster.make_spool(900001)
    (orphan / "nested-link").symlink_to(target, target_is_directory=True)
    linked_job = cluster.spool / "job900002"
    linked_job.symlink_to(target, target_is_directory=True)
    names = ["job0", "job01", "job-1", "job4294967296", "jobs", "containers"]
    for name in names:
        (cluster.spool / name).mkdir()
    log = cluster.start_agent()
    assert not orphan.exists(), log
    assert linked_job.is_symlink()
    assert (target / "sentinel").read_text() == "outside spool\n"
    assert all((cluster.spool / name).is_dir() for name in names)


@pytest.mark.parametrize("mode", ["unavailable", "legacy"])
def test_failed_or_legacy_query_retains_spools_until_next_startup(local_cluster, mode):
    cluster = local_cluster
    orphan = cluster.make_spool(900001)
    cluster.call("proxy_mode", mode=mode)
    log = cluster.start_agent()
    assert (orphan / "sentinel").read_text() == "job 900001\n", log
    assert len(cluster.call("queries")) == 1
    cluster.call("proxy_mode", mode="pass")
    cluster.cli("info")
    assert orphan.exists()
    log = cluster.restart_agent()
    assert not orphan.exists(), log
    assert len(cluster.call("queries")) == 2


def test_legacy_terminal_response_cannot_delete_a_pending_job_spool(local_cluster):
    cluster = local_cluster
    pending = _submit_held(cluster)
    assert pending == 1, "frozen legacy response describes job 1"
    held = cluster.make_spool(pending)
    absent = cluster.make_spool(900001)
    cluster.call("proxy_mode", mode="legacy-terminal")
    log = cluster.start_agent()
    assert (held / "sentinel").read_text() == "job 1\n", log
    assert (absent / "sentinel").read_text() == "job 900001\n", log
    assert "JobState=PENDING" in cluster.cli("control", "show", "job", pending)
    assert len(cluster.call("queries")) == 1
    cluster.call("proxy_mode", mode="pass")
    log = cluster.restart_agent()
    assert held.exists(), log
    assert not absent.exists(), log
    assert len(cluster.call("queries")) == 2


def test_deletion_error_is_retried_only_after_restart(local_cluster):
    cluster = local_cluster
    orphan = cluster.make_spool(900001)
    cluster.call("proxy_mode", mode="blocked")
    cluster.start_agent(wait=False)
    cluster.call("wait_query", count=1)
    cluster.spool.chmod(0o777)
    cluster.call("proxy_mode", mode="pass")
    log = cluster.wait_agent()
    assert orphan.exists(), log
    assert "errors=1" in log, log
    cluster.spool.chmod(0o755)
    cluster.cli("info")
    assert orphan.exists()
    log = cluster.restart_agent()
    assert not orphan.exists(), log
    assert len(cluster.call("queries")) == 2


@pytest.mark.parametrize("fault", ["disabled-cgroups", "writable-root", "symlink-root", "unknown-runtime"])
def test_uncertain_or_unsafe_startup_inventory_retains_spools(local_cluster, fault):
    cluster = local_cluster
    orphan = cluster.make_spool(900001)
    if fault == "disabled-cgroups":
        cluster.cgroups(False)
    elif fault == "writable-root":
        cluster.spool.chmod(0o777)
    elif fault == "symlink-root":
        saved = cluster.root / "saved-spool"
        cluster.spool.rename(saved)
        cluster.spool.symlink_to(saved, target_is_directory=True)
    else:
        (cluster.root / "agent/runtime/unidentified").mkdir(parents=True)
        (cluster.root / "agent/runtime").chmod(0o700)
    log = cluster.start_agent()
    assert (orphan / "sentinel").read_text() == "job 900001\n", log
    assert "deleted=0" in log, log
    expected_queries = 1 if fault == "unknown-runtime" else 0
    assert len(cluster.call("queries")) == expected_queries


@pytest.mark.parametrize("holder", ["environment", "cwd", "hardlink-fd"])
def test_real_process_owners_protect_only_their_spool_until_restart(local_cluster, holder):
    cluster = local_cluster
    held = cluster.make_spool(900001)
    absent = cluster.make_spool(900002)
    environment = {"SLURM_JOB_ID": "900001"} if holder == "environment" else {}
    setup = ""
    if holder == "cwd":
        setup = f"import os; os.chdir({str(held)!r}); "
    elif holder == "hardlink-fd":
        alias = cluster.root / "output-alias"
        os.link(held / "sentinel", alias)
        setup = f"output = open({str(alias)!r}); "
    process = cluster.call("start_holder", name="holder", environment=environment,
                           script=setup + "import signal; print('READY', flush=True); signal.pause()")
    cluster.wait_file(process["log"], "READY")
    log = cluster.start_agent()
    assert held.exists(), log
    assert not absent.exists(), log
    assert cluster.call("pid_exists", pid=process["pid"])
    assert cluster.call("stop", name="holder") == -15
    assert held.exists()
    log = cluster.restart_agent()
    assert not held.exists(), log


def test_spools_created_after_startup_wait_for_the_next_daemon_start(local_cluster):
    cluster = local_cluster
    cluster.start_agent()
    orphan = cluster.make_spool(900001)
    cluster.call("wait_heartbeat", count=2)
    assert orphan.exists()
    log = Path(cluster.agent["log"]).read_text()
    assert log.count("startup job spool cleanup finished") == 1
    assert cluster.call("queries") == []
    log = cluster.restart_agent()
    assert not orphan.exists(), log


def test_running_batch_and_step_keep_spool_and_output_across_restart(local_cluster):
    cluster = local_cluster
    cluster.cgroups(False)
    cluster.start_agent()
    release = cluster.root / "batch-release"
    step_release = cluster.root / "step-release"
    os.mkfifo(release)
    os.mkfifo(step_release)
    output = cluster.root / "batch.out"
    script = cluster.root / "batch.sh"
    script.write_text(f"#!/bin/bash\necho START\nread token < {release}\necho DONE\n")
    job_id = int(cluster.cli("submit", "--parsable", "--export", "NONE", "-o", output, script).strip())
    cluster.wait_file(output, "START")
    spool = cluster.spool / f"job{job_id}"
    assert spool.is_dir()
    step_output = cluster.root / "step.out"
    step = cluster.call("start_cli", name="step", environment={"SPUR_JOB_ID": str(job_id)},
                        arguments=["run", "--overlap", "--output", str(step_output), "bash", "-c",
                        f"echo STEP_START; read token < {step_release}; echo STEP_DONE"])
    step_spool_output = spool / "step0.out"
    cluster.wait_file(step_spool_output, "STEP_START")
    sessions = list((cluster.root / "agent/runtime").glob(f"{job_id}.*.*/descriptor.json"))
    assert len(sessions) == 2, sessions
    descriptors = [json.loads(path.read_text()) for path in sessions]
    assert any(0 <= descriptor["step_id"] < 100 for descriptor in descriptors), descriptors
    pids = [descriptor["pid"] for descriptor in descriptors]
    assert all(cluster.call("pid_exists", pid=pid) for pid in pids)
    unrelated = cluster.make_spool(900001)
    cluster.cgroups(True)
    log = cluster.restart_agent()
    assert spool.is_dir(), log
    assert not unrelated.exists(), log
    assert all(cluster.call("pid_exists", pid=pid) for pid in pids)
    assert "JobState=RUNNING" in cluster.cli("control", "show", "job", job_id)
    channel = os.open(step_release, os.O_WRONLY | os.O_NONBLOCK)
    try:
        os.write(channel, b"release\n")
    finally:
        os.close(channel)
    cluster.wait_file(step_output, "STEP_DONE")
    assert cluster.call("wait_process", name=step["name"]) == 0
    channel = os.open(release, os.O_WRONLY | os.O_NONBLOCK)
    try:
        os.write(channel, b"release\n")
    finally:
        os.close(channel)
    content = cluster.wait_file(output, "DONE")
    assert content.splitlines() == ["START", "DONE"]
    assert step_output.read_text().splitlines() == ["STEP_START", "STEP_DONE"]


def test_deep_spool_cannot_abort_daemon_startup(local_cluster):
    cluster = local_cluster
    limits = cluster.call("deep_tree_limits")
    if not limits["available"]:
        pytest.skip(f"deep-spool regression requires RLIMIT_NOFILE hard >= 65536, got {limits['hard']}")
    orphan = cluster.make_spool(900001)
    cluster.call("make_deep_tree", path=str(orphan), depth=6000)
    try:
        cluster.start_agent()
    finally:
        cluster.call("remove_deep_tree", path=str(orphan))


@pytest.mark.parametrize("fallback_mode", [0o755, 0o777])
def test_completion_removes_spool_when_openat2_is_unavailable(local_cluster, fallback_mode):
    cluster = local_cluster
    fault = cluster.call("openat2_fault_available")
    if not fault["available"]:
        pytest.skip(fault["reason"])
    cluster.cgroups(False)
    log = cluster.start_agent(deny_openat2=True)
    assert "fixture: openat2 returns ENOSYS" in log
    release = cluster.root / "completion-release"
    os.mkfifo(release)
    output = cluster.root / "completion.out"
    job_id = int(cluster.cli("submit", "--parsable", "--export", "NONE", "-o", output,
                             "--wrap", f"echo READY; read token < {release}; echo DONE").strip())
    cluster.wait_file(output, "READY")
    spool = cluster.spool / f"job{job_id}"
    assert spool.is_dir()
    session, = (cluster.root / "agent/runtime").glob(f"{job_id}.*.*/descriptor.json")
    supervisor_pid = json.loads(session.read_text())["pid"]
    target = cluster.root / "completion-symlink-target"
    target.mkdir()
    (target / "sentinel").write_text("outside spool\n")
    (spool / "nested-link").symlink_to(target, target_is_directory=True)
    fallback_root = cluster.root / "tmp/spur"
    fallback_root.chmod(fallback_mode)
    fallback = fallback_root / f"job{job_id}"
    fallback.mkdir()
    (fallback / "step0.out").write_text("output from an earlier agent\n")
    (fallback / "nested-link").symlink_to(target, target_is_directory=True)
    channel = os.open(release, os.O_WRONLY | os.O_NONBLOCK)
    try:
        os.write(channel, b"release\n")
    finally:
        os.close(channel)
    cluster.wait_file(output, "DONE")
    cluster.call("wait_pid_exit", pid=supervisor_pid)
    log = cluster.wait_file(cluster.agent["log"], "reported completion to controller")
    assert not spool.exists(), log
    assert not fallback.exists(), log
    assert (target / "sentinel").read_text() == "outside spool\n"
    assert "JobState=COMPLETED" in cluster.cli("control", "show", "job", job_id)
