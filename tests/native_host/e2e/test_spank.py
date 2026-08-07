# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for SPANK plugin support.

The plugin (fixtures/spank_test.c) is dlopen'd by spurd and resolves the
spank_* API from the agent's exported dynamic symbols, so these tests cover
the symbol export list in spurd's build script as much as the hook dispatch.

spank_cluster is provisioned but not started: the plugin and plugstack.conf
must be in place before spurd reads SPUR_PLUGSTACK at startup.
"""

import pytest

from cluster import parse_job_id, wait_job

ENV_PROBE = (
    "#!/bin/bash\n"
    'echo "SPANK_TEST_VAR=${SPANK_TEST_VAR}"\n'
    'echo "SPANK_TEST_INIT=${SPANK_TEST_INIT}"\n'
    'echo "SPANK_TEST_JOB_ID=${SPANK_TEST_JOB_ID}"\n'
    'echo "SPANK_TEST_UID=${SPANK_TEST_UID}"\n'
    'echo "SPANK_TEST_SAW_INIT=${SPANK_TEST_SAW_INIT}"\n'
    'echo "SPANK_TEST_NOCLOBBER=${SPANK_TEST_NOCLOBBER}"\n'
    "echo PROBE_DONE\n"
)


def _build_plugin(cluster) -> str:
    return cluster.compile_c_fixture(
        "spank_test.c",
        output_name="spank_test.so",
        extra_flags=["-shared", "-fPIC"],
    )


def _run_probe(cluster, name: str) -> str:
    out_path = f"{cluster.remote_dir}/{name}.out"
    script = cluster.write_file(f"{name}.sh", ENV_PROBE)
    job_id = parse_job_id(cluster.sbatch(["-J", name, "-N", "1", "-o", out_path, script]))
    assert job_id is not None, "sbatch did not return a job id"
    state = wait_job(cluster, job_id, timeout=90)
    assert state in ("CD", "GONE"), f"probe job ended in {state}"
    content = cluster.read_output_on_any_node(out_path)
    assert "PROBE_DONE" in content, f"probe job produced no output:\n{content}"
    return content


def _env(output: str) -> dict[str, str]:
    return dict(
        line.split("=", 1) for line in output.splitlines() if "=" in line
    )


@pytest.fixture
def spank_plugin(spank_cluster):
    """A started cluster with the test plugin loaded as a required entry."""
    plugin = _build_plugin(spank_cluster)
    spank_cluster.write_plugstack([f"required {plugin} var=SPANK_TEST_VAR value=hello"])
    spank_cluster.start()
    return spank_cluster


class TestPluginLoading:
    def test_plugin_is_loaded_at_agent_startup(self, spank_plugin):
        log = spank_plugin.spurd_log()
        assert "loaded SPANK plugin" in log, f"plugin was not loaded:\n{log}"
        assert "SPANK plugins loaded" in log, f"load summary missing:\n{log}"

    def test_plugin_args_reach_the_hooks(self, spank_cluster):
        """Args after the path in plugstack.conf are passed as argv."""
        plugin = _build_plugin(spank_cluster)
        trace = f"{spank_cluster.remote_dir}/spank-trace.log"
        spank_cluster.write_plugstack([f"required {plugin} trace={trace} var=X value=y"])
        spank_cluster.start()

        _run_probe(spank_cluster, "spank-args")

        recorded = spank_cluster.read_output_on_any_node(trace)
        assert "init ac=3" in recorded, (
            f"init hook did not see all three args:\n{recorded}"
        )
        assert "task_init ac=3" in recorded, (
            f"task_init hook did not see all three args:\n{recorded}"
        )


class TestEnvInjection:
    def test_setenv_reaches_the_job_environment(self, spank_plugin):
        env = _env(_run_probe(spank_plugin, "spank-env"))
        assert env["SPANK_TEST_VAR"] == "hello", (
            f"plugstack value did not reach the job: {env}"
        )
        assert env["SPANK_TEST_INIT"] == "1", (
            f"init-hook env edit was dropped before task_init: {env}"
        )

    def test_init_edits_are_visible_to_task_init(self, spank_plugin):
        """Both hooks share one handle, so task_init can read back what init
        set."""
        env = _env(_run_probe(spank_plugin, "spank-handle"))
        assert env["SPANK_TEST_SAW_INIT"] == "1", (
            f"task_init could not read the init hook's variable: {env}"
        )

    def test_setenv_without_overwrite_keeps_the_first_value(self, spank_plugin):
        env = _env(_run_probe(spank_plugin, "spank-noclobber"))
        assert env["SPANK_TEST_NOCLOBBER"] == "first", (
            f"overwrite=0 must not replace an existing value: {env}"
        )

    def test_get_item_exposes_job_context(self, spank_plugin):
        out_path = f"{spank_plugin.remote_dir}/spank-item.out"
        script = spank_plugin.write_file("spank-item.sh", ENV_PROBE)
        job_id = parse_job_id(
            spank_plugin.sbatch(
                ["-J", "spank-item", "-N", "1", "-o", out_path, script]
            )
        )
        assert job_id is not None
        wait_job(spank_plugin, job_id, timeout=90)

        env = _env(spank_plugin.read_output_on_any_node(out_path))
        assert env["SPANK_TEST_JOB_ID"] == str(job_id), (
            f"S_JOB_ID did not match the running job: {env}"
        )
        assert env["SPANK_TEST_UID"].isdigit(), f"S_JOB_UID was not set: {env}"


class TestHookLifecycle:
    def test_task_exit_hook_runs_after_the_job(self, spank_cluster):
        plugin = _build_plugin(spank_cluster)
        trace = f"{spank_cluster.remote_dir}/spank-lifecycle.log"
        spank_cluster.write_plugstack([f"required {plugin} trace={trace}"])
        spank_cluster.start()

        _run_probe(spank_cluster, "spank-lifecycle")

        recorded = spank_cluster.read_output_on_any_node(trace)
        for hook in ("init", "task_init", "task_exit"):
            assert any(line.startswith(hook) for line in recorded.splitlines()), (
                f"{hook} hook never ran:\n{recorded}"
            )


class TestFailureSemantics:
    def test_missing_required_plugin_is_logged_and_survivable(self, spank_cluster):
        """A required plugin that cannot be dlopen'd is loud but must not stop
        the agent from serving jobs."""
        spank_cluster.write_plugstack(
            [f"required {spank_cluster.remote_dir}/no-such-plugin.so"]
        )
        spank_cluster.start()

        log = spank_cluster.spurd_log()
        assert "required SPANK plugin failed to load" in log, (
            f"a missing required plugin must be logged at warn:\n{log}"
        )

        script = spank_cluster.write_file(
            "spank-survive.sh", "#!/bin/bash\necho SURVIVED\n"
        )
        out_path = f"{spank_cluster.remote_dir}/spank-survive.out"
        job_id = parse_job_id(
            spank_cluster.sbatch(["-J", "spank-survive", "-o", out_path, script])
        )
        assert job_id is not None
        assert wait_job(spank_cluster, job_id, timeout=90) in ("CD", "GONE")
        assert "SURVIVED" in spank_cluster.read_output_on_any_node(out_path)

    def test_missing_optional_plugin_is_skipped_quietly(self, spank_cluster):
        spank_cluster.write_plugstack(
            [f"optional {spank_cluster.remote_dir}/no-such-plugin.so"]
        )
        spank_cluster.start()

        log = spank_cluster.spurd_log()
        assert "optional SPANK plugin failed to load, skipping" in log, (
            f"a missing optional plugin must be logged as skipped:\n{log}"
        )
        assert "required SPANK plugin failed to load" not in log, (
            f"an optional entry must not be reported as required:\n{log}"
        )

    def test_failing_hook_is_logged_and_does_not_kill_the_job(self, spank_cluster):
        """Spur treats a non-zero hook return as advisory: it is logged, but
        the launch continues."""
        plugin = _build_plugin(spank_cluster)
        spank_cluster.write_plugstack([f"required {plugin} fail=init"])
        spank_cluster.start()

        out_path = f"{spank_cluster.remote_dir}/spank-failhook.out"
        script = spank_cluster.write_file(
            "spank-failhook.sh", "#!/bin/bash\necho RAN_ANYWAY\n"
        )
        job_id = parse_job_id(
            spank_cluster.sbatch(["-J", "spank-failhook", "-o", out_path, script])
        )
        assert job_id is not None
        assert wait_job(spank_cluster, job_id, timeout=90) in ("CD", "GONE")
        assert "RAN_ANYWAY" in spank_cluster.read_output_on_any_node(out_path)

        log = spank_cluster.spurd_log()
        assert "SPANK hook returned error" in log, (
            f"a failing hook must be logged:\n{log}"
        )

    def test_failing_init_hook_does_not_block_task_init(self, spank_cluster):
        """Hooks are dispatched independently, so an init failure must not
        silently disable env injection."""
        plugin = _build_plugin(spank_cluster)
        spank_cluster.write_plugstack(
            [f"required {plugin} fail=init var=SPANK_TEST_VAR value=still-set"]
        )
        spank_cluster.start()

        env = _env(_run_probe(spank_cluster, "spank-failinit"))
        assert env["SPANK_TEST_VAR"] == "still-set", (
            f"task_init must still run after init failed: {env}"
        )
        assert env["SPANK_TEST_INIT"] == "", (
            f"the failing init hook must not have set its variable: {env}"
        )


class TestNoPlugstack:
    def test_jobs_run_without_a_plugstack_file(self, cluster):
        """The default deployment has no plugstack.conf; SPANK must stay
        entirely out of the launch path."""
        assert "SPANK plugins loaded" not in cluster.spurd_log()

        out_path = f"{cluster.remote_dir}/no-spank.out"
        script = cluster.write_file("no-spank.sh", ENV_PROBE)
        job_id = parse_job_id(cluster.sbatch(["-J", "no-spank", "-o", out_path, script]))
        assert job_id is not None
        wait_job(cluster, job_id, timeout=90)

        env = _env(cluster.read_output_on_any_node(out_path))
        assert env["SPANK_TEST_VAR"] == "", f"unexpected SPANK env: {env}"
