# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the Spur prolog/epilog hook framework.

Tests cover all hook types (prolog, epilog, prolog_slurmctld, epilog_slurmctld),
environment variable propagation, failure semantics, and execution ordering.
"""

import time

from cluster import parse_job_id, wait_job, job_state


LOGGING_PROLOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
{
    echo "HOOK=prolog"
    echo "TS=$(date +%s%N)"
    echo "SPUR_JOB_ID=$SPUR_JOB_ID"
    echo "SPUR_JOB_USER=$SPUR_JOB_USER"
    echo "SPUR_JOB_UID=$SPUR_JOB_UID"
    echo "SPUR_JOB_GID=$SPUR_JOB_GID"
    echo "SPUR_JOB_WORK_DIR=$SPUR_JOB_WORK_DIR"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SPUR_JOB_NODELIST=$SPUR_JOB_NODELIST"
    echo "SPUR_CPUS_ON_NODE=$SPUR_CPUS_ON_NODE"
    echo "SPUR_JOB_MEMORY_MB=$SPUR_JOB_MEMORY_MB"
    echo "SPUR_SCRIPT_CONTEXT=$SPUR_SCRIPT_CONTEXT"
} > "{RD}/hook-out/prolog-$SPUR_JOB_ID.log"
"""

LOGGING_EPILOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
{
    echo "HOOK=epilog"
    echo "TS=$(date +%s%N)"
    echo "SPUR_JOB_ID=$SPUR_JOB_ID"
    echo "SPUR_JOB_USER=$SPUR_JOB_USER"
    echo "SPUR_JOB_UID=$SPUR_JOB_UID"
    echo "SPUR_JOB_GID=$SPUR_JOB_GID"
    echo "SPUR_JOB_WORK_DIR=$SPUR_JOB_WORK_DIR"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SPUR_JOB_NODELIST=$SPUR_JOB_NODELIST"
    echo "SPUR_CPUS_ON_NODE=$SPUR_CPUS_ON_NODE"
    echo "SPUR_JOB_MEMORY_MB=$SPUR_JOB_MEMORY_MB"
    echo "SPUR_SCRIPT_CONTEXT=$SPUR_SCRIPT_CONTEXT"
} > "{RD}/hook-out/epilog-$SPUR_JOB_ID.log"
"""

LOGGING_PROLOG_CTLD = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
{
    echo "HOOK=prolog_slurmctld"
    echo "SPUR_JOB_ID=$SPUR_JOB_ID"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SPUR_JOB_NODELIST=$SPUR_JOB_NODELIST"
    echo "SPUR_SCRIPT_CONTEXT=$SPUR_SCRIPT_CONTEXT"
} > "{RD}/hook-out/prolog_ctld-$SPUR_JOB_ID.log"
"""

