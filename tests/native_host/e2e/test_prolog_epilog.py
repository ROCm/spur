# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the Spur prolog/epilog hook framework.

Tests cover all hook types (prolog, epilog, prolog_slurmctld, epilog_slurmctld),
environment variable propagation, failure semantics, and execution ordering.
"""

import re
import time

from cluster import parse_job_id, wait_job, job_state


LOGGING_PROLOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
{
    echo "HOOK=prolog"
    echo "TS=$(date +%s%N)"
    echo "SPUR_JOB_ID=$SPUR_JOB_ID"
    echo "SLURM_JOB_ID=$SLURM_JOB_ID"
    echo "SPUR_JOB_USER=$SPUR_JOB_USER"
    echo "SPUR_JOB_UID=$SPUR_JOB_UID"
    echo "SPUR_JOB_GID=$SPUR_JOB_GID"
    echo "SPUR_JOB_WORK_DIR=$SPUR_JOB_WORK_DIR"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SLURM_JOB_PARTITION=$SLURM_JOB_PARTITION"
    echo "SPUR_JOB_NODELIST=$SPUR_JOB_NODELIST"
    echo "SLURM_JOB_NODELIST=$SLURM_JOB_NODELIST"
    echo "SPUR_CPUS_ON_NODE=$SPUR_CPUS_ON_NODE"
    echo "SLURM_CPUS_ON_NODE=$SLURM_CPUS_ON_NODE"
    echo "SPUR_JOB_MEMORY_MB=$SPUR_JOB_MEMORY_MB"
    echo "SPUR_JOB_GPUS=$SPUR_JOB_GPUS"
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
    echo "SLURM_JOB_ID=$SLURM_JOB_ID"
    echo "SPUR_JOB_USER=$SPUR_JOB_USER"
    echo "SPUR_JOB_UID=$SPUR_JOB_UID"
    echo "SPUR_JOB_GID=$SPUR_JOB_GID"
    echo "SPUR_JOB_WORK_DIR=$SPUR_JOB_WORK_DIR"
    echo "SPUR_JOB_PARTITION=$SPUR_JOB_PARTITION"
    echo "SLURM_JOB_PARTITION=$SLURM_JOB_PARTITION"
    echo "SPUR_JOB_NODELIST=$SPUR_JOB_NODELIST"
    echo "SLURM_JOB_NODELIST=$SLURM_JOB_NODELIST"
    echo "SPUR_CPUS_ON_NODE=$SPUR_CPUS_ON_NODE"
    echo "SLURM_CPUS_ON_NODE=$SLURM_CPUS_ON_NODE"
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

COUNTING_PROLOG = """\
#!/bin/bash
mkdir -p "{RD}/hook-out"
echo "$SPUR_JOB_ID $(date +%s%N)" >> "{RD}/hook-out/prolog-count-$SPUR_JOB_ID.log"
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
    states = {}
    while time.time() < deadline:
        states = cluster.sinfo_nodes()
        if any(target in s.lower() for s in states.values()):
            return states
        time.sleep(1)
    assert False, (
        f"no node reached '{target_state}' within {timeout}s:\n{states}"
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
        assert log["SLURM_JOB_ID"] == str(job_id), "SLURM twin must match"
        assert log["SPUR_JOB_PARTITION"] == "default"
        assert log["SLURM_JOB_PARTITION"] == "default", "SLURM twin must match"
        assert log["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmd"
        assert log.get("SPUR_JOB_USER"), "SPUR_JOB_USER must be set"
        assert log.get("SPUR_JOB_UID"), "SPUR_JOB_UID must be set"
        assert log.get("SPUR_JOB_GID"), "SPUR_JOB_GID must be set"
        assert log.get("SPUR_JOB_WORK_DIR"), "SPUR_JOB_WORK_DIR must be set"
        assert log.get("SPUR_JOB_NODELIST"), "SPUR_JOB_NODELIST must be set"
        assert log.get("SLURM_JOB_NODELIST"), "SLURM_JOB_NODELIST must be set"
        assert log.get("SPUR_CPUS_ON_NODE"), "SPUR_CPUS_ON_NODE must be set"
        assert log.get("SLURM_CPUS_ON_NODE"), "SLURM_CPUS_ON_NODE must be set"
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
        assert log["SLURM_JOB_ID"] == str(job_id), "SLURM twin must match"
        assert log["SPUR_JOB_PARTITION"] == "default"
        assert log["SLURM_JOB_PARTITION"] == "default", "SLURM twin must match"
        assert log["SPUR_SCRIPT_CONTEXT"] == "epilog_slurmd"
        assert log.get("SPUR_JOB_USER"), "SPUR_JOB_USER must be set"
        assert log.get("SPUR_JOB_UID"), "SPUR_JOB_UID must be set"
        assert log.get("SPUR_JOB_GID"), "SPUR_JOB_GID must be set"
        assert log.get("SPUR_JOB_WORK_DIR"), "SPUR_JOB_WORK_DIR must be set"
        assert log.get("SPUR_JOB_NODELIST"), "SPUR_JOB_NODELIST must be set"
        assert log.get("SLURM_JOB_NODELIST"), "SLURM_JOB_NODELIST must be set"
        assert log.get("SPUR_CPUS_ON_NODE"), "SPUR_CPUS_ON_NODE must be set"
        assert log.get("SLURM_CPUS_ON_NODE"), "SLURM_CPUS_ON_NODE must be set"
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

    def test_missing_work_dir_does_not_drain_node(self, unstarted_cluster):
        """A job whose WorkDir is absent on the node must run, not drain it."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG))

        # Unique per run so a leftover dir can't let the prolog chdir succeed.
        missing = f"{cluster.remote_dir}/nonexistent-wd/deep/missing"
        out_path = f"{cluster.remote_dir}/missing-wd.out"
        script = cluster.write_file("test.sh", "#!/bin/bash\necho WD_OK\n")
        sb = cluster.sbatch(
            ["-J", "missing-wd", "-N", "1", "-D", missing, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), (
            f"job with a missing WorkDir should complete, got {state}"
        )

        states = cluster.sinfo_nodes()
        assert not any(s.startswith("drain") for s in states.values()), (
            f"a missing WorkDir must not drain the node:\n{states}"
        )

        content = cluster.read_output_on_any_node(out_path)
        assert "WD_OK" in content, f"job output missing:\n{content}"

        # Prolog ran from the fallback CWD but still reports the submitted dir.
        prolog = _read_hook_log(cluster, job_id, "prolog")
        assert prolog["SPUR_JOB_ID"] == str(job_id)
        assert prolog["SPUR_JOB_WORK_DIR"] == missing, (
            f"prolog should still report the submitted WorkDir, got "
            f"{prolog.get('SPUR_JOB_WORK_DIR')!r}"
        )

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

        states = cluster.sinfo_nodes()
        assert not any(s.startswith("drain") for s in states.values()), (
            f"EpilogSlurmctld failure should not drain nodes:\n{states}"
        )


class TestSrunStandaloneProlog:
    """Standalone srun on native hosts takes the controller's
    ``srun_step_dispatch`` path (RegisterJobAllocation, not LaunchJob), which
    used to skip the node Prolog entirely. Regression-locks the fix (the
    prolog call added to ``register_job_allocation`` in agent_server.rs)."""

    def test_srun_standalone_triggers_node_prolog_and_epilog(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG, epilog=LOGGING_EPILOG))

        code, out = cluster.srun_with_exit(
            ["-J", "srun-prolog", "bash", "-c", "echo SPUR_JOB_ID=$SPUR_JOB_ID"]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        m = re.search(r"SPUR_JOB_ID=(\d+)", out)
        assert m, f"job output missing SPUR_JOB_ID:\n{out}"
        job_id = int(m.group(1))

        prolog = _read_hook_log(cluster, job_id, "prolog")
        assert prolog["SPUR_JOB_ID"] == str(job_id)
        assert prolog["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmd"

        # The extern stepd runs the epilog on teardown; give it a moment after
        # srun (which blocks until the step completes) has already returned.
        deadline = time.time() + 15
        epilog = None
        while time.time() < deadline:
            raw = cluster.read_output_on_any_node(
                f"{cluster.remote_dir}/hook-out/epilog-{job_id}.log"
            )
            if raw.strip():
                epilog = _parse_hook_log(raw)
                break
            time.sleep(1)
        assert epilog is not None, "node Epilog did not run for standalone srun"
        assert epilog["SPUR_JOB_ID"] == str(job_id)
        assert int(prolog["TS"]) < int(epilog["TS"]), (
            f"prolog must run before epilog: {prolog['TS']} vs {epilog['TS']}"
        )

    def test_srun_standalone_gpu_allocation_reaches_prolog(self, gpu_cluster):
        """The reporter's exact repro (``srun --gpus=N ...``) on a real GPU
        host: the node Prolog must run and see the allocated GPU device IDs."""
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        cluster.stop()
        cluster.start(_setup_hooks(cluster, prolog=LOGGING_PROLOG))

        code, out = cluster.srun_with_exit(
            ["-J", "srun-gpu-prolog", "--gpus=1", "bash", "-c", "echo SPUR_JOB_ID=$SPUR_JOB_ID"]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        m = re.search(r"SPUR_JOB_ID=(\d+)", out)
        assert m, f"job output missing SPUR_JOB_ID:\n{out}"
        job_id = int(m.group(1))

        prolog = _read_hook_log(cluster, job_id, "prolog")
        assert prolog["SPUR_SCRIPT_CONTEXT"] == "prolog_slurmd"
        assert prolog.get("SPUR_JOB_GPUS"), (
            f"node Prolog must see the allocated GPU device IDs for a --gpus=1 "
            f"srun job, got {prolog.get('SPUR_JOB_GPUS')!r}"
        )

    def test_sbatch_prolog_runs_exactly_once(self, unstarted_cluster):
        """Locks the invariant that the batch path runs the node Prolog exactly
        once. sbatch dispatches via launch_job and never reaches the srun
        register_job_allocation prolog site, so the two sites are disjoint by
        construction — this cannot itself catch a double-run from that split.
        It guards against a future change that runs the batch prolog twice, or
        that wires the srun-allocation site into the batch path."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=COUNTING_PROLOG))

        script = cluster.write_file("test.sh", "#!/bin/bash\necho DONE\n")
        sb = cluster.sbatch(["-J", "count-prolog", "-N", "1", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"

        raw = cluster.read_output_on_any_node(
            f"{cluster.remote_dir}/hook-out/prolog-count-{job_id}.log"
        )
        lines = [ln for ln in raw.strip().splitlines() if ln.strip()]
        assert len(lines) == 1, (
            f"sbatch node Prolog must run exactly once, ran {len(lines)} times:\n{raw}"
        )

    def test_srun_standalone_prolog_failure_drains_node(self, unstarted_cluster):
        """A failing prolog on the srun path must drain the node, matching the
        LaunchJob path. Otherwise the node is only cooled and srun keeps landing
        on a node whose prolog fails every time (the prolog gates node access)."""
        cluster = unstarted_cluster
        cluster.start(_setup_hooks(cluster, prolog=FAILING_HOOK))

        target = cluster.node_names[0]
        code, out = cluster.srun_with_exit(
            ["-J", "srun-prolog-fail", "-w", target, "bash", "-c", "echo SHOULD_NOT_RUN"]
        )
        assert code != 0, f"srun must fail when its prolog fails, got exit 0:\n{out}"
        assert "SHOULD_NOT_RUN" not in out, (
            f"the step must not run after a prolog failure:\n{out}"
        )

        states = _wait_node_state(cluster, "drain")
        assert any("drain" in s.lower() for s in states.values()), (
            f"the node whose srun prolog failed must drain, not just cool:\n{states}"
        )


class TestSrunClientHooks:
    """srun_prolog / srun_epilog (``srun --prolog`` / ``--epilog``) run
    client-side on the node where srun is invoked, as the invoking user — a
    separate axis from the node prolog/epilog that run on the compute node as
    root. A failing SrunProlog blocks step dispatch; the node hooks are a
    distinct mechanism and unaffected."""

    def test_srun_prolog_runs_client_side_before_the_step(self, cluster):
        marker = f"{cluster.remote_dir}/srun-prolog-ran"
        prolog = cluster.write_file(
            "srun-prolog.sh", f'#!/bin/bash\necho "ctx=$SPUR_SCRIPT_CONTEXT" > {marker}\n'
        )
        code, out = cluster.srun_with_exit(
            ["--prolog", prolog, "bash", "-c", "echo STEP_RAN"]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"
        assert "STEP_RAN" in out, f"the step must run after a passing SrunProlog:\n{out}"
        recorded = cluster.nodes[0].read_file(marker).strip()
        assert "prolog_srun" in recorded, (
            f"SrunProlog must run client-side with context prolog_srun, got {recorded!r}"
        )

    def test_srun_prolog_failure_blocks_the_step(self, cluster):
        prolog = cluster.write_file("srun-prolog-fail.sh", "#!/bin/bash\nexit 1\n")
        code, out = cluster.srun_with_exit(
            ["--prolog", prolog, "bash", "-c", "echo SHOULD_NOT_RUN"]
        )
        assert code != 0, f"a failing SrunProlog must fail srun, got exit 0:\n{out}"
        assert "SHOULD_NOT_RUN" not in out, (
            f"the step must not dispatch when SrunProlog fails:\n{out}"
        )

    def test_srun_epilog_runs_client_side_after_the_step(self, cluster):
        marker = f"{cluster.remote_dir}/srun-epilog-ran"
        epilog = cluster.write_file(
            "srun-epilog.sh", f'#!/bin/bash\necho "ctx=$SPUR_SCRIPT_CONTEXT" > {marker}\n'
        )
        code, out = cluster.srun_with_exit(
            ["--epilog", epilog, "bash", "-c", "echo STEP_RAN"]
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"
        assert "STEP_RAN" in out, out
        recorded = cluster.nodes[0].read_file(marker).strip()
        assert "epilog_srun" in recorded, (
            f"SrunEpilog must run client-side with context epilog_srun, got {recorded!r}"
        )
