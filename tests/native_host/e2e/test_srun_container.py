# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for containerized srun job steps on bare-metal nodes (issue #777).

Two dispatch paths exist for a containerized step, and they are tested
separately because they are reached by genuinely different setups:

  Case 2 — NEW container per step (spurd forks a fresh rootfs):
    Reached by standalone `srun --container-image` and by `salloc
    --container-image` + `srun`. Neither runs a batch process on the compute
    node, so the step's tracked job has no live namespaces and spurd builds a
    new container for the step. Capabilities exercised: container entry, bind
    mounts, workdir, home mount, user identity, DNS, GPU env deny, GPU device
    injection.

  Case 1 — ENTER the parent container (nsenter):
    Reached only by `sbatch --container-image` + a nested `srun`. The batch
    script runs *inside* a live container, so the nested step enters those
    namespaces instead of building a new rootfs. Capabilities exercised:
    container entry, parent bind mounts visible, GPU visibility through nsenter.

  Multi-node:
    A single `srun -N2 --container-image` step fans out one container per
    node; concurrent `srun --overlap` steps run one container per node.
"""

import shlex
import subprocess
import time
from pathlib import Path

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


def _toolchain_mounts(cluster) -> list:
    """--container-mounts args that make the host `spur`/`srun` binary runnable
    inside a container: the bin dir plus the shared-library dirs it links
    against (libc, libm, libgcc_s, ld-linux). Bind-mounting the *node's* own
    libs (rather than baking them into the image) guarantees an ABI match with
    the node's binary."""
    paths = [cluster.bin_dir, "/lib", "/lib64", "/usr/lib"]
    return [f"--container-mounts={p}:{p}:ro" for p in paths]


MARKER_PATH = "/inside-image-marker"
MARKER_CONTENT = "spur-container-step-marker"


# ---------------------------------------------------------------------------
# Image builder + fixtures
# ---------------------------------------------------------------------------

def _build_marked_image(tmp_path: Path, cluster) -> str:
    """Minimal squashfs image with MARKER_PATH and common utilities."""
    remote_path = f"{cluster.remote_dir}/step-test-container.sqsh"
    rootfs = tmp_path / "rootfs"
    local_img = tmp_path / "step-test-container.sqsh"

    build_script = f"""set -e
R='{rootfs}'
mkdir -p "$R/bin" "$R/usr/bin" "$R/lib" "$R/lib64" \
  "$R/etc" "$R/dev" "$R/proc" "$R/sys" "$R/tmp" \
  "$R/run" "$R/home" "$R/mnt"
for b in bash cat echo sleep hostname id stat ls touch pwd env grep awk; do
  src=$(which "$b" 2>/dev/null) || continue
  [ -f "$src" ] && cp "$src" "$R/usr/bin/"
done
ln -sf /usr/bin/bash "$R/bin/bash"
ln -sf /usr/bin/bash "$R/bin/sh"
for f in "$R/usr/bin/"*; do
  ldd "$f" 2>/dev/null | grep '=>' | awk '{{print $3}}' | while read -r lib; do
    [ -f "$lib" ] || continue
    dir=$(dirname "$lib")
    mkdir -p "$R$dir"
    cp -n "$lib" "$R$lib" 2>/dev/null || true
  done
done
for nsslib in libnss_dns.so.2 libnss_files.so.2 libresolv.so.2; do
  src="/lib/x86_64-linux-gnu/$nsslib"
  [ -f "$src" ] && mkdir -p "$R/lib/x86_64-linux-gnu" && cp -n "$src" "$R/lib/x86_64-linux-gnu/" 2>/dev/null || true
done
[ -f /lib64/ld-linux-x86-64.so.2 ] && cp -n /lib64/ld-linux-x86-64.so.2 "$R/lib64/" 2>/dev/null || true
for f in /etc/passwd /etc/group /etc/nsswitch.conf; do
  [ -f "$f" ] && cp "$f" "$R/etc/"
done
echo '{MARKER_CONTENT}' > "$R{MARKER_PATH}"
mksquashfs "$R" '{local_img}' -noappend -quiet >/dev/null 2>&1
"""
    result = subprocess.run(["sh", "-c", build_script], capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"mksquashfs failed: {result.stderr}")

    for node in cluster.nodes:
        node.upload(str(local_img), remote_path)

    probe = cluster.nodes[0].exec_allow_fail(
        f"test -e '{MARKER_PATH}' && echo PRESENT || echo ABSENT"
    )
    if "PRESENT" in probe:
        pytest.skip(f"marker {MARKER_PATH} exists on host — test cannot be conclusive")

    return remote_path


