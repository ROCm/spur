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


def _hook_config(cluster, body: str) -> dict:
    path = cluster.write_file("hooks/job_submit.sh", body)
    return {"hooks": {"job_submit": path}}


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