LOGGING_EPILOG_CTLD = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
{
    echo "HOOK=epilog_slurmctld"
    echo "SPUR_JOB_ID=$SPUR_JOB_ID"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SPUR_SCRIPT_CONTEXT=$SPUR_SCRIPT_CONTEXT"
} > "{RD}/hook-out/epilog_ctld-$SPUR_JOB_ID.log"
"""

FAILING_HOOK = """\
#!/bin/bash
exit 1
"""


def _parse_hook_log(content: str) -> dict[str, str]:
    """Parse a hook log file (KEY=VALUE per line) into a dict."""
    result = {}
    for line in content.strip().splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            result[key] = value
    return result


def _read_hook_log(cluster, job_id, hook_name, *, controller_only=False):
    """Read and parse a hook log file, asserting it exists."""
    path = f"{cluster.remote_dir}/hook-out/{hook_name}-{job_id}.log"
    if controller_only:
        raw = cluster.nodes[0].read_file(path)
    else:
        raw = cluster.read_output_on_any_node(path)
    assert raw.strip(), f"hook log not found: {path}"
    return _parse_hook_log(raw)


def _setup_hooks(cluster, **hook_bodies: str) -> dict:
    """Write hook scripts to all nodes and return config overrides.

    Each keyword argument maps a hook name (e.g. ``prolog``,
    ``epilog_slurmctld``) to a script body template.  ``{RD}`` in
    the body is replaced with ``cluster.remote_dir``.

    Returns a *config_overrides* dict ready to pass to
    :meth:`SpurCluster.start`.
    """
    rd = cluster.remote_dir
    hooks_config: dict[str, str] = {}
    for hook_name, body in hook_bodies.items():
        script_name = f"hooks/{hook_name}.sh"
        cluster.write_file(script_name, body.replace("{RD}", rd), all_nodes=True)
        hooks_config[hook_name] = f"{rd}/{script_name}"
    return {"hooks": hooks_config}


def _wait_node_state(cluster, target_state, timeout=15):
    """Poll sinfo until any node shows *target_state* (case-insensitive)."""
    target = target_state.lower()
    deadline = time.time() + timeout
    info = ""
    while time.time() < deadline:
        info = cluster.sinfo()
        if target in info.lower():
            return info
        time.sleep(1)
    assert False, (
        f"no node reached '{target_state}' within {timeout}s:\n{info}"
    )


class TestHookExecution:
    """Verify hooks execute in the right order with correct env vars."""

    def test_prolog_executes_before_epilog(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG, epilog=LOGGING_EPILOG))

        out_path = f"{cluster.remote_dir}/ordering.out"
        script = cluster.write_file("test.sh", "#!/bin/bash\nsleep 2\necho DONE\n")
        sb = cluster.sbatch(["-J", "ordering", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        content = cluster.read_output_on_any_node(out_path)
        assert "DONE" in content, f"job output missing:\n{content}"

        prolog = _read_hook_log(cluster, job_id, "prolog")
        epilog = _read_hook_log(cluster, job_id, "epilog")

        assert int(prolog["TS"]) < int(epilog["TS"]), (
            f"prolog must run before epilog: {prolog['TS']} vs {epilog['TS']}"
        )

    def test_prolog_receives_all_env_vars(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG))

        script = cluster.write_file("test.sh", "#!/bin/bash\necho ENV_OK\n")
        sb = cluster.sbatch(["-J", "env-prolog", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        log = _read_hook_log(cluster, job_id, "prolog")
        assert log["SPUR_JOB_ID"] == str(job_id)
        assert log["SPUR_JOB_PARTITION"] == "default"
        assert log["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmd"
        assert log.get("SPUR_JOB_USER"), "SPUR_JOB_USER must be set"
        assert log.get("SPUR_JOB_UID"), "SPUR_JOB_UID must be set"
        assert log.get("SPUR_JOB_GID"), "SPUR_JOB_GID must be set"
        assert log.get("SPUR_JOB_WORK_DIR"), "SPUR_JOB_WORK_DIR must be set"
        assert log.get("SPUR_JOB_NODELIST"), "SPUR_JOB_NODELIST must be set"
        assert log.get("SPUR_CPUS_ON_NODE"), "SPUR_CPUS_ON_NODE must be set"
        assert log.get("SPUR_JOB_MEMORY_MB"), "SPUR_JOB_MEMORY_MB must be set"

    def test_epilog_receives_all_env_vars(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, epilog=LOGGING_EPILOG))

        script = cluster.write_file("test.sh", "#!/bin/bash\necho ENV_OK\n")
        sb = cluster.sbatch(["-J", "env-epilog", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        log = _read_hook_log(cluster, job_id, "epilog")
        assert log["SPUR_JOB_ID"] == str(job_id)
        assert log["SPUR_JOB_PARTITION"] == "default"
        assert log["SPUR_SCRIPT_CONTEXT"] == "epilog_slurmd"
        assert log.get("SPUR_JOB_USER"), "SPUR_JOB_USER must be set"
        assert log.get("SPUR_JOB_UID"), "SPUR_JOB_UID must be set"
        assert log.get("SPUR_JOB_GID"), "SPUR_JOB_GID must be set"
        assert log.get("SPUR_JOB_WORK_DIR"), "SPUR_JOB_WORK_DIR must be set"
        assert log.get("SPUR_JOB_NODELIST"), "SPUR_JOB_NODELIST must be set"
        assert log.get("SPUR_CPUS_ON_NODE"), "SPUR_CPUS_ON_NODE must be set"
        assert log.get("SPUR_JOB_MEMORY_MB"), "SPUR_JOB_MEMORY_MB must be set"

    def test_prolog_nodelist_matches_allocated_node(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG))

        target = cluster.node_names[0]
        script = cluster.write_file("test.sh", "#!/bin/bash\necho OK\n")
        sb = cluster.sbatch(["-J", "nodelist-check", "-N", "1", "-w", target, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        log = _read_hook_log(cluster, job_id, "prolog")
        assert log["SPUR_JOB_NODELIST"] == target, (
            f"expected nodelist {target!r}, got {log['SPUR_JOB_NODELIST']!r}"
        )

    def test_all_hooks_env_vars_consistent(self, unstarted_cluster):
        """All four hook types receive the same job ID and correct script context."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(
            cluster,
            prolog=LOGGING_PROLOG,
            epilog=LOGGING_EPILOG,
            prolog_slurmctld=LOGGING_PROLOG_CTLD,
            epilog_slurmctld=LOGGING_EPILOG_CTLD,
        ))
        script = cluster.write_file("test.sh", "#!/bin/bash\necho CONSISTENT_OK\n")
        sb = cluster.sbatch(["-J", "consistent", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        prolog = _read_hook_log(cluster, job_id, "prolog")
        epilog = _read_hook_log(cluster, job_id, "epilog")
        prolog_ctld = _read_hook_log(cluster, job_id, "prolog_ctld", controller_only=True)
        epilog_ctld = _read_hook_log(cluster, job_id, "epilog_ctld", controller_only=True)

        jid_str = str(job_id)
        assert prolog["SPUR_JOB_ID"] == jid_str
        assert epilog["SPUR_JOB_ID"] == jid_str
        assert prolog_ctld["SPUR_JOB_ID"] == jid_str
        assert epilog_ctld["SPUR_JOB_ID"] == jid_str

        assert prolog["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmd"
        assert epilog["SPUR_SCRIPT_CONTEXT"] == "epilog_slurmd"
        assert prolog_ctld["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmctld"
        assert epilog_ctld["SPUR_SCRIPT_CONTEXT"] == "epilog_slurmctld"

    def test_multiple_jobs_get_independent_hooks(self, unstarted_cluster):
        """Each job should trigger its own prolog/epilog with the correct job ID."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG, epilog=LOGGING_EPILOG))
        script = cluster.write_file("test.sh", "#!/bin/bash\necho MULTI_OK\n")

        sb1 = cluster.sbatch(["-J", "multi-1", "-N", "1", script])
        jid1 = parse_job_id(sb1)
        assert jid1 is not None
        wait_job(cluster, jid1, timeout=60)

        sb2 = cluster.sbatch(["-J", "multi-2", "-N", "1", script])
        jid2 = parse_job_id(sb2)
        assert jid2 is not None
        wait_job(cluster, jid2, timeout=60)

        assert jid1 != jid2

        for job_id in (jid1, jid2):
            prolog = _read_hook_log(cluster, job_id, "prolog")
            epilog = _read_hook_log(cluster, job_id, "epilog")
            assert prolog["SPUR_JOB_ID"] == str(job_id), (
                f"prolog logged wrong job_id for job {job_id}: {prolog['SPUR_JOB_ID']}"
            )
            assert epilog["SPUR_JOB_ID"] == str(job_id), (
                f"epilog logged wrong job_id for job {job_id}: {epilog['SPUR_JOB_ID']}"
            )

    def test_hooks_run_for_failed_job_script(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG, epilog=LOGGING_EPILOG))
        script = cluster.write_file("test.sh", "#!/bin/bash\nexit 42\n")
        sb = cluster.sbatch(["-J", "fail-job-hooks", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state == "F", f"expected failed state, got {state}"

        prolog = _read_hook_log(cluster, job_id, "prolog")
        epilog = _read_hook_log(cluster, job_id, "epilog")
        assert prolog["SPUR_JOB_ID"] == str(job_id)
        assert epilog["SPUR_JOB_ID"] == str(job_id)


class TestHookFailure:
    """Verify failure semantics for all hook types."""

    def test_prolog_failure_fails_job_and_drains_node(self, unstarted_cluster):
        # TODO: Slurm requeues+holds the job on prolog failure. Spur currently
        # races between JobFailed (agent report_completion) and requeue
        # (scheduler all-dispatch-fail path), so we accept both F and PD.
        # Tighten to PD-only once requeue+hold semantics are implemented.
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=FAILING_HOOK))
        out_path = f"{cluster.remote_dir}/fail-prolog.out"
        script = cluster.write_file(
            "test.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n"
        )
        sb = cluster.sbatch(["-J", "fail-prolog", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        _wait_node_state(cluster, "drain")

        # With multiple nodes the scheduler may re-dispatch before the failure
        # report lands, draining nodes one by one.  Once all are drained the
        # job is either F (failure reported) or PD (stuck, no eligible nodes).
        deadline = time.time() + 30
        state = None
        while time.time() < deadline:
            sq = cluster.squeue_all()
            state = job_state(sq, job_id)
            if state in ("F", "PD", None):
                break
            time.sleep(1)
        assert state in ("F", "PD", None), (
            f"job should fail or stay pending after prolog failure, got {state}"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "SHOULD_NOT_RUN" not in content, (
            "job should not have run after prolog failure"
        )

        if state == "PD":
            cluster.scancel(str(job_id))

    def test_epilog_failure_drains_node(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, epilog=FAILING_HOOK))
        out_path = f"{cluster.remote_dir}/fail-epilog.out"
        script = cluster.write_file("test.sh", "#!/bin/bash\necho EPILOG_JOB_OK\n")
        sb = cluster.sbatch(["-J", "fail-epilog", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), (
            f"job should complete before epilog runs, got {state}"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "EPILOG_JOB_OK" in content, (
            "job should have run before epilog failure"
        )

        _wait_node_state(cluster, "drain")

    def test_prolog_slurmctld_failure_requeues_batch_job(self, unstarted_cluster):
        """PrologSlurmctld failure requeues batch jobs — they never reach the agents."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(
            cluster, prolog_slurmctld=FAILING_HOOK, prolog=LOGGING_PROLOG,
        ))
        out_path = f"{cluster.remote_dir}/ctld-fail.out"
        script = cluster.write_file("test.sh", "#!/bin/bash\necho SHOULD_NOT_RUN\n")
        sb = cluster.sbatch(["-J", "ctld-fail", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        deadline = time.time() + 30
        state = None
        while time.time() < deadline:
            sq = cluster.squeue_all()
            state = job_state(sq, job_id)
            if state == "PD":
                break
            assert state != "R", (
                f"job should not run after PrologSlurmctld failure\n{sq}"
            )
            time.sleep(1)
        assert state == "PD", (
            f"batch job should stay pending (requeued) after PrologSlurmctld "
            f"failure, got {state}"
        )

        agent_prolog = cluster.read_output_on_any_node(
            f"{cluster.remote_dir}/hook-out/prolog-{job_id}.log"
        )
        assert not agent_prolog.strip(), (
            "agent-side prolog should never run when PrologSlurmctld fails"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "SHOULD_NOT_RUN" not in content, (
            "job script should never execute when PrologSlurmctld fails"
        )

        cluster.scancel(str(job_id))

    def test_epilog_slurmctld_failure_is_nonfatal(self, unstarted_cluster):
        """EpilogSlurmctld failure is logged but does not affect job or node state."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, epilog_slurmctld=FAILING_HOOK))
        out_path = f"{cluster.remote_dir}/ctld-epilog-nonfatal.out"
        script = cluster.write_file("test.sh", "#!/bin/bash\necho NONFATAL_OK\n")
        sb = cluster.sbatch(["-J", "ctld-epilog-nf", "-N", "1", "-o", out_path, script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), (
            f"job should complete despite EpilogSlurmctld failure, got {state}"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "NONFATAL_OK" in content

        info = cluster.sinfo()
        assert "drain" not in info.lower(), (
            f"EpilogSlurmctld failure should not drain nodes:\n{info}"
        )
