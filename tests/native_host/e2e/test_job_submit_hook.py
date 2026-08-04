# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the job_submit validation hook.

Exercise the full user-facing path (spur sbatch -> gRPC -> submit hook ->
CLI) that in-process unit tests cannot: a rejection's message must reach the
submitting user, and a modify must persist and be queryable via scontrol.
"""

from cluster import parse_job_id


REJECT_HOOK = """\
#!/bin/bash
echo 'submission denied: policy requires an approved account' >&2
exit 1
"""

MODIFY_HOOK = """\
#!/bin/bash
echo '{"priority": 777, "comment": "policy-tagged"}'
"""

REJECT_LUA = """\
function slurm_job_submit(job_desc, submit_uid)
  slurm.log_user('submission denied by lua policy')
  return slurm.ERROR
end
"""

MODIFY_LUA = """\
function slurm_job_submit(job_desc, submit_uid)
  job_desc.priority = 555
  job_desc.comment = 'lua-tagged'
  return slurm.SUCCESS
end
"""


def _hook_config(cluster, body: str) -> dict:
    path = cluster.write_file("hooks/job_submit.sh", body)
    return {"hooks": {"job_submit": path}}


def _lua_config(cluster, body: str) -> dict:
    path = cluster.write_file("hooks/job_submit.lua", body, executable=False)
    return {"hooks": {"job_submit_lua": path}}


class TestJobSubmitHook:
    def test_reject_message_reaches_user(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_hook_config(cluster, REJECT_HOOK))

        script = cluster.write_file("job.sh", "#!/bin/bash\necho hi\n")
        out = cluster.cli_allow_fail(["sbatch", "-J", "blocked", script])

        assert "submission denied: policy requires an approved account" in out
        assert parse_job_id(out) is None

    def test_modify_persists_and_is_queryable(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_hook_config(cluster, MODIFY_HOOK))

        script = cluster.write_file("job.sh", "#!/bin/bash\necho hi\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "modme", script]))
        assert job_id is not None

        detail = cluster.scontrol("show", "job", str(job_id))
        assert "Priority=777" in detail
        assert "policy-tagged" in detail

    def test_unset_hook_is_inert(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start()

        script = cluster.write_file("job.sh", "#!/bin/bash\necho hi\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "plain", script]))
        assert job_id is not None

    def test_lua_reject_message_reaches_user(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_lua_config(cluster, REJECT_LUA))

        script = cluster.write_file("job.sh", "#!/bin/bash\necho hi\n")
        out = cluster.cli_allow_fail(["sbatch", "-J", "blocked-lua", script])

        assert "submission denied by lua policy" in out
        assert parse_job_id(out) is None

    def test_lua_modify_persists_and_is_queryable(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_lua_config(cluster, MODIFY_LUA))

        script = cluster.write_file("job.sh", "#!/bin/bash\necho hi\n")
        job_id = parse_job_id(cluster.sbatch(["-J", "lua-mod", script]))
        assert job_id is not None

        detail = cluster.scontrol("show", "job", str(job_id))
        assert "Priority=555" in detail
        assert "lua-tagged" in detail
