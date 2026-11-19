# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E stress: many concurrent submissions through a follower stay intact.

A follower forwards every write to the leader over one long-lived HTTP/2
connection shared by all callers. This test puts that hop under load: it fires
many concurrent ``sbatch --wait`` calls at a follower, each carrying a distinct
~8.5 KB ASCII script that re-hashes its own payload and exits non-zero if a
single byte changed. Every client that runs to completion exiting 0 is the real
check that the concurrent, overlapping forwards were applied intact.

Each payload is unique, so one request's bytes leaking into another changes the
hash and fails that job; a corrupted frame instead surfaces as a decode error at
submit. As a best effort it also SIGKILLs every fourth client that has not yet
printed ``Submitted batch job`` — its submit may still be on the shared
connection — rather than killing after a fixed delay, which would only reset the
later ``--wait`` polls. On a fast local cluster the near-instant submit usually
wins that race, so the reset is opportunistic, not guaranteed.

Opt-in: the ``ha_cluster`` fixture skips unless the harness was given >= 3
nodes. Pointing it directly at the leader is a useful control, since then no
forwarding happens.
"""

import hashlib
import os

from cluster import CONTROLLER_PORT, parse_job_id

# Enough concurrent, size-matched forwards to exercise the shared connection
# without overwhelming a small test cluster; both are overridable.
JOB_COUNT = int(os.environ.get("SPUR_TEST_FORWARD_STRESS_JOBS", "40"))
PAYLOAD_BYTES = int(os.environ.get("SPUR_TEST_FORWARD_STRESS_PAYLOAD", "8500"))
# A decode failure surfaces through any of these in the sbatch output.
DECODE_MARKERS = ("failed to decode", "invalid UTF-8", "corrupted in transit", "DataLoss")


def _payload(job_index: int) -> str:
    """A unique ~PAYLOAD_BYTES ASCII block that ends in a newline, so a heredoc
    reproduces it byte-for-byte."""
    line = f"forward-stress-job-{job_index:04d}-"
    lines = []
    width = 0
    counter = 0
    while width < PAYLOAD_BYTES:
        entry = f"{line}{counter:08d}\n"
        lines.append(entry)
        width += len(entry)
        counter += 1
    return "".join(lines)


def _self_verifying_script(job_index: int) -> str:
    payload = _payload(job_index)
    expected = hashlib.sha256(payload.encode()).hexdigest()
    # The heredoc writes `payload` verbatim; the script then hashes it and exits
    # 42 on any drift, so the job's exit alone reports corruption — no need to
    # read output back across nodes.
    return (
        "#!/bin/bash\n"
        "set -u\n"
        'tmp=$(mktemp)\n'
        "cat > \"$tmp\" <<'FWDSTRESSEOF'\n"
        f"{payload}"
        "FWDSTRESSEOF\n"
        'actual=$(sha256sum "$tmp" | cut -d" " -f1)\n'
        'rm -f "$tmp"\n'
        f'if [ "$actual" != "{expected}" ]; then\n'
        f'  echo "payload corruption in job {job_index}: $actual" >&2\n'
        "  exit 42\n"
        "fi\n"
        f'echo "forward-stress ok job {job_index}"\n'
    )


def _follower_addr(cluster) -> str:
    cluster._wait_leader_elected()
    leader = cluster._current_raft_leader()
    assert leader is not None, "no stable leader to pick a follower against"
    follower = next(i for i in range(cluster.ha_controller_count) if i != leader)
    return f"http://{cluster.nodes[follower].host}:{CONTROLLER_PORT}"


class TestForwardedSubmitStress:
    def test_concurrent_forwarded_submissions_are_not_corrupted(self, ha_cluster):
        cluster = ha_cluster
        follower = _follower_addr(cluster)

        script_paths = []
        for i in range(JOB_COUNT):
            path = cluster.write_file(
                f"forward-stress-{i}.sh", _self_verifying_script(i)
            )
            script_paths.append(path)

        # One remote orchestrator: launch each `sbatch --wait` directly in the
        # background so $! is the client itself (not a result-collecting subshell
        # whose death would leave sbatch running), sending each client's output to
        # its own file. Then SIGKILL every fourth client that has not yet printed
        # "Submitted batch job" — its submit forward may still be on the shared
        # connection — instead of a fixed delay that would only catch the later
        # GetJob --wait polls. Finally wait on each and record its exit + output.
        env = " ".join(cluster._cli_env_assignments(controller_addr=follower))
        outdir = f"{cluster.remote_dir}/fwd-stress-out"
        launch = "\n".join(
            f"{env} sbatch --wait -N1 -o /dev/null {path} > {outdir}/c{i}.out 2>&1 &\n"
            f"pid{i}=$!"
            for i, path in enumerate(script_paths)
        )
        kills = "\n".join(
            f'grep -q "Submitted batch job" {outdir}/c{i}.out 2>/dev/null '
            f'|| {{ kill -9 $pid{i} 2>/dev/null && echo "KILLED idx={i}"; }}'
            for i in range(JOB_COUNT)
            if i % 4 == 0
        )
        collect = "\n".join(
            f"wait $pid{i}; rc=$?; "
            f"echo \"RESULT idx={i} rc=$rc out=[$(tr '\\n' ' ' < {outdir}/c{i}.out)]\""
            for i in range(JOB_COUNT)
        )
        orchestrator = f"mkdir -p {outdir}\n{launch}\n{kills}\n{collect}\n"

        # A generous ceiling: the jobs are instant, but they queue on a small
        # cluster and each killed client's job may still run to completion.
        out = cluster.nodes[0].exec(f"timeout 600 bash -c {_sh_quote(orchestrator)}")

        killed = {int(line.split("idx=")[1]) for line in out.splitlines() if line.startswith("KILLED")}
        results = {}
        for line in out.splitlines():
            if not line.startswith("RESULT"):
                continue
            idx = int(line.split("idx=")[1].split()[0])
            rc = int(line.split("rc=")[1].split()[0])
            body = line.split("out=[", 1)[1].rsplit("]", 1)[0]
            results[idx] = (rc, body)

        # No submission, killed or not, may reveal a decode failure on the hop.
        for idx, (_, body) in results.items():
            lowered = body.lower()
            assert not any(m.lower() in lowered for m in DECODE_MARKERS), (
                f"job {idx} saw a decode/integrity failure on the forwarded hop:\n{body}"
            )

        # Every client that was not killed must have completed cleanly; a payload
        # the hop corrupted would have exited 42 (or failed to submit at all).
        survivors = [idx for idx in range(JOB_COUNT) if idx not in killed]
        assert survivors, "expected at least some clients to run to completion"
        for idx in survivors:
            assert idx in results, f"client {idx} produced no result line:\n{out}"
            rc, body = results[idx]
            assert rc == 0, f"job {idx} did not complete cleanly (rc={rc}):\n{body}"

        # The killed clients' jobs may still have been created; let any that are
        # queued or running drain so the fixture tears down cleanly.
        for jid in _submitted_job_ids(results):
            cluster.cli_allow_fail(["scancel", str(jid)])


def _submitted_job_ids(results: dict[int, tuple[int, str]]) -> list[int]:
    ids = []
    for _, body in results.values():
        jid = parse_job_id(body)
        if jid is not None:
            ids.append(jid)
    return ids


def _sh_quote(script: str) -> str:
    return "'" + script.replace("'", "'\"'\"'") + "'"
