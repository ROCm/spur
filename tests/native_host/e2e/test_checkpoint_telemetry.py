# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Checkpoint telemetry E2E tests.

A job reports checkpoint events over a per-step Unix socket
(`$SPUR_CHECKPOINT_SOCK`) using a few lines of the Python standard library --
no Spur client in the job. spurstepd attributes and counts the events;
spurd ships changed records to spurctld on a 30s batch (or immediately at
step end); spurctld finalizes a run's checkpoint outcome exactly
`FINALIZE_GRACE` (60s) after it ends -- there is no early finalize, even once
every allocated node's final record has arrived -- and exposes it on
`/metrics/jobs` (cluster aggregates) and, behind `metrics.high_cardinality`,
per job on `/metrics/jobs-users-accts`. A metric wait tied to a run ending
(`spur_job_runs_ended_total`, `spur_job_run_checkpoints_completed_sum`)
budgets `RUN_END_METRIC_TIMEOUT` (150s: the 60s grace plus up to a 30s report
tick plus slack); the per-job invalid-event counter and the live per-job
gauges are not grace-gated and use shorter waits.

Cases:
  - `TestCheckpointBasicLifecycle` (H1, native-only half): two completed
    checkpoints and a restore on one batch step, counted once the run ends.
  - `TestCheckpointAcrossBatchAndStep`: a batch script and the one srun step
    it runs each report a completed checkpoint -- both land in the same run's
    outcome (completed-sum += 2, one run end), not just the last one to end.
  - `TestCheckpointSocketSecurity`: the socket and its parent directory match
    the design's attribution/hardening contract.
  - `TestCheckpointStepSocketIsPerStep`: an srun step's own
    `SPUR_CHECKPOINT_SOCK` names its own step's socket, never the batch
    step's `STEP_BATCH` sentinel, and differs from the batch step's own.
  - `TestCheckpointCancelledInProgress`: a run whose last event is an
    unfinished begin, then cancelled, is counted `checkpoint="in_progress"`.
  - `TestCheckpointSpoofingAndDrops` (H4): malformed/oversize/mismatched
    events are dropped and counted without affecting the job; a foreign
    process on another job's socket is caught by the cgroup check.
  - `TestCheckpointPerJobGauges`: per-job gauges on a still-live run, behind
    `metrics.high_cardinality`.
  - `TestCheckpointPerJobFailedCounterAcrossRequeue`: the per-job invalid-event
    counter is cumulative across run attempts of the same job -- one invalid
    event per attempt, same reason, sums to 2 after an admin requeue.
  - `TestCheckpointContainerStep` (H2): a Spur-managed container step reports
    the same way a native step does, and `SPUR_CHECKPOINT_SOCK` is present
    and reachable inside the container.
  - `TestCheckpointMultiNode` (H3): both nodes of a 2-node run report the same
    training steps; the run is still counted exactly once.
  - `TestCheckpointPreemptionRequeue`: a batch script (not an srun step --
    see the grace-delivery note below) that reports a completed checkpoint
    before its job is preempt-requeued is recorded `reason="preempted"`,
    `checkpoint="completed"`, not `"none"` or silently dropped.

A container rootfs whose image plants a regular file where the checkpoint
socket's parent directory would be does NOT exercise a mount failure here:
`container.rs` mounts a fresh tmpfs on `/run` (`mount_filesystems`) before
`mount_checkpoint_socket` ever runs, so anything the image itself put under
`/run` is already hidden by the time the bind mount is attempted -- an e2e
case built that way would pass without touching the code it claims to cover.
That path (a bind-mount failure leaving `SPUR_CHECKPOINT_SOCK` unset without
aborting the job) is covered by the unit test
`checkpoint_socket_mount_failure_is_absorbed_and_reports_unavailable` in
`crates/spurd/src/container.rs` instead; `TestCheckpointContainerStep` here
covers the positive path (the socket present and reachable).

Cluster isolation: every fixture in this file uses its own `cluster_name`
(`CLUSTER_NAME` below, "e2e-ckpt") via a config override, instead of the
harness-wide default ("e2e-test", `cluster.py`'s `_default_config`) that
every other e2e suite shares. Mixing a rootless and a rootful spurd under the
same cluster_name leaves `/run/spur/<cluster>` owned by whichever uid ran
last; `checkpoint_dir::harden()` then refuses it for a differently-privileged
spurd and silently disables checkpoint telemetry for that node, which once
surfaced as a `TestCheckpointBasicLifecycle` metric-wait timeout with no
other symptom. Every fixture also removes `/run/spur/<cluster_name>` (the
whole directory, not just its `checkpoint` subdirectory) on each node before
deploying, and guards against `/run/spur` itself being left group- or
world-writable (which `harden()` also refuses, regardless of cluster_name
isolation) -- the harness itself never creates `/run/spur` (confirmed: no
other e2e file references it), so a loose `/run/spur` is an OS/CI-image
issue, not something cluster_name separation can fix; the guard fails fast
with a clear message rather than the confusing downstream metric timeout
described above.

Every `_wait_for_increase` call (and the handful of metric waits that poll a
value directly) accepts the cluster and, where known, the job id, so a
timeout's message includes each node's spurd log lines mentioning
"checkpoint" and the job's own output (which should print
`SPUR_CHECKPOINT_SOCK`) -- diagnosing a CI failure like the one above without
needing to re-run it.

spurd hardens `/run/spur/<cluster>/checkpoint` against its own effective
uid, so every fixture clears that directory up front: it is global to the
node, not scoped under the harness's remote dir. Every fixture runs spurd as
root, the standard deployment, which also keeps the container case
independent of unprivileged user namespaces.