def _bracket(pattern: str) -> str:
    """Wrap the first char in a regex class so pgrep -f does not match the shell
    that is running pgrep itself (its own cmdline contains the literal pattern)."""
    return f"[{pattern[0]}]{pattern[1:]}" if pattern else pattern


def _wait_no_process(cluster, pattern: str, timeout: int = 20) -> bool:
    """Poll node 0 until no process matches *pattern* (pgrep -f), or timeout."""
    pat = _bracket(pattern)
    deadline = time.time() + timeout
    while time.time() < deadline:
        out = cluster.nodes[0].exec_allow_fail(f"pgrep -f {shlex.quote(pat)} || echo NONE")
        if "NONE" in out or not out.strip():
            return True
        time.sleep(1)
    return False


def _parse_probe(content: str) -> dict:
    out = {}
    for line in content.splitlines():
        if "=" not in line or line.startswith(" ") or line.startswith("\x1b"):
            continue
        key, _, val = line.partition("=")
        out[key.strip()] = val.strip()
    return out


@pytest.fixture
def step_container_cluster(cluster, tmp_path):
    cluster.container_preflight()
    cluster.step_container_image = _build_marked_image(tmp_path, cluster)
    return cluster


@pytest.fixture
def step_container_multi_cluster(multi_node_cluster, tmp_path):
    multi_node_cluster.container_preflight()
    multi_node_cluster.step_container_image = _build_marked_image(tmp_path, multi_node_cluster)
    return multi_node_cluster


@pytest.fixture
def step_gpu_container_cluster(gpu_cluster, tmp_path):
    """GPU-capable cluster with a container image and the shared GPU probe.

    Depending on `gpu_cluster` auto-marks these tests `gpu` (conftest) and skips
    them when no GPU hardware is present.
    """
    gpu_cluster.gpu_preflight(1)
    gpu_cluster.container_preflight()
    gpu_cluster.step_container_image = _build_marked_image(tmp_path, gpu_cluster)
    probe = gpu_cluster.ship_fixture("gpu_env_probe.sh")
    for node in gpu_cluster.nodes:
        node.exec(f"chmod +x '{probe}'")
    gpu_cluster.step_probe = probe
    return gpu_cluster


# ---------------------------------------------------------------------------
# Case 2: new container per step (standalone srun --container-image)
# ---------------------------------------------------------------------------

class TestSrunContainerStepNewRootfs:
    def test_container_entry(self, step_container_cluster):
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "cat", MARKER_PATH,
        ])
        assert code == 0, f"srun --container-image failed (exit {code}):\n{out}"
        assert MARKER_CONTENT in out, f"step ran on host, not in container:\n{out}"

    def test_bind_mount(self, step_container_cluster):
        # Mount a directory (the common case, matching the batch container test);
        # file bind mounts are a separate, untested edge case.
        cluster = step_container_cluster
        bind_dir = f"{cluster.remote_dir}/bind-test"
        cluster.nodes[0].exec(
            f"mkdir -p '{bind_dir}' && echo 'bind-mount-works' > '{bind_dir}/data.txt'"
        )
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            f"--container-mounts={bind_dir}:/mnt/data:ro",
            "cat", "/mnt/data/data.txt",
        ])
        assert code == 0, f"srun with bind mount failed (exit {code}):\n{out}"
        assert "bind-mount-works" in out, f"bind mount not visible in step:\n{out}"

    def test_workdir(self, step_container_cluster):
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-workdir=/tmp",
            "pwd",
        ])
        assert code == 0, f"srun --container-workdir failed (exit {code}):\n{out}"
        assert "/tmp" in out.strip(), f"expected workdir /tmp, got:\n{out}"

    def test_mount_home(self, step_container_cluster):
        """--container-mount-home makes the job user's home available inside the
        step. The probe prints diagnostics (HOME, its contents, mount table) and
        always exits 0, so a failed assertion surfaces the real in-container
        state instead of a bare exit code."""
        cluster = step_container_cluster
        probe = (
            'echo "HOME=$HOME"; echo "USER=$USER"; '
            'test -d "$HOME" && echo HOME_IS_DIR || echo HOME_NOT_DIR; '
            'echo "-- ls $HOME --"; ls -la "$HOME" 2>&1 | head -20; '
            'echo "-- home mounts --"; grep -i home /proc/mounts 2>/dev/null || echo NO_HOME_MOUNT; '
            'echo "-- write --"; { touch "$HOME/.spur_probe" && echo HOME_WRITABLE; } || echo HOME_NOT_WRITABLE; '
            'true'
        )
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-mount-home",
            "bash", "-c", probe,
        ])
        assert "HOME_WRITABLE" in out, (
            f"--container-mount-home did not yield a usable home (exit {code}):\n{out}"
        )

    def test_user_identity(self, step_container_cluster):
        """id runs inside the container (passwd/shadow injected — no 'no such user')."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "id",
        ])
        assert code == 0, f"srun id failed (exit {code}):\n{out}"
        assert "uid=" in out, f"id output malformed inside container:\n{out}"

    def test_dns_injected(self, step_container_cluster):
        """/etc/resolv.conf is injected (image ships without one)."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "bash", "-c", "test -s /etc/resolv.conf && echo DNS_OK || echo DNS_MISSING",
        ])
        assert code == 0, f"srun dns check failed (exit {code}):\n{out}"
        assert "DNS_OK" in out, f"/etc/resolv.conf not injected in step:\n{out}"

    def test_gpu_env_denied_on_zero_gpu_step(self, step_container_cluster):
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "bash", "-c", "echo ROCR=${ROCR_VISIBLE_DEVICES:-unset}",
        ])
        assert code == 0, f"srun gpu env check failed (exit {code}):\n{out}"
        assert "ROCR=-1" in out or "ROCR=unset" in out, f"GPU env not denied:\n{out}"
        assert "ROCR=0" not in out, f"GPU 0 visible in zero-GPU step:\n{out}"

    def test_container_env_gpu_deny_not_bypassable(self, step_container_cluster):
        """--container-env cannot re-enable GPU visibility on a zero-GPU step."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-env=ROCR_VISIBLE_DEVICES=0",
            "bash", "-c", "echo ROCR=${ROCR_VISIBLE_DEVICES:-unset}",
        ])
        assert code == 0, f"srun container-env check failed (exit {code}):\n{out}"
        assert "ROCR=0" not in out, f"--container-env smuggled GPU visibility:\n{out}"

    def test_named_container(self, step_container_cluster):
        """--container-name is accepted and the step still runs in the image."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-name=step-named-ctr",
            "cat", MARKER_PATH,
        ])
        assert code == 0, f"srun --container-name failed (exit {code}):\n{out}"
        assert MARKER_CONTENT in out, f"named-container step not in image:\n{out}"

    def test_container_entrypoint(self, step_container_cluster):
        """--container-entrypoint runs before the step command inside the container."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-entrypoint=touch /tmp/ep_ran",
            "bash", "-c", "test -f /tmp/ep_ran && echo EP_OK || echo EP_MISSING",
        ])
        assert code == 0, f"srun --container-entrypoint failed (exit {code}):\n{out}"
        assert "EP_OK" in out, (
            f"entrypoint did not run before the step command:\n{out}"
        )

    def test_salloc_then_srun_container(self, step_container_cluster):
        """salloc allocation followed by a containerized srun step (issue's suggested case).

        The salloc allocation has no compute-node container, so the step builds
        its own (Case 2). The container image is specified on the step's srun.
        """
        cluster = step_container_cluster
        img = cluster.step_container_image
        out_path = f"{cluster.remote_dir}/salloc-step.out"
        body = (
            f"'{cluster.bin_dir}/srun' --container-image='{img}' "
            f"cat '{MARKER_PATH}' > '{out_path}' 2>&1"
        )
        code, out = cluster.salloc_run(body, salloc_args=["-N", "1", "-t", "0:02"])
        assert code == 0, f"salloc failed (exit {code}):\n{out}"
        step_out = cluster.nodes[0].read_file(out_path)
        assert MARKER_CONTENT in step_out, (
            f"salloc + srun --container-image step not in container:\n{step_out}"
        )

    def test_cancellation_terminates_container_step(self, step_container_cluster):
        """scancel of a containerized srun step terminates the container (no orphan).

        A standalone `srun --container-image sleep 300` is launched in the
        background; scancel must stop the container so nothing lingers.
        """
        cluster = step_container_cluster
        img = cluster.step_container_image
        name = "cancel-ctr-step"
        out_path = f"{cluster.remote_dir}/cancel-ctr.out"
        launch = (
            f"SPUR_CONTROLLER_ADDR={shlex.quote(cluster.controller_addr)} "
            f"PATH={shlex.quote(cluster.bin_dir)}:$PATH "
            f"nohup {shlex.quote(cluster.bin_dir + '/srun')} -J {name} -N1 -t 0:05 "
            f"--container-image={shlex.quote(img)} sleep 300 "
            f"> {shlex.quote(out_path)} 2>&1 & echo backgrounded"
        )
        cluster.nodes[0].exec(launch)

        job_id = None
        for _ in range(30):
            ids = cluster.running_job_ids_by_name(name)
            if ids:
                job_id = ids[0]
                break
            time.sleep(1)
        assert job_id is not None, "containerized srun step never reached running"

        cluster.scancel(str(job_id))

        state = wait_job(cluster, job_id, timeout=45)
        assert state in ("CA", "F", "COMPLETED", "GONE"), (
            f"cancelled containerized step should be terminal, got {state}"
        )

        # The container's sleep must not linger (allow for the SIGKILL escalation
        # and the srun client teardown).
        assert _wait_no_process(cluster, "sleep 300", timeout=20), (
            "container step process lingered after cancel:\n"
            + cluster.nodes[0].exec_allow_fail("pgrep -af '[s]leep 300' || echo NONE")
        )

    def test_ctrl_c_terminates_container_step(self, step_container_cluster):
        """Ctrl-C (SIGINT to the srun client) terminates the containerized step.

        srun's Ctrl-C handler sends a job cancel; the agent must signal the
        in-flight containerized step so nothing orphans — the path exercised by
        an interactive `srun --container-image ...` the user aborts.
        """
        cluster = step_container_cluster
        img = cluster.step_container_image
        name = "ctrlc-ctr-step"
        out_path = f"{cluster.remote_dir}/ctrlc.out"
        launch = (
            f"SPUR_CONTROLLER_ADDR={shlex.quote(cluster.controller_addr)} "
            f"PATH={shlex.quote(cluster.bin_dir)}:$PATH "
            f"nohup {shlex.quote(cluster.bin_dir + '/srun')} -J {name} -N1 -t 0:05 "
            f"--container-image={shlex.quote(img)} sleep 300 "
            f"> {shlex.quote(out_path)} 2>&1 & echo PID:$!"
        )
        res = cluster.nodes[0].exec(launch)
        srun_pid = next(
            (tok.split(":", 1)[1].strip() for tok in res.split() if tok.startswith("PID:")),
            None,
        )
        assert srun_pid, f"could not capture srun client pid:\n{res}"

        job_id = None
        for _ in range(30):
            ids = cluster.running_job_ids_by_name(name)
            if ids:
                job_id = ids[0]
                break
            time.sleep(1)
        assert job_id is not None, "containerized srun step never reached running"

        # Ctrl-C: SIGINT to the srun client triggers its cancel handler.
        cluster.nodes[0].exec_allow_fail(f"kill -INT {srun_pid}")

        state = wait_job(cluster, job_id, timeout=45)
        assert state in ("CA", "F", "COMPLETED", "GONE"), (
            f"Ctrl-C'd containerized step should be terminal, got {state}"
        )
        assert _wait_no_process(cluster, "sleep 300", timeout=20), (
            "container step process lingered after Ctrl-C:\n"
            + cluster.nodes[0].exec_allow_fail("pgrep -af '[s]leep 300' || echo NONE")
        )


# ---------------------------------------------------------------------------
# Case 1: enter the parent container via nsenter
# ---------------------------------------------------------------------------

class TestSrunContainerStepNsenter:
    """A step whose job is already containerized enters the parent container's
    namespaces (nsenter) instead of building a new rootfs.

    Two shapes are covered:
      - `spur exec` into a running containerized job: `spur` runs on the host and
        enters the container — no in-image binary needed. This proves the nsenter
        mechanism works for a rootless user-namespace container.
      - a nested `srun` *inside* an `sbatch --container-image` batch script: the
        script runs post-pivot_root, so `spur`/`srun` and its shared libraries
        are bind-mounted in via `_toolchain_mounts`. The nested srun attaches as
        a step (SPUR_JOB_ID is in the batch env) and routes through run_command's
        Case 1.
    """

    def test_spur_exec_enters_container_job(self, step_container_cluster):
        cluster = step_container_cluster
        img = cluster.step_container_image
        hold = cluster.write_file("nsenter-hold.sh", "#!/bin/bash\nsleep 300\n")
        sb = cluster.sbatch([
            "-J", "nsenter-hold", "-N", "1", "-t", "0:05",
            f"--container-image={img}", hold,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch did not return a job id: {sb}"
        try:
            wait_job_state(cluster, job_id, "R", timeout=60)
            out = cluster.cli(["spur", "exec", str(job_id), "cat", MARKER_PATH])
            assert MARKER_CONTENT in out, (
                f"spur exec did not enter the parent container:\n{out}"
            )
        finally:
            cluster.scancel(str(job_id))

    def test_nested_srun_enters_parent_container(self, step_container_cluster):
        cluster = step_container_cluster
        img = cluster.step_container_image
        out_path = f"{cluster.remote_dir}/nsenter-entry.out"
        script = cluster.write_file(
            "nsenter-entry.sh",
            f"#!/bin/bash\n{cluster.bin_dir}/srun cat '{MARKER_PATH}'\n",
        )
        sb = cluster.sbatch([
            "-J", "nsenter-entry", "-N", "1", "-t", "0:03", "-o", out_path,
            f"--container-image={img}",
            *_toolchain_mounts(cluster),
            script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch did not return a job id: {sb}"
        wait_job(cluster, job_id, timeout=120)
        out = cluster.read_output_on_any_node(out_path)
        diag = cluster.debug_job(job_id)
        assert MARKER_CONTENT in out, (
            f"nested srun did not enter parent container:\n{diag}\noutput:\n{out}"
        )

    def test_parent_bind_mount_visible_to_nested_step(self, step_container_cluster):
        cluster = step_container_cluster
        img = cluster.step_container_image
        bind_dir = f"{cluster.remote_dir}/nsenter-bind"
        cluster.nodes[0].exec(
            f"mkdir -p '{bind_dir}' && echo 'nsenter-bind-ok' > '{bind_dir}/data.txt'"
        )

        out_path = f"{cluster.remote_dir}/nsenter-bind.out"
        script = cluster.write_file(
            "nsenter-bind.sh",
            f"#!/bin/bash\n{cluster.bin_dir}/srun cat /mnt/nsdata/data.txt\n",
        )
        sb = cluster.sbatch([
            "-J", "nsenter-bind", "-N", "1", "-t", "0:03", "-o", out_path,
            f"--container-image={img}",
            f"--container-mounts={bind_dir}:/mnt/nsdata:ro",
            *_toolchain_mounts(cluster),
            script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        out = cluster.read_output_on_any_node(out_path)
        diag = cluster.debug_job(job_id)
        assert "nsenter-bind-ok" in out, (
            f"parent bind mount not visible to nested step:\n{diag}\noutput:\n{out}"
        )


# ---------------------------------------------------------------------------
# GPU device injection (requires GPU hardware; auto-skipped otherwise)
# ---------------------------------------------------------------------------

class TestSrunContainerStepGpuInjection:
    def test_new_container_step_gpu_injection(self, step_gpu_container_cluster):
        """Standalone srun --gres=gpu:1 --container-image sees the GPU device
        nodes bind-mounted into the fresh step container (Case 2)."""
        cluster = step_gpu_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:03",
            "--gres=gpu:1",
            f"--container-image={cluster.step_container_image}",
            f"--container-mounts={cluster.step_probe}:/probe.sh:ro",
            "/probe.sh",
        ])
        assert code == 0, f"gpu step failed (exit {code}):\n{out}"
        parsed = _parse_probe(out)
        assert parsed.get("VISIBLE_COUNT") == "1", f"expected 1 visible GPU:\n{out}"
        assert parsed.get("SPUR_COUNT") == "1", f"expected SPUR_JOB_GPUS=1:\n{out}"
        assert parsed.get("KFD") == "yes", f"/dev/kfd not injected in step:\n{out}"
        assert int(parsed.get("RENDER_COUNT", "0")) >= 1, (
            f"no /dev/dri/renderD* injected in step:\n{out}"
        )

    def test_nsenter_step_sees_parent_gpus(self, step_gpu_container_cluster):
        """A nested srun (nsenter) sees the GPUs injected into the parent
        sbatch --gres=gpu:1 --container-image job (Case 1)."""
        cluster = step_gpu_container_cluster
        img = cluster.step_container_image
        out_path = f"{cluster.remote_dir}/nsenter-gpu.out"
        script = cluster.write_file(
            "nsenter-gpu.sh",
            f"#!/bin/bash\n{cluster.bin_dir}/srun /probe.sh\n",
        )
        sb = cluster.sbatch([
            "-J", "nsenter-gpu", "-N", "1", "-t", "0:03", "-o", out_path,
            "--gres=gpu:1",
            f"--container-image={img}",
            f"--container-mounts={cluster.step_probe}:/probe.sh:ro",
            *_toolchain_mounts(cluster),
            script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        out = cluster.read_output_on_any_node(out_path)
        parsed = _parse_probe(out)
        diag = cluster.debug_job(job_id)
        assert parsed.get("VISIBLE_COUNT") == "1", (
            f"nested step did not see parent GPU:\n{diag}\n{out}"
        )
        assert parsed.get("KFD") == "yes", f"/dev/kfd not visible to nested step:\n{out}"

    def test_gpu_isolation_two_gpus(self, step_gpu_container_cluster):
        """A step allocated 2 GPUs sees exactly 2 (no more, no fewer)."""
        cluster = step_gpu_container_cluster
        if max(cluster.node_gpu_count(n) for n in cluster.node_names) < 2:
            pytest.skip("need >= 2 GPUs on a node")
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:03",
            "--gres=gpu:2",
            f"--container-image={cluster.step_container_image}",
            f"--container-mounts={cluster.step_probe}:/probe.sh:ro",
            "/probe.sh",
        ])
        assert code == 0, f"gpu step failed (exit {code}):\n{out}"
        parsed = _parse_probe(out)
        assert parsed.get("VISIBLE_COUNT") == "2", f"expected 2 visible GPUs:\n{out}"
        assert int(parsed.get("RENDER_COUNT", "0")) == 2, (
            f"expected exactly 2 renderD nodes injected:\n{out}"
        )


# ---------------------------------------------------------------------------
# Multi-node
# ---------------------------------------------------------------------------

class TestSrunContainerStepMultiNode:
    def test_single_step_fans_out_to_all_nodes(self, step_container_multi_cluster):
        """One `srun -N2 --container-image` step runs a container on each node."""
        cluster = step_container_multi_cluster
        code, out = cluster.srun_with_exit([
            "-N", "2", "-t", "0:03",
            f"--container-image={cluster.step_container_image}",
            "cat", MARKER_PATH,
        ])
        assert code == 0, f"multi-node srun step failed (exit {code}):\n{out}"
        assert out.count(MARKER_CONTENT) >= 2, (
            f"expected the marker from both nodes' containers, got:\n{out}"
        )

    def test_concurrent_overlap_steps(self, step_container_multi_cluster):
        """The Miles pattern: from a salloc shell, launch one containerized
        `srun --overlap` per node concurrently. Each step builds its own
        container (Case 2). srun runs on the host, so the redirects land on the
        allocation shell's node."""
        cluster = step_container_multi_cluster
        img = cluster.step_container_image
        out1 = f"{cluster.remote_dir}/overlap-1.out"
        out2 = f"{cluster.remote_dir}/overlap-2.out"
        node1, node2 = cluster.node_names[0], cluster.node_names[1]
        body = (
            f"'{cluster.bin_dir}/srun' --overlap -N1 -w '{node1}' "
            f"--container-image='{img}' cat '{MARKER_PATH}' > '{out1}' 2>&1 &\n"
            f"'{cluster.bin_dir}/srun' --overlap -N1 -w '{node2}' "
            f"--container-image='{img}' cat '{MARKER_PATH}' > '{out2}' 2>&1 &\n"
            f"wait"
        )
        code, out = cluster.salloc_run(body, salloc_args=["-N", "2", "-t", "0:03"])
        assert code == 0, f"salloc overlap run failed (exit {code}):\n{out}"
        # Both srun clients run in the salloc shell (node 0), so both outputs
        # land on node 0.
        out1_c = cluster.nodes[0].read_file(out1)
        out2_c = cluster.nodes[0].read_file(out2)
        assert MARKER_CONTENT in out1_c, f"node1 overlap step not in container:\n{out1_c}"
        assert MARKER_CONTENT in out2_c, f"node2 overlap step not in container:\n{out2_c}"


# ---------------------------------------------------------------------------
# Sanity / negative
# ---------------------------------------------------------------------------

class TestSrunContainerStepSanity:
    def test_container_readonly_rejected(self, step_container_cluster):
        """--container-readonly is refused at submission (not a silent no-op)."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            f"--container-image={cluster.step_container_image}",
            "--container-readonly",
            "true",
        ])
        assert code != 0, f"--container-readonly should be rejected, got exit 0:\n{out}"
        assert "container-readonly" in out and "not yet implemented" in out, (
            f"expected a clear rejection message, got:\n{out}"
        )

    def test_step_without_image_runs_on_host(self, step_container_cluster):
        """A step with no container image runs on the host and cannot see the
        in-image marker — proves the flag is what gates the container path."""
        cluster = step_container_cluster
        code, out = cluster.srun_with_exit([
            "-N", "1", "-t", "0:02",
            "bash", "-c", f"test -f {MARKER_PATH} && echo FOUND || echo NOTFOUND",
        ])
        assert code == 0, f"host srun step failed (exit {code}):\n{out}"
        assert "NOTFOUND" in out, f"marker found on host — assumption violated:\n{out}"