Markers: none of these need `gpu` (native CPU batch jobs and, for one case,
a Spur-managed container whose image carries `nc` instead of python3);
`TestCheckpointMultiNode` requires 2
nodes via a local fixture (skips otherwise, same convention as
`multi_node_cluster`).

Wire format used by the embedded emitter: `{"v":1,"kind":"checkpoint-begin"|
"checkpoint-end"|"restore","step":N,["status":"ok"|"failed",]"job_id":"<id>"}`
followed by `\\n`. `step_id` is optional and, when a test case includes it,
is cross-checked and a mismatch is reported the same way as a wrong
`job_id` (`job_mismatch`) -- it is otherwise omitted, because a plain batch
script's real step id is the `STEP_BATCH` sentinel, not a value the job's
own `$SPUR_STEP_ID` can be relied on to carry correctly in a one-off test
message.

Grace delivery to srun steps under a rootful spurd is a known separate
issue: `TestCheckpointPreemptionRequeue` deliberately checkpoints from the
victim's batch script, not an srun step, to stay clear of it.

H5 (spurstepd stopped/hung, SIGSTOP) needs direct process control the
harness does not expose here and H8 (8 concurrent 8-rank jobs, connection
rate) is a scale/perf case sized for a dedicated benchmark, not this file;
H10 (mixed spurd/spurstepd versions) needs two binary versions side by
side, which this harness's single `SPUR_TEST_BINARIES_DIR` cannot stage.
None are implemented below. H9 (heartbeat payload unchanged) is not
observable from this harness -- it would require diffing wire bytes on
the heartbeat RPC -- and is covered by unit tests instead
(`crates/spurd/src/reporter.rs`).
"""

import os
import re
import time
from pathlib import Path

import pytest

from cluster import (
    SpurCluster,
    ensure_bins,
    job_state,
    make_remote_dir,
    parse_job_id,
    wait_job,
    wait_job_state,
    wait_until,
)

_REPO_ROOT = Path(__file__).resolve().parents[3]

# This suite's own cluster_name, isolated from the harness-wide default
# ("e2e-test") every other e2e suite shares -- see the module docstring.
CLUSTER_NAME = "e2e-ckpt"
RUN_ROOT = "/run/spur"
CLUSTER_DIR = f"{RUN_ROOT}/{CLUSTER_NAME}"
METRICS_PORT = 6822

# FINALIZE_GRACE (60s, crates/spurctld/src/checkpoint_store.rs) plus a report
# tick (<=30s) plus slack, for a metric gated by a run actually ending.
RUN_END_METRIC_TIMEOUT = 150

# spur_core::step::STEP_BATCH -- a batch step's checkpoint socket always ends
# `.<this>.sock`; a numbered srun step's never does.
STEP_BATCH_SENTINEL = 4294967291

# A single stdlib call embedded in every job script below: connects to
# $SPUR_CHECKPOINT_SOCK (or whatever socket path is passed as $1's sibling
# override), sends one line, waits <=50ms for the 1-byte reply, and never
# raises -- the job never knows or cares whether Spur received it.
EMIT_FN = r'''
emit() {
  if ! command -v python3 >/dev/null 2>&1; then
    # Minimal container images carry no python3; nc -U speaks the same protocol.
    printf '%s\n' "$1" | nc -U -w1 -q1 "$SPUR_CHECKPOINT_SOCK" >/dev/null 2>&1 || true
    return 0
  fi
  python3 -c "
import os, socket, sys
sock_path = os.environ.get('SPUR_CHECKPOINT_SOCK')
if not sock_path:
    sys.exit(0)
s = None
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(0.05)
    s.connect(sock_path)
    s.sendall((sys.argv[1] + '\n').encode())
    s.recv(1)
except Exception:
    pass
finally:
    if s is not None:
        try:
            s.close()
        except Exception:
            pass
" "$1"
}
'''


def _emit_line(kind: str, step: int, status: str | None = None,
               job_id: str = "$SPUR_JOB_ID", step_id: str | None = None) -> str:
    """One `emit "..."` bash line. *job_id*/*step_id* may be a literal value
    (a deliberately wrong id) or a `$VAR` reference expanded by the shell."""
    obj = f'{{\\"v\\":1,\\"kind\\":\\"{kind}\\",\\"step\\":{step}'
    if status is not None:
        obj += f',\\"status\\":\\"{status}\\"'
    obj += f',\\"job_id\\":\\"{job_id}\\"'
    if step_id is not None:
        obj += f',\\"step_id\\":\\"{step_id}\\"'
    obj += "}"
    return f'emit "{obj}"'


def _emit_invalid_kind(step: int, job_id: str = "$SPUR_JOB_ID") -> str:
    """One `emit "..."` line with a well-formed but unrecognized `kind`,
    dropped and counted as `unknown_kind` (`_emit_line` only knows the three
    valid v1 kinds)."""
    return (
        f'emit "{{\\"v\\":1,\\"kind\\":\\"checkpoint-start\\",\\"step\\":{step},'
        f'\\"job_id\\":\\"{job_id}\\"}}"'
    )


def _binaries_dir() -> str:
    return os.environ.get("SPUR_TEST_BINARIES_DIR", str(_REPO_ROOT / "target" / "release"))


def _clean_run_dir(nodes) -> None:
    """Remove this suite's cluster-local checkpoint state before spurd
    re-hardens it, so ownership (see module docstring) is deterministic, and
    guard against `/run/spur` itself being left group- or world-writable by
    something outside this harness."""
    for node in nodes:
        mode = node.exec_allow_fail(f"stat -c '%a' {RUN_ROOT} 2>/dev/null").strip()
        if mode:
            assert not (int(mode, 8) & 0o022), (
                f"{RUN_ROOT} on {node.host} is mode {mode} (group- or world-writable); "
                "spurd's checkpoint directory hardening refuses it regardless of "
                "cluster_name isolation"
            )
        node.exec_allow_fail(f"sudo rm -rf '{CLUSTER_DIR}' 2>/dev/null || true")


def _start_checkpoint_cluster(c: SpurCluster, config_overrides: dict) -> None:
    """Shared provision+preflight+start sequence for a rootful checkpoint
    cluster. *config_overrides* must already include `cluster_name`."""
    c.provision()
    c.root_agent_preflight()
    c.start(config_overrides=config_overrides, agent_as_root=True)


@pytest.fixture
def checkpoint_cluster(ssh_nodes, remote_bin_dir):
    """Single-node rootful cluster for checkpoint telemetry tests."""
    if len(ssh_nodes) < 1:
        pytest.skip("checkpoint telemetry tests require at least one node")
    _clean_run_dir(ssh_nodes[:1])
    c = SpurCluster(ssh_nodes[:1], make_remote_dir(), remote_bin_dir)
    try:
        _start_checkpoint_cluster(c, {"cluster_name": CLUSTER_NAME})
        yield c
    finally:
        c.teardown()


@pytest.fixture
def checkpoint_cluster_high_cardinality(ssh_nodes, remote_bin_dir):
    """Single-node rootful cluster with `metrics.high_cardinality` on, for the
    per-job gauges and invalid-event counter on `/metrics/jobs-users-accts`."""
    if len(ssh_nodes) < 1:
        pytest.skip("checkpoint telemetry tests require at least one node")
    _clean_run_dir(ssh_nodes[:1])
    c = SpurCluster(ssh_nodes[:1], make_remote_dir(), remote_bin_dir)
    try:
        _start_checkpoint_cluster(
            c, {"cluster_name": CLUSTER_NAME, "metrics": {"high_cardinality": True}}
        )
        yield c
    finally:
        c.teardown()


@pytest.fixture
def checkpoint_multi_node_cluster(ssh_nodes, remote_bin_dir):
    """Two-node rootful cluster for multi-node checkpoint finalization."""
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"multi-node checkpoint telemetry tests require >= 2 nodes "
            f"(got {len(ssh_nodes)})"
        )
    _clean_run_dir(ssh_nodes[:2])
    c = SpurCluster(ssh_nodes[:2], make_remote_dir(), remote_bin_dir)
    try:
        _start_checkpoint_cluster(c, {"cluster_name": CLUSTER_NAME})
        yield c
    finally:
        c.teardown()


@pytest.fixture
def checkpoint_container_cluster(ssh_nodes, remote_bin_dir, tmp_path):
    """Single-node Spur-managed container cluster with the checkpoint feature
    active. spurd runs as root so the test does not depend on unprivileged
    user namespaces, which some hosts restrict (AppArmor)."""
    if len(ssh_nodes) < 1:
        pytest.skip("checkpoint telemetry tests require at least one node")
    _clean_run_dir(ssh_nodes)
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    try:
        c.deploy(agent_as_root=True, config_overrides={"cluster_name": CLUSTER_NAME})
        c.container_preflight()
        c.container_image = _build_image_with_nc(c, tmp_path)
        yield c
    finally:
        c.teardown()


# Config for TestCheckpointPreemptionRequeue's victim/hunter QOS pair.
_PREEMPT_VICTIM_QOS = "ckpt-grace-victim"
_PREEMPT_HUNTER_QOS = "ckpt-grace-hunter"
_PREEMPT_GRACE_SECS = 20
_PREEMPT_KILL_WAIT_SECS = 5
_PREEMPT_QOS_WARMUP_SECS = 15


@pytest.fixture
def checkpoint_preempt_cluster(ssh_nodes, remote_bin_dir):
    """Single-node rootful cluster with Postgres-backed accounting and a
    requeue-mode partition, for the preempt-requeue checkpoint case. Mirrors
    `test_preemption_grace.py`'s `accounting_cluster` fixture (same
    `enable_accounting()`/deploy sequence), but rootful (this suite's
    convention) and under this suite's own cluster_name isolation. Skips
    when node 0 lacks Docker (no accounting) or sudo (no rootful agent)."""
    if len(ssh_nodes) < 1:
        pytest.skip("checkpoint telemetry tests require at least one node")
    try:
        ensure_bins(ssh_nodes[:1], _binaries_dir(), remote_bin_dir, with_accounting=True)
    except FileNotFoundError as error:
        pytest.skip(f"accounting binaries unavailable: {error}")
    _clean_run_dir(ssh_nodes[:1])
    c = SpurCluster(ssh_nodes[:1], make_remote_dir(), remote_bin_dir)
    try:
        try:
            c.enable_accounting()
        except RuntimeError as error:
            pytest.skip(str(error))
        c.provision()
        c.root_agent_preflight()
        c.start(
            config_overrides={
                "cluster_name": CLUSTER_NAME,
                "auth": {"allow_root_jobs": True},
                "partitions": [{
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                    "preempt_mode": "requeue",
                }],
                "scheduler": {
                    "preempt_type": "qos_priority",
                    "kill_wait_secs": _PREEMPT_KILL_WAIT_SECS,
                },
            },
            agent_as_root=True,
        )
        yield c
    finally:
        c.teardown()


def _build_image_with_nc(cluster: SpurCluster, tmp_path) -> str:
    """The harness image plus nc and its libraries, so the container step can
    reach the checkpoint socket without python3."""
    rootfs = tmp_path / "rootfs"
    local_img = tmp_path / "ckpt-container.sqsh"
    remote_path = f"{cluster.remote_dir}/ckpt-container.sqsh"
    cluster.build_test_rootfs(
        rootfs,
        extra=f"""nc=$(readlink -f "$(command -v nc)")
cp "$nc" "$R/usr/bin/nc"
ldd "$nc" | awk '/=>/ {{print $3}}' | while read -r lib; do
  [ -f "$lib" ] && mkdir -p "$R$(dirname "$lib")" && cp -n "$lib" "$R$lib"
done
mksquashfs "$R" '{local_img}' -noappend -quiet >/dev/null 2>&1""",
    )
    for node in cluster.nodes:
        node.upload(str(local_img), remote_path)
    return remote_path


def _scrape(cluster: SpurCluster, path: str) -> str:
    return cluster.nodes[0].exec(f"curl -sf http://127.0.0.1:{METRICS_PORT}{path}")


def _scrape_jobs(cluster: SpurCluster) -> str:
    return _scrape(cluster, "/metrics/jobs")


def _scrape_jobs_users_accts(cluster: SpurCluster) -> str:
    return _scrape(cluster, "/metrics/jobs-users-accts")


def _metric_value(body: str, line_prefix: str) -> float:
    """*line_prefix* is the metric name plus its full, exact `{...}` label
    set (e.g. `spur_job_runs_ended_total{reason="completed",checkpoint="completed"}`).
    Returns 0.0 when the series has never been observed (a family with no
    matching labels has no line at all, not a zero line)."""
    pattern = re.escape(line_prefix) + r"\s+(\S+)"
    for line in body.splitlines():
        m = re.match(pattern, line)
        if m:
            return float(m.group(1))
    return 0.0


def _job_gauge(body: str, metric: str, job_id: int) -> float | None:
    """Value of a per-job gauge whose label set starts with `job_id="<id>"`
    (the struct field order `JobLabel { job_id, user, account }` in
    `spur-metrics/src/export/checkpoint.rs`), regardless of user/account.
    None when the job has no such series."""
    pattern = rf'^{re.escape(metric)}\{{job_id="{job_id}"[^}}]*\}}\s+(\S+)'
    for line in body.splitlines():
        m = re.match(pattern, line)
        if m:
            return float(m.group(1))
    return None


def _job_failed_reason_total(body: str, job_id: int, reason: str) -> float:
    """Value of `spur_job_checkpoints_failed_total` for *job_id* and
    *reason*, regardless of user/account/job_name (the labels in between,
    per `JobFailedLabel`'s field order). 0.0 when that (job, reason) pair has
    never been observed."""
    pattern = rf'^spur_job_checkpoints_failed_total\{{job_id="{job_id}"[^}}]*reason="{reason}"\}}\s+(\S+)'
    for line in body.splitlines():
        m = re.match(pattern, line)
        if m:
            return float(m.group(1))
    return 0.0


def _spurd_checkpoint_log_lines(cluster: SpurCluster) -> str:
    """Each node's spurd log, filtered to lines mentioning "checkpoint" --
    attached to a metric-wait timeout so a CI failure shows whether the event
    was even ingested, without needing to re-run the test."""
    parts = []
    for i in range(len(cluster.nodes)):
        try:
            log = cluster.spurd_log(i)
        except Exception as error:
            parts.append(f"--- node {i} spurd log unavailable: {error} ---")
            continue
        matched = "\n".join(
            line for line in log.splitlines() if "checkpoint" in line.lower()
        )
        parts.append(f"--- node {i} spurd log (checkpoint lines) ---\n{matched or '(none)'}")
    return "\n".join(parts)


def _job_output(cluster: SpurCluster, job_id: int) -> str:
    """Best-effort job stdout via `scontrol show job`'s `StdOut=` path --
    shows whether `SPUR_CHECKPOINT_SOCK` was ever printed inside the job, for
    a metric-wait timeout's diagnostics."""
    try:
        show = cluster.scontrol("show", "job", str(job_id))
    except Exception as error:
        return f"<scontrol show job failed: {error}>"
    match = re.search(r"StdOut=(\S+)", show)
    if not match:
        return "<no StdOut= field in scontrol show job>"
    try:
        content = cluster.read_output_on_any_node(match.group(1))
    except Exception as error:
        return f"<job output unavailable: {error}>"
    return content or "<empty or unreadable>"


def _augmented_timeout(error: TimeoutError, cluster: SpurCluster,
                       job_id: int | None = None) -> TimeoutError:
    """Wrap a metric-wait `TimeoutError` with each node's checkpoint-related
    spurd log lines and, when *job_id* is known, that job's output."""
    diagnostics = [_spurd_checkpoint_log_lines(cluster)]
    if job_id is not None:
        diagnostics.append(f"--- job {job_id} output ---\n{_job_output(cluster, job_id)}")
    return TimeoutError(f"{error}\n\n" + "\n\n".join(diagnostics))


def _wait_for_increase(scrape_and_read, before: float, min_delta: float,
                       timeout: int = 90, desc: str = "metric increase",
                       cluster: SpurCluster | None = None,
                       job_id: int | None = None):
    """Poll *scrape_and_read* (no args, returns the current value) until it is
    at least *before + min_delta*. A transient curl failure (controller mid-restart,
    momentary connection reset) is tolerated and retried.

    When *cluster* is given, a timeout's message is augmented with
    diagnostics (see `_augmented_timeout`); *job_id*, when also given, adds
    that job's own output."""
    def check():
        after = scrape_and_read()
        return after if after - before >= min_delta else None

    try:
        return wait_until(check, timeout_s=timeout, interval_s=3, desc=desc,
                          tolerate=(RuntimeError,))
    except TimeoutError as error:
        if cluster is None:
            raise
        raise _augmented_timeout(error, cluster, job_id) from error


class TestCheckpointBasicLifecycle:
    """H1 (cut down to native-only; the container half is in
    TestCheckpointContainerStep): a native batch job reports two completed
    checkpoints and a restore, and the run is counted once it ends."""

    def test_begin_end_restore_counted_on_completion(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        before = _scrape_jobs(cluster)
        before_runs = _metric_value(
            before,
            'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
        )
        before_sum = _metric_value(
            before, 'spur_job_run_checkpoints_completed_sum{reason="completed"}'
        )

        script = cluster.write_file(
            "checkpoint-basic.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_line("checkpoint-begin", 100) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 100, status="ok") + "\n" +
            _emit_line("checkpoint-begin", 200) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 200, status="ok") + "\n" +
            _emit_line("restore", 50) + "\n"
            "exit 0\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "ckpt-basic", "-N", "1", script]))
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=60) == "CD", cluster.debug_job(job_id)

        _wait_for_increase(
            lambda: _metric_value(
                _scrape_jobs(cluster),
                'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
            ),
            before_runs, 1, timeout=RUN_END_METRIC_TIMEOUT,
            desc="spur_job_runs_ended_total{reason=completed,checkpoint=completed} += 1",
            cluster=cluster, job_id=job_id,
        )
        after_sum = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_run_checkpoints_completed_sum{reason="completed"}',
        )
        assert after_sum - before_sum == 2, (
            f"expected the checkpoints-completed histogram _sum to grow by 2 "
            f"(one per completed step), got {before_sum} -> {after_sum}"
        )


class TestCheckpointAcrossBatchAndStep:
    """Comment 7: a batch job runs a short srun step that reports one
    checkpoint and exits, then the batch script itself reports another --
    both are distinct Spur steps (the numbered srun step and the batch
    step's own STEP_BATCH), so the run's outcome counts both
    (completed-sum += 2), not just whichever step ended last."""

    def test_step_and_batch_checkpoints_both_counted(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        before = _scrape_jobs(cluster)
        before_runs = _metric_value(
            before,
            'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
        )
        before_sum = _metric_value(
            before, 'spur_job_run_checkpoints_completed_sum{reason="completed"}'
        )

        step_script = cluster.write_file(
            "checkpoint-step-and-batch-step.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_line("checkpoint-begin", 1) + "\n" +
            _emit_line("checkpoint-end", 1, status="ok") + "\n",
        )
        batch_script = cluster.write_file(
            "checkpoint-step-and-batch.sh",
            "#!/bin/bash\n" + EMIT_FN +
            f"srun -N1 {step_script}\n" +
            _emit_line("checkpoint-begin", 2) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 2, status="ok") + "\n"
            "exit 0\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "ckpt-step-batch", "-N", "1", batch_script]))
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=90) == "CD", cluster.debug_job(job_id)

        _wait_for_increase(
            lambda: _metric_value(
                _scrape_jobs(cluster),
                'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
            ),
            before_runs, 1, timeout=RUN_END_METRIC_TIMEOUT,
            desc="spur_job_runs_ended_total{reason=completed,checkpoint=completed} += 1 "
                 "(step + batch)",
            cluster=cluster, job_id=job_id,
        )
        after_sum = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_run_checkpoints_completed_sum{reason="completed"}',
        )
        assert after_sum - before_sum == 2, (
            f"expected the checkpoints-completed histogram _sum to grow by 2 "
            f"(one from the srun step, one from the batch script), got "
            f"{before_sum} -> {after_sum}"
        )


class TestCheckpointSocketSecurity:
    """The socket and its parent directory match the design's
    attribution/hardening contract."""

    def test_socket_mode_and_directory_ownership(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        probe_out = f"{cluster.remote_dir}/socket-probe.out"
        script = cluster.write_file(
            "checkpoint-probe.sh",
            "#!/bin/bash\n"
            f'out={probe_out}\n'
            'echo "SOCK=$SPUR_CHECKPOINT_SOCK" > "$out"\n'
            'echo "SOCK_STAT=$(stat -c "%a %U" "$SPUR_CHECKPOINT_SOCK" 2>&1)" >> "$out"\n'
            'dir=$(dirname "$SPUR_CHECKPOINT_SOCK")\n'
            'echo "DIR=$dir" >> "$out"\n'
            'echo "DIR_STAT=$(stat -c "%a %U" "$dir" 2>&1)" >> "$out"\n'
            "sleep 2\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-sock-probe", "-N", "1", script])
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=60) == "CD", cluster.debug_job(job_id)

        content = cluster.read_output_on_any_node(probe_out)
        assert "SOCK=" in content and content.split("SOCK=", 1)[1].split("\n", 1)[0].strip(), (
            f"SPUR_CHECKPOINT_SOCK must be set inside the job:\n{content}"
        )
        sock_stat = content.split("SOCK_STAT=", 1)[1].split("\n", 1)[0].strip()
        assert sock_stat.startswith("600 "), (
            f"checkpoint socket must be mode 0600, got {sock_stat!r}:\n{content}"
        )
        sock_owner = sock_stat.split(" ", 1)[1]
        assert sock_owner == (cluster.nodes[0].user or "root"), (
            f"checkpoint socket must be owned by the job user, got {sock_owner!r}:\n{content}"
        )
        dir_stat = content.split("DIR_STAT=", 1)[1].split("\n", 1)[0].strip()
        assert dir_stat == "711 root", (
            f"checkpoint socket's parent dir must be root-owned 0711, got {dir_stat!r}:\n"
            f"{content}"
        )


class TestCheckpointStepSocketIsPerStep:
    """Comment 9: inside a batch job, an srun step's own
    SPUR_CHECKPOINT_SOCK names its own step's socket (ending
    `.<step_id>.sock` with its own numbered step id), never the batch step's
    STEP_BATCH sentinel, and differs from the batch step's own socket."""

    def test_srun_step_reports_its_own_socket(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        batch_sock_file = f"{cluster.remote_dir}/ckpt-sock-batch.out"
        step_sock_file = f"{cluster.remote_dir}/ckpt-sock-step.out"
        step_script = cluster.write_file(
            "checkpoint-step-sock-step.sh",
            "#!/bin/bash\n"
            f'echo "$SPUR_CHECKPOINT_SOCK" > {step_sock_file}\n',
        )
        batch_script = cluster.write_file(
            "checkpoint-step-sock-batch.sh",
            "#!/bin/bash\n"
            f'echo "$SPUR_CHECKPOINT_SOCK" > {batch_sock_file}\n'
            f"srun -N1 {step_script}\n"
            "exit 0\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "ckpt-step-sock", "-N", "1", batch_script]))
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=60) == "CD", cluster.debug_job(job_id)

        batch_sock = cluster.read_output_on_any_node(batch_sock_file).strip()
        step_sock = cluster.read_output_on_any_node(step_sock_file).strip()
        assert batch_sock, "batch step must have SPUR_CHECKPOINT_SOCK set"
        assert step_sock, "srun step must have its own SPUR_CHECKPOINT_SOCK set"
        assert step_sock != batch_sock, (
            f"srun step's socket must differ from the batch step's: "
            f"{step_sock!r} == {batch_sock!r}"
        )
        assert batch_sock.endswith(f".{STEP_BATCH_SENTINEL}.sock"), (
            f"batch step's own socket must use the STEP_BATCH sentinel: {batch_sock!r}"
        )
        match = re.search(r"\.(\d+)\.sock$", step_sock)
        assert match, f"srun step socket must end with .<step_id>.sock, got {step_sock!r}"
        assert int(match.group(1)) != STEP_BATCH_SENTINEL, (
            f"srun step's socket must use its own step id, not the batch sentinel "
            f"{STEP_BATCH_SENTINEL}: {step_sock!r}"
        )


class TestCheckpointCancelledInProgress:
    """A run whose last event is an unfinished begin, then cancelled, is
    counted as `checkpoint="in_progress"` rather than `completed` or `none`."""

    def test_cancel_after_begin_counts_in_progress(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        before = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_runs_ended_total{reason="cancelled",checkpoint="in_progress"}',
        )

        script = cluster.write_file(
            "checkpoint-cancel.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_line("checkpoint-begin", 300) + "\n"
            "sleep 120\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-cancel", "-N", "1", "--time=5", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")
        time.sleep(2)  # let the begin event land before cancelling
        cluster.scancel(job_id)
        assert wait_job(cluster, job_id, timeout=60) in ("CA", "GONE"), cluster.debug_job(job_id)

        _wait_for_increase(
            lambda: _metric_value(
                _scrape_jobs(cluster),
                'spur_job_runs_ended_total{reason="cancelled",checkpoint="in_progress"}',
            ),
            before, 1, timeout=RUN_END_METRIC_TIMEOUT,
            desc="spur_job_runs_ended_total{reason=cancelled,checkpoint=in_progress} += 1",
            cluster=cluster, job_id=job_id,
        )


class TestCheckpointSpoofingAndDrops:
    """H4: malformed/oversize/mismatched events are dropped and counted, and
    never affect the job; a foreign process on another job's socket is caught
    by the cgroup check, not the (matching) uid."""

    def test_malformed_oversize_and_job_mismatch_are_dropped(self, checkpoint_cluster):
        cluster = checkpoint_cluster
        body = _scrape_jobs(cluster)
        before = {
            reason: _metric_value(
                body, f'spur_job_run_checkpoints_failed_total{{reason="{reason}"}}'
            )
            for reason in ("job_mismatch", "oversize", "malformed_json")
        }

        oversize_line = "A" * 600  # over the 512-byte line cap
        malformed_line = "{not valid json"
        script = cluster.write_file(
            "checkpoint-drops.sh",
            "#!/bin/bash\n" + EMIT_FN +
            # Right schema, wrong job_id -> job_mismatch.
            _emit_line("checkpoint-begin", 400, job_id="999999999") + "\n"
            # Right job_id, wrong step_id -> also job_mismatch (cross-checked
            # only when step_id is present).
            + _emit_line("checkpoint-begin", 401, step_id="999999999") + "\n"
            f'emit "{oversize_line}"\n'
            f'emit "{malformed_line}"\n'
            "exit 0\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-J", "ckpt-drops", "-N", "1", script]))
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=60) == "CD", cluster.debug_job(job_id)
        assert job_state(cluster.squeue_all(), job_id) != "F"

        def scrape(reason):
            return _metric_value(
                _scrape_jobs(cluster),
                f'spur_job_run_checkpoints_failed_total{{reason="{reason}"}}',
            )

        _wait_for_increase(lambda: scrape("job_mismatch"), before["job_mismatch"], 2,
                           desc="job_mismatch invalid-events += 2 (wrong job_id + wrong step_id)",
                           cluster=cluster, job_id=job_id)
        _wait_for_increase(lambda: scrape("oversize"), before["oversize"], 1,
                           desc="oversize invalid-events += 1", cluster=cluster, job_id=job_id)
        _wait_for_increase(lambda: scrape("malformed_json"), before["malformed_json"], 1,
                           desc="malformed_json invalid-events += 1",
                           cluster=cluster, job_id=job_id)

    def test_foreign_process_on_another_jobs_socket_is_not_job_process(
        self, checkpoint_cluster
    ):
        cluster = checkpoint_cluster
        cputot = re.search(r"CPUTot=(\d+)", cluster.scontrol_show_node(cluster.node_names[0]))
        if cputot is None or int(cputot.group(1)) < 2:
            pytest.skip("node has fewer than 2 CPUs; cannot co-locate two jobs")

        before = _metric_value(
            _scrape_jobs(cluster),
            'spur_checkpoint_events_dropped_total{reason="not_job_process"}',
        )

        sock_file = f"{cluster.remote_dir}/ckpt-victim-sock"
        victim_script = cluster.write_file(
            "checkpoint-victim.sh",
            "#!/bin/bash\n"
            f'echo "$SPUR_CHECKPOINT_SOCK" > {sock_file}\n'
            "sleep 20\n",
        )
        victim_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-victim", "-N", "1", "-c", "1", victim_script])
        )
        assert victim_id is not None
        wait_job_state(cluster, victim_id, "R")

        def victim_sock_ready():
            return cluster.read_output_on_any_node(sock_file).strip() or None

        victim_sock = wait_until(victim_sock_ready, timeout_s=30, interval_s=1,
                                 desc="victim job's socket path file")

        attacker_script = cluster.write_file(
            "checkpoint-attacker.sh",
            "#!/bin/bash\n" + EMIT_FN +
            f'SPUR_CHECKPOINT_SOCK="{victim_sock}" ' +
            _emit_line("checkpoint-begin", 1) + "\n"
            "exit 0\n",
        )
        attacker_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-attacker", "-N", "1", "-c", "1", attacker_script])
        )
        assert attacker_id is not None
        assert wait_job(cluster, attacker_id, timeout=60) == "CD", (
            cluster.debug_job(attacker_id)
        )

        try:
            _wait_for_increase(
                lambda: _metric_value(
                    _scrape_jobs(cluster),
                    'spur_checkpoint_events_dropped_total{reason="not_job_process"}',
                ),
                before, 1,
                desc="not_job_process drops += 1",
                cluster=cluster, job_id=attacker_id,
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])


class TestCheckpointPerJobGauges:
    """Per-job gauges appear on `/metrics/jobs-users-accts` while a run is
    still live (not yet finalized), behind `metrics.high_cardinality`."""

    def test_live_job_gauges(self, checkpoint_cluster_high_cardinality):
        cluster = checkpoint_cluster_high_cardinality
        script = cluster.write_file(
            "checkpoint-live.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_line("checkpoint-begin", 10) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 10, status="ok") + "\n"
            "sleep 60\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-live", "-N", "1", "--time=5", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R")

        try:
            def completed_gauge():
                return _job_gauge(
                    _scrape_jobs_users_accts(cluster),
                    "spur_job_checkpoints_completed", job_id,
                )

            try:
                value = wait_until(completed_gauge, timeout_s=60, interval_s=3,
                                   desc=f"spur_job_checkpoints_completed gauge for job {job_id}",
                                   tolerate=(RuntimeError,))
            except TimeoutError as error:
                raise _augmented_timeout(error, cluster, job_id) from error
            assert value == 1, f"expected 1 completed checkpoint for job {job_id}, got {value}"

            last_step = _job_gauge(
                _scrape_jobs_users_accts(cluster),
                "spur_job_checkpoint_last_step", job_id,
            )
            assert last_step == 10, f"expected last_step=10 for job {job_id}, got {last_step}"
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestCheckpointPerJobFailedCounterAcrossRequeue:
    """Comment 8: a job's per-job invalid-event counter is cumulative across
    run attempts -- one invalid event per attempt, same reason, sums to 2
    after `scontrol requeue`. Needs `metrics.high_cardinality` for the
    per-job series."""

    def test_invalid_event_count_survives_a_requeue(self, checkpoint_cluster_high_cardinality):
        cluster = checkpoint_cluster_high_cardinality
        script = cluster.write_file(
            "checkpoint-requeue-invalid.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_invalid_kind(1) + "\n"
            "sleep 60\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(["-J", "ckpt-requeue-invalid", "-N", "1", "--time=5", script])
        )
        assert job_id is not None
        wait_job_state(cluster, job_id, "R", timeout=60)

        try:
            def failed_total():
                return _job_failed_reason_total(
                    _scrape_jobs_users_accts(cluster), job_id, "unknown_kind",
                )

            try:
                wait_until(
                    lambda: failed_total() if failed_total() >= 1 else None,
                    timeout_s=60, interval_s=3,
                    desc=f"job {job_id}'s first unknown_kind invalid event",
                    tolerate=(RuntimeError,),
                )
            except TimeoutError as error:
                raise _augmented_timeout(error, cluster, job_id) from error

            cluster.scontrol("requeue", str(job_id))
            wait_job_state(cluster, job_id, "R", timeout=90)

            try:
                value = wait_until(
                    lambda: failed_total() if failed_total() >= 2 else None,
                    timeout_s=60, interval_s=3,
                    desc=f"spur_job_checkpoints_failed_total(unknown_kind) for job "
                         f"{job_id} to reach 2",
                    tolerate=(RuntimeError,),
                )
            except TimeoutError as error:
                raise _augmented_timeout(error, cluster, job_id) from error
            assert value == 2, (
                f"expected the cumulative unknown_kind count to be 2 across both run "
                f"attempts for job {job_id}, got {value}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestCheckpointContainerStep:
    """H2: a Spur-managed container step reports the same way a native step
    does, and the run is counted once it ends.

    The socket itself (not its parent directory) is bind-mounted into the
    container at the same path, by `spurd/src/stepd.rs` right after the
    listener starts (see the `launch_spec.container` block there) -- so
    `$SPUR_CHECKPOINT_SOCK` resolves to a real path inside the container,
    even though `/run/spur/.../checkpoint` itself is not mounted. See the
    module docstring for why the mount-failure path is unit-tested only.
    """

    def test_container_step_reports_checkpoint(self, checkpoint_container_cluster):
        cluster = checkpoint_container_cluster
        img = cluster.container_image
        before = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
        )
        script = cluster.write_file(
            "checkpoint-container.sh",
            "#!/bin/bash\n" + EMIT_FN +
            'test -S "$SPUR_CHECKPOINT_SOCK" && echo SOCKET_REACHABLE\n' +
            _emit_line("checkpoint-begin", 1) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 1, status="ok") + "\n"
            "exit 0\n",
        )
        job_id = parse_job_id(
            cluster.sbatch(
                ["-J", "ckpt-container", "-N", "1", f"--container-image={img}", script]
            )
        )
        assert job_id is not None
        state = wait_job(cluster, job_id, timeout=60)
        assert state == "CD", cluster.debug_job(job_id)

        _wait_for_increase(
            lambda: _metric_value(
                _scrape_jobs(cluster),
                'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
            ),
            before, 1, timeout=RUN_END_METRIC_TIMEOUT,
            desc="spur_job_runs_ended_total{reason=completed,checkpoint=completed} += 1 "
                 "(container step)",
            cluster=cluster, job_id=job_id,
        )


class TestCheckpointMultiNode:
    """H3: both nodes of a 2-node run report the same training steps; the
    run is still counted exactly once (not once per node)."""

    def test_two_node_run_counted_once(self, checkpoint_multi_node_cluster):
        cluster = checkpoint_multi_node_cluster
        before_runs = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
        )
        before_sum = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_run_checkpoints_completed_sum{reason="completed"}',
        )

        # A standalone 2-node, 2-task step: both ranks report the same two
        # training steps, so a doubled count would show up as _sum += 4.
        emit_body = (
            EMIT_FN +
            _emit_line("checkpoint-begin", 100) + "\n"
            + _emit_line("checkpoint-end", 100, status="ok") + "\n"
            + _emit_line("checkpoint-begin", 200) + "\n"
            + _emit_line("checkpoint-end", 200, status="ok")
        )
        script = cluster.write_file(
            "checkpoint-multinode.sh", f"#!/bin/bash\n{emit_body}\n", all_nodes=True
        )
        code, out = cluster.srun_with_exit(
            ["-N", "2", "-n", "2", "-t", "2", "bash", script], timeout=90
        )
        assert code == 0, f"srun failed (exit {code}):\n{out}"

        _wait_for_increase(
            lambda: _metric_value(
                _scrape_jobs(cluster),
                'spur_job_runs_ended_total{reason="completed",checkpoint="completed"}',
            ),
            before_runs, 1, timeout=RUN_END_METRIC_TIMEOUT,
            desc="spur_job_runs_ended_total{reason=completed,checkpoint=completed} += 1",
            cluster=cluster,
        )
        after_sum = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_run_checkpoints_completed_sum{reason="completed"}',
        )
        assert after_sum - before_sum == 2, (
            f"expected the checkpoints-completed histogram _sum to grow by 2 "
            f"(two distinct steps, not doubled by node count), got "
            f"{before_sum} -> {after_sum}"
        )


def _create_preempt_qos_pair(cluster: SpurCluster) -> None:
    """Victim QOS carrying a short GraceTime, plus a hunter QOS that
    allow-lists and outranks it."""
    cluster.sacctmgr(["add", "qos", f"name={_PREEMPT_VICTIM_QOS}", "priority=100",
                      f"GraceTime={_PREEMPT_GRACE_SECS}"])
    cluster.sacctmgr(["add", "qos", f"name={_PREEMPT_HUNTER_QOS}", "priority=10000",
                      f"preempt={_PREEMPT_VICTIM_QOS}"])
    time.sleep(_PREEMPT_QOS_WARMUP_SECS)


class TestCheckpointPreemptionRequeue:
    """Comment 1: a victim batch job that reports a completed checkpoint
    before its grace deadline, then is preempt-requeued, is recorded
    `reason="preempted"`, `checkpoint="completed"` -- not `"none"` or
    dropped. Checkpoints from the batch script itself, not an srun step: see
    the module docstring's note on grace delivery to srun steps."""

    def test_preempted_batch_checkpoint_is_recorded(self, checkpoint_preempt_cluster):
        cluster = checkpoint_preempt_cluster
        _create_preempt_qos_pair(cluster)
        before = _metric_value(
            _scrape_jobs(cluster),
            'spur_job_runs_ended_total{reason="preempted",checkpoint="completed"}',
        )

        victim_script = cluster.write_file(
            "checkpoint-preempt-victim.sh",
            "#!/bin/bash\n" + EMIT_FN +
            _emit_line("checkpoint-begin", 900) + "\n"
            "sleep 1\n" +
            _emit_line("checkpoint-end", 900, status="ok") + "\n"
            "sleep 600\n",
        )
        victim_id = parse_job_id(cluster.sbatch(
            ["-J", "ckpt-preempt-victim", "-N", "1", "--exclusive",
             "-q", _PREEMPT_VICTIM_QOS, victim_script]
        ))
        assert victim_id is not None
        wait_job_state(cluster, victim_id, "R", timeout=60)

        aggressor_script = cluster.write_file(
            "checkpoint-preempt-aggressor.sh", "#!/bin/bash\nsleep 5\n"
        )
        aggressor_id = parse_job_id(cluster.sbatch(
            ["-J", "ckpt-preempt-aggressor", "-N", "1", "--exclusive",
             "-q", _PREEMPT_HUNTER_QOS, aggressor_script]
        ))
        assert aggressor_id is not None

        try:
            wait_job_state(
                cluster, victim_id, "PD",
                timeout=_PREEMPT_GRACE_SECS + _PREEMPT_KILL_WAIT_SECS + 30,
            )

            _wait_for_increase(
                lambda: _metric_value(
                    _scrape_jobs(cluster),
                    'spur_job_runs_ended_total{reason="preempted",checkpoint="completed"}',
                ),
                before, 1, timeout=RUN_END_METRIC_TIMEOUT,
                desc="spur_job_runs_ended_total{reason=preempted,checkpoint=completed} += 1",
                cluster=cluster, job_id=victim_id,
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])
