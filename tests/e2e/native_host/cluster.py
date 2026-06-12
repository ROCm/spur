# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Spur native-host cluster management for E2E tests.

Handles SSH connections, binary deployment, cluster startup/teardown,
and CLI wrappers for interacting with the running cluster.
"""

import os
import re
import shutil
import subprocess
import time
import logging
from pathlib import Path

import paramiko
import pytest
import tomli_w

logger = logging.getLogger(__name__)

BINARIES = ["spurctld", "spurd", "spur"]
CLI_SYMLINKS = ["sbatch", "srun", "squeue", "scancel", "sinfo", "scontrol"]

CONTROLLER_PORT = int(os.environ.get("SPUR_TEST_CONTROLLER_PORT", "6817"))
AGENT_PORT = int(os.environ.get("SPUR_TEST_AGENT_PORT", "6818"))


def make_remote_dir() -> str:
    """Generate a unique remote working directory path."""
    return f"/tmp/spur-e2e-{os.getpid()}-{time.time_ns()}"


def deep_merge(base: dict, overrides: dict) -> dict:
    """Deep-merge *overrides* into *base* (mutates and returns *base*).

    - Dicts are merged recursively.
    - Everything else (scalars, lists) is replaced outright.
    """
    for key, value in overrides.items():
        if key in base and isinstance(base[key], dict) and isinstance(value, dict):
            deep_merge(base[key], value)
        else:
            base[key] = value
    return base


class SshNode:
    """SSH connection to a single test node."""

    def __init__(self, host: str, user: str, password: str | None = None, key_path: str | None = None):
        self.host = host
        self.user = user
        self.client = paramiko.SSHClient()
        self.client.set_missing_host_key_policy(paramiko.AutoAddPolicy())

        connect_kwargs = {"hostname": host, "username": user}
        if key_path:
            connect_kwargs["key_filename"] = key_path
        elif password:
            connect_kwargs["password"] = password
        else:
            connect_kwargs["allow_agent"] = True

        self.client.connect(**connect_kwargs)
        self._sftp = None

    @property
    def sftp(self) -> paramiko.SFTPClient:
        if self._sftp is None:
            self._sftp = self.client.open_sftp()
        return self._sftp

    def exec(self, cmd: str, check: bool = True) -> str:
        """Run a command via SSH. Returns stdout. Raises on non-zero exit if check=True."""
        _, stdout, stderr = self.client.exec_command(cmd)
        exit_code = stdout.channel.recv_exit_status()
        out = stdout.read().decode()
        err = stderr.read().decode()

        if check and exit_code != 0:
            raise RuntimeError(
                f"Command failed on {self.host} (exit {exit_code}): {cmd}\n"
                f"stdout: {out}\nstderr: {err}"
            )
        return out

    def exec_allow_fail(self, cmd: str) -> str:
        """Run a command, returning stdout+stderr regardless of exit code."""
        _, stdout, stderr = self.client.exec_command(cmd)
        stdout.channel.recv_exit_status()
        out = stdout.read().decode()
        err = stderr.read().decode()
        return out + err

    def upload(self, local_path: str, remote_path: str):
        """Upload a local file to the remote node."""
        self.sftp.put(local_path, remote_path)

    def write_file(self, remote_path: str, content: str, mode: int | None = None):
        """Write string content to a remote file, optionally setting permissions."""
        with self.sftp.open(remote_path, "w") as f:
            f.write(content)
        if mode is not None:
            self.sftp.chmod(remote_path, mode)

    def read_file(self, remote_path: str) -> str:
        """Read a remote file. Returns empty string if file doesn't exist."""
        return self.exec(f"cat '{remote_path}' 2>/dev/null || true", check=False)

    def close(self):
        if self._sftp:
            self._sftp.close()
        self.client.close()


def ensure_bins(nodes: list[SshNode], binaries_dir: str, bin_dir: str):
    """
    Upload binaries to all nodes if not already present (or size differs).
    bin_dir is the remote directory where binaries are installed.
    """
    for node in nodes:
        node.exec(f"mkdir -p '{bin_dir}'")

    for name in BINARIES:
        local_path = Path(binaries_dir) / name
        if not local_path.is_file():
            raise FileNotFoundError(
                f"Missing binary: {local_path}\n"
                f"Set SPUR_TEST_BINARIES_DIR or run: cargo build --release"
            )
        local_size = local_path.stat().st_size

        for node in nodes:
            remote_path = f"{bin_dir}/{name}"
            remote_size = node.exec_allow_fail(
                f"stat -c%s '{remote_path}' 2>/dev/null || echo 0"
            ).strip()

            if remote_size == str(local_size):
                logger.debug("Binary %s already present on %s", name, node.host)
                continue

            logger.info("Uploading %s to %s", name, node.host)
            node.upload(str(local_path), remote_path)
            node.exec(f"chmod +x '{remote_path}'")

    # Create CLI symlinks
    symlink_cmd = (
        f"cd '{bin_dir}' && "
        + " && ".join(f"ln -sf spur {cmd} 2>/dev/null || true" for cmd in CLI_SYMLINKS)
    )
    for node in nodes:
        node.exec_allow_fail(symlink_cmd)

    logger.info("Binaries ready at %s on all nodes", bin_dir)


class SpurCluster:
    """
    Manages a Spur cluster lifecycle on remote nodes.

    Starts controller + agents in a unique working directory,
    waits for the cluster to become ready, and tears everything down.
    """

    def __init__(self, nodes: list[SshNode], remote_dir: str, bin_dir: str):
        self.nodes = nodes
        self.node_names: list[str] = []
        self.remote_dir = remote_dir
        self.bin_dir = bin_dir
        self.etc_dir = f"{remote_dir}/etc"
        self.state_dir = f"{remote_dir}/state"
        self.log_dir = f"{remote_dir}/log"
        self.controller_addr = f"http://{nodes[0].host}:{CONTROLLER_PORT}"
        self.config_overrides: dict = {}
        self.agent_as_root: bool = False
        self.agent_labels: dict[int, dict[str, str]] = {}

    # --- Lifecycle ---

    def provision(self):
        """Create remote dirs and resolve hostnames.

        After this call the cluster infrastructure is ready (dirs exist,
        hostnames known) but no daemons are running.  Call :meth:`start`
        to bring up the cluster with a specific configuration.
        """
        self._create_dirs()
        self._resolve_hostnames()
        logger.info("Cluster provisioned: %s", self.node_names)

    def start(
        self,
        config_overrides: dict | None = None,
        kill_stale: bool = True,
        agent_as_root: bool = False,
        agent_labels: dict[int, dict[str, str]] | None = None,
    ):
        """Write config and start all daemons.

        *config_overrides* is deep-merged into the default config.
        Requires :meth:`provision` to have been called first.

        When *kill_stale* is True (default), any lingering spurctld/spurd
        processes on the nodes are killed before starting fresh.

        When *agent_as_root* is True, spurd is launched via sudo on each
        node (rootful agent). spurctld always runs as the SSH user.

        *agent_labels* maps node index to a dict of labels that will be
        passed as ``--label key=value`` args to spurd on that node.
        """
        if not self.node_names:
            raise RuntimeError("provision() must be called before start()")
        if kill_stale:
            self._kill_daemons(use_sudo=False)
            self._kill_daemons(use_sudo=True)
        self.agent_as_root = agent_as_root
        self.agent_labels = agent_labels or {}
        self.config_overrides = config_overrides or {}
        self._write_config()
        self._start_controller()
        time.sleep(2)
        self._start_agents()
        self._wait_all_idle(timeout=120)
        logger.info(
            "Cluster ready: %s (agent_as_root=%s)",
            self.node_names,
            self.agent_as_root,
        )

    def stop(self):
        """Kill all daemons but keep the working directory intact."""
        self._kill_daemons(use_sudo=self.agent_as_root)

    def deploy(
        self,
        config_overrides: dict | None = None,
        agent_as_root: bool = False,
        agent_labels: dict[int, dict[str, str]] | None = None,
    ):
        """Provision + start in one call."""
        self.provision()
        if agent_as_root:
            self.root_agent_preflight()
        self.start(config_overrides, agent_as_root=agent_as_root, agent_labels=agent_labels)

    def teardown(self):
        """Kill all daemons and remove the working directory."""
        self.stop()
        rm_prefix = self._sudo_prefix() if self.agent_as_root else ""
        for node in self.nodes:
            node.exec_allow_fail(f"{rm_prefix}rm -rf '{self.remote_dir}'")
        logger.info("Cluster torn down")

    # --- CLI wrappers ---

    def cli(self, args: list[str]) -> str:
        """Run a spur CLI command on the controller node."""
        cmd_parts = [
            f"SPUR_CONTROLLER_ADDR='{self.controller_addr}'",
            f"PATH='{self.bin_dir}':$PATH",
            f"'{self.bin_dir}/{args[0]}'",
        ]
        cmd_parts.extend(f"'{a}'" for a in args[1:])
        return self.nodes[0].exec(" ".join(cmd_parts))

    def cli_allow_fail(self, args: list[str]) -> str:
        """Run a spur CLI command, returning stdout+stderr regardless of exit
        code. Use to assert on expected submission rejections."""
        cmd_parts = [
            f"SPUR_CONTROLLER_ADDR='{self.controller_addr}'",
            f"PATH='{self.bin_dir}':$PATH",
            f"'{self.bin_dir}/{args[0]}'",
        ]
        cmd_parts.extend(f"'{a}'" for a in args[1:])
        return self.nodes[0].exec_allow_fail(" ".join(cmd_parts))

    def sbatch(self, args: list[str]) -> str:
        return self.cli(["sbatch"] + args)

    def squeue_all(self) -> str:
        return self.cli(["squeue", "-t", "all"])

    def sinfo(self) -> str:
        return self.cli(["sinfo"])

    def scancel(self, job_id: str) -> str:
        return self.cli(["scancel", job_id])

    def scontrol(self, *args: str) -> str:
        return self.cli(["scontrol"] + list(args))

    def write_file(self, name: str, body: str, *,
                   all_nodes: bool = False, executable: bool = True) -> str:
        """Write a file under remote_dir. Returns the absolute remote path.

        By default the file is written to the controller node only and
        made executable.  Set *all_nodes=True* to write on every node,
        and *executable=False* for non-script files.
        """
        path = f"{self.remote_dir}/{name}"
        mode = 0o755 if executable else None
        targets = self.nodes if all_nodes else self.nodes[:1]
        parent = path.rsplit("/", 1)[0]
        for node in targets:
            node.exec(f"mkdir -p '{parent}'")
            node.write_file(path, body, mode=mode)
        return path

    def read_output_on_any_node(self, path: str) -> str:
        """Try to read a file from any node (controller first)."""
        for node in self.nodes:
            content = node.read_file(path)
            if content.strip():
                return content
        return ""

    def read_output_all_nodes(self, path: str) -> str:
        """Read a file from all nodes and combine."""
        combined = []
        for node in self.nodes:
            content = node.read_file(path)
            if content.strip():
                combined.append(content)
        return "\n".join(combined)

    def debug_job(self, job_id: int) -> str:
        """Collect diagnostic info for a failed job."""
        lines = [f"=== DEBUG job {job_id} ==="]

        try:
            lines.append("scontrol show job:")
            lines.append(self.scontrol("show", "job", str(job_id)))
        except Exception:
            pass

        try:
            lines.append("sinfo:")
            lines.append(self.sinfo())
        except Exception:
            pass

        try:
            lines.append("squeue:")
            lines.append(self.squeue_all())
        except Exception:
            pass

        ctrl_log = f"{self.log_dir}/spurctld.log"
        log = self.nodes[0].exec_allow_fail(f"tail -30 '{ctrl_log}'")
        if log.strip():
            lines.append(f"spurctld.log (last 30 lines):\n{log}")

        for i, node in enumerate(self.nodes):
            agent_log = f"{self.log_dir}/spurd.log"
            log = node.exec_allow_fail(f"tail -15 '{agent_log}'")
            if log.strip():
                lines.append(f"spurd.log on {self.node_names[i]} (last 15 lines):\n{log}")

        return "\n".join(lines)

    def ship_file_to_all(self, local_path: Path, remote_name: str):
        """SCP a local file to all nodes under remote_dir."""
        for node in self.nodes:
            node.upload(str(local_path), f"{self.remote_dir}/{remote_name}")

    def gpu_preflight(self, min_nodes: int):
        """Verify that at least min_nodes have GPU hardware. Skips test if not."""
        gpu_nodes = []
        for i, node in enumerate(self.nodes):
            probe = node.exec_allow_fail(
                "{ ls /dev/kfd 2>/dev/null && echo HAS_GPU; } || "
                "{ nvidia-smi -L 2>/dev/null && echo HAS_GPU; } || "
                "echo NO_GPU"
            )
            if "HAS_GPU" in probe:
                gpu_nodes.append(self.node_names[i])

        if len(gpu_nodes) < min_nodes:
            pytest.skip(
                f"GPU preflight: need {min_nodes} GPU node(s), "
                f"found {len(gpu_nodes)} ({gpu_nodes})"
            )

    def root_agent_preflight(self):
        """Verify passwordless (or password-backed) sudo for rootful spurd. Skips if not."""
        for i, node in enumerate(self.nodes):
            try:
                node.exec(f"{self._sudo_prefix()}true")
            except RuntimeError:
                pytest.skip(
                    f"rootful spurd requires sudo on {self.node_names[i]} "
                    f"(set SPUR_TEST_SSH_PASSWORD or configure NOPASSWD sudo)"
                )

    def require_nodes(self, min_nodes: int):
        """Skip the test if fewer than min_nodes are configured."""
        if len(self.nodes) < min_nodes:
            pytest.skip(
                f"Need at least {min_nodes} nodes in SPUR_TEST_NODES "
                f"(got {len(self.nodes)})"
            )

    @staticmethod
    def devices_config(auto_detect: bool = True, **extra) -> dict:
        """Return config_overrides for spur-devices CDI auto-detect."""
        devices = {"auto_detect": auto_detect, **extra}
        return {"devices": devices}

    def spurd_log(self, node_index: int = 0) -> str:
        raw = self.nodes[node_index].read_file(f"{self.log_dir}/spurd.log")
        return re.sub(r"\x1b\[[0-9;]*m", "", raw)

    def spurd_registry_gpu_count(self, node_index: int = 0) -> int | None:
        """Parse cdi_devices count from spurd startup log."""
        log = self.spurd_log(node_index)
        match = re.search(r"cdi_devices=(\d+)", log)
        if match:
            return int(match.group(1))
        match = re.search(r"resources discovered.*?gpus=(\d+)", log)
        if match:
            return int(match.group(1))
        return None

    def assert_spurd_registry(self, node_index: int = 0, min_gpus: int = 1):
        log = self.spurd_log(node_index)
        assert "device registry initialized" in log, (
            f"spurd on {self.node_names[node_index]} must initialize device registry\n"
            f"log tail:\n{log[-2000:]}"
        )
        count = self.spurd_registry_gpu_count(node_index)
        assert count is not None and count >= min_gpus, (
            f"expected >= {min_gpus} CDI GPUs on {self.node_names[node_index]}, "
            f"got {count}\nlog tail:\n{log[-2000:]}"
        )

    def scontrol_show_node(self, node_name: str) -> str:
        return self.scontrol("show", "node", node_name)

    def node_gpu_count(self, node_name: str) -> int:
        """Return schedulable GPU count from scontrol show node."""
        out = self.scontrol_show_node(node_name)
        for line in out.splitlines():
            if "Gres=" not in line:
                continue
            gres = line.split("Gres=", 1)[1].strip()
            if not gres:
                return 0
            return len(re.findall(r"gpu:[^,]+", gres))
        return 0

    def assert_sinfo_gpus(self, min_per_node: int = 1):
        for name in self.node_names:
            count = self.node_gpu_count(name)
            assert count >= min_per_node, (
                f"node {name} must expose >= {min_per_node} GPU(s) in scontrol, "
                f"got {count}\n{self.scontrol_show_node(name)}"
            )

    def ship_fixture(self, fixture_name: str) -> str:
        """Ship a file from native_host/fixtures/ to remote_dir on all nodes."""
        fixtures_dir = Path(__file__).resolve().parent / "fixtures"
        local_path = fixtures_dir / fixture_name
        if not local_path.is_file():
            raise FileNotFoundError(f"Missing fixture: {local_path}")
        self.ship_file_to_all(local_path, fixture_name)
        return f"{self.remote_dir}/{fixture_name}"

    def compile_hip_fixture(self, hip_filename: str) -> str:
        """Ship and compile a HIP fixture on all nodes. Returns remote binary path."""
        self.ship_fixture(hip_filename)
        bin_name = hip_filename.rsplit(".", 1)[0]
        remote_bin = f"{self.remote_dir}/{bin_name}"
        remote_hip = f"{self.remote_dir}/{hip_filename}"
        for node in self.nodes:
            node.exec_allow_fail(
                f"export PATH=/opt/rocm/bin:$PATH; "
                f"command -v hipcc >/dev/null && "
                f"hipcc -o '{remote_bin}' '{remote_hip}' 2>/dev/null || true"
            )
            if not node.exec_allow_fail(f"test -x '{remote_bin}' && echo OK").strip():
                pytest.skip(f"hipcc could not build {hip_filename} on {node.host}")
        return remote_bin

    def restart_agent(self, node_index: int = 0):
        """Restart spurd on one node without touching the controller."""
        node = self.nodes[node_index]
        self._pkill(node, f"{self.bin_dir}/spurd", use_sudo=self.agent_as_root)
        time.sleep(1)
        node.exec(self._spurd_start_cmd(node_index))
        time.sleep(5)

    def restart_controller(self):
        """Restart spurctld without touching the agents. State is recovered
        from the Raft log on the existing state-dir. Waits for the controller
        to answer queries again (does not require nodes to be idle, since a
        suspended job keeps its allocation)."""
        self._pkill(self.nodes[0], f"{self.bin_dir}/spurctld", use_sudo=False)
        time.sleep(1)
        self._start_controller()
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                if all(n in self.sinfo() for n in self.node_names):
                    return
            except Exception:
                pass
            time.sleep(2)
        raise TimeoutError("controller did not recover after restart")

    def spurd_agent_user(self, node_index: int = 0) -> str:
        """Return the user owning the spurd process on a node."""
        node = self.nodes[node_index]
        return node.exec_allow_fail(
            f"ps -o user= -p $(pgrep -f '{self.bin_dir}/spurd' | head -1) "
            f"2>/dev/null || echo unknown"
        ).strip()

    def wait_output(self, path: str, marker: str, timeout: int = 120) -> str:
        """Poll job output file until marker appears or timeout."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            content = self.read_output_on_any_node(path)
            if marker in content:
                return content
            if "MISSING" in content or "HIP error" in content:
                return content
            time.sleep(2)
        return self.read_output_on_any_node(path)

    def container_preflight(self):
        """Check container prerequisites. Skips test if anything is missing."""
        if not shutil.which("mksquashfs"):
            pytest.skip("mksquashfs not found on test runner (apt install squashfs-tools)")

        for i, node in enumerate(self.nodes):
            probe = node.exec_allow_fail(
                "command -v unsquashfs >/dev/null && echo OK || echo MISSING"
            )
            if "OK" not in probe:
                pytest.skip(
                    f"unsquashfs not found on {self.node_names[i]} "
                    f"(apt install squashfs-tools)"
                )

    def build_container_image(self, tmp_path: Path) -> str:
        """
        Build a minimal squashfs container image locally, ship to all nodes.
        Returns the remote path to the .sqsh file.
        """
        remote_path = f"{self.remote_dir}/test-container.sqsh"
        rootfs = tmp_path / "rootfs"
        local_img = tmp_path / "test-container.sqsh"

        build_script = f"""set -e
R='{rootfs}'
mkdir -p "$R/bin" "$R/usr/bin" "$R/lib" "$R/lib64" \
  "$R/etc" "$R/dev" "$R/proc" "$R/sys" "$R/tmp" \
  "$R/run" "$R/home" "$R/mnt"
for b in bash cat echo sleep hostname id df env stat ls wc head tail tr touch mkdir getent; do
  src=$(which "$b" 2>/dev/null) || continue
  [ -f "$src" ] && cp "$src" "$R/usr/bin/"
done
ln -sf /usr/bin/bash "$R/bin/bash"
ln -sf /usr/bin/bash "$R/bin/sh"
for f in "$R/usr/bin/"*; do
  ldd "$f" 2>/dev/null | grep '=>' | awk '{{print $3}}' | while read -r lib; do
    if [ -f "$lib" ]; then
      dir=$(dirname "$lib")
      mkdir -p "$R$dir"
      cp -n "$lib" "$R$lib" 2>/dev/null || true
    fi
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
mksquashfs "$R" '{local_img}' -noappend -quiet >/dev/null 2>&1
"""
        result = subprocess.run(
            ["sh", "-c", build_script],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            raise RuntimeError(f"mksquashfs failed: {result.stderr}")

        for node in self.nodes:
            node.upload(str(local_img), remote_path)

        return remote_path

    # --- Internal helpers ---

    def _create_dirs(self):
        for node in self.nodes:
            node.exec_allow_fail(f"rm -rf '{self.remote_dir}'")
            node.exec(f"mkdir -p '{self.remote_dir}' '{self.etc_dir}' '{self.state_dir}' '{self.log_dir}'")

    def _resolve_hostnames(self):
        style = os.environ.get("SPUR_TEST_HOSTNAME_STYLE", "short")
        self.node_names = []
        for node in self.nodes:
            if style == "short":
                name = node.exec("hostname -s").strip()
            else:
                name = node.exec("hostname -f").strip()
            if not name:
                name = node.host
            self.node_names.append(name)

    def _default_config(self) -> dict:
        nodes_list = ",".join(self.node_names)
        return {
            "cluster_name": "e2e-test",
            "scheduler": {"interval_secs": 1, "plugin": "backfill"},
            "auth": {"plugin": "none"},
            "network": {"wg_enabled": False, "agent_port": AGENT_PORT},
            "partitions": [
                {
                    "name": "default",
                    "state": "UP",
                    "default": True,
                    "nodes": nodes_list,
                    "max_time": "24:00:00",
                    "default_time": "10:00",
                }
            ],
            "nodes": [
                {"names": name, "cpus": 64, "memory_mb": 262144}
                for name in self.node_names
            ],
        }

    def _write_config(self):
        cfg = self._default_config()
        deep_merge(cfg, self.config_overrides)
        config = tomli_w.dumps(cfg)

        for node in self.nodes:
            node.write_file(f"{self.etc_dir}/spur.conf", config)

    def _start_controller(self):
        listen = f"[::]:{CONTROLLER_PORT}"
        cmd = (
            f"nohup '{self.bin_dir}/spurctld' "
            f"-f '{self.etc_dir}/spur.conf' "
            f"--listen '{listen}' --state-dir '{self.state_dir}' --log-level info -D "
            f"> '{self.log_dir}/spurctld.log' 2>&1 & echo $!"
        )
        pid = self.nodes[0].exec(cmd).strip()
        logger.info("spurctld started on %s (pid %s)", self.node_names[0], pid)

    def _sudo_prefix(self) -> str:
        pw = os.environ.get("SPUR_TEST_SSH_PASSWORD", "")
        if pw:
            escaped = pw.replace("'", "'\"'\"'")
            return f"echo '{escaped}' | sudo -S "
        return "sudo -n "

    def _pkill(self, node: SshNode, pattern: str, *, use_sudo: bool = False):
        prefix = self._sudo_prefix() if use_sudo else ""
        node.exec_allow_fail(f"{prefix}pkill -f '{pattern}' 2>/dev/null || true")

    def _kill_daemons(self, *, use_sudo: bool):
        for node in self.nodes:
            self._pkill(node, f"{self.bin_dir}/spurctld", use_sudo=use_sudo)
            self._pkill(node, f"{self.bin_dir}/spurd", use_sudo=use_sudo)

    def _spurd_start_cmd(self, node_index: int) -> str:
        node = self.nodes[node_index]
        hostname = self.node_names[node_index]
        address = node.host
        agent_listen = f"0.0.0.0:{AGENT_PORT}"
        spurd_bin = (
            f"{self._sudo_prefix()}'{self.bin_dir}/spurd'"
            if self.agent_as_root
            else f"'{self.bin_dir}/spurd'"
        )
        label_args = ""
        labels = self.agent_labels.get(node_index, {})
        if labels:
            label_args = " ".join(f"--label '{k}={v}'" for k, v in labels.items())
            label_args = f" {label_args}"
        return (
            f"nohup {spurd_bin} "
            f"-f '{self.etc_dir}/spur.conf' "
            f"--controller '{self.controller_addr}' "
            f"--listen '{agent_listen}' "
            f"--hostname '{hostname}' --address '{address}' --log-level info -D"
            f"{label_args} "
            f"> '{self.log_dir}/spurd.log' 2>&1 & echo $!"
        )

    def _start_agents(self):
        for i, node in enumerate(self.nodes):
            cmd = self._spurd_start_cmd(i)
            pid = node.exec(cmd).strip()
            logger.info(
                "spurd started on %s (pid %s, root=%s)",
                self.node_names[i],
                pid,
                self.agent_as_root,
            )

    def _wait_all_idle(self, timeout: int):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                out = self.sinfo()
                if self._cluster_is_ready(out):
                    return
            except Exception:
                pass
            time.sleep(2)
        raise TimeoutError(
            f"Cluster not ready after {timeout}s. "
            f"Expected {len(self.node_names)} idle nodes.\n"
            f"sinfo output:\n{self.sinfo()}"
        )

    def _cluster_is_ready(self, sinfo_output: str) -> bool:
        for name in self.node_names:
            if name not in sinfo_output:
                return False
        total_idle = 0
        for line in sinfo_output.splitlines():
            if "idle" not in line:
                continue
            fields = line.split()
            for j, field in enumerate(fields):
                if field == "idle" and j > 0:
                    try:
                        total_idle += int(fields[j - 1])
                    except ValueError:
                        pass
        return total_idle >= len(self.node_names)


# --- Job helpers ---


def parse_job_id(sbatch_output: str) -> int | None:
    """Extract job ID from sbatch output like 'Submitted batch job 42'."""
    parts = sbatch_output.split()
    if parts:
        try:
            return int(parts[-1])
        except ValueError:
            pass
    return None


def job_state(squeue_output: str, job_id: int) -> str | None:
    """Parse job state from squeue -t all output."""
    valid_states = {"PD", "R", "CD", "CG", "F", "CA", "TO", "NF", "PR", "S"}
    id_str = str(job_id)
    for line in squeue_output.splitlines()[1:]:
        fields = line.split()
        if not fields or fields[0] != id_str:
            continue
        for field in fields[1:]:
            if field in valid_states:
                return field
    return None


def wait_job(cluster: SpurCluster, job_id: int, timeout: int = 120) -> str:
    """
    Wait for a job to reach a terminal state. Returns the final state string.
    Returns "GONE" if the job disappears from the queue after being seen.
    Raises TimeoutError if the job doesn't finish within the timeout.
    """
    deadline = time.time() + timeout
    last = ""
    seen = False
    while time.time() < deadline:
        sq = cluster.squeue_all()
        state = job_state(sq, job_id)
        if state in ("CD", "F", "CA", "TO"):
            return state
        if state is None:
            if seen:
                return last if last else "GONE"
            # Job not visible yet — give scheduler a moment
            time.sleep(1)
            continue
        seen = True
        last = state
        time.sleep(2)
    raise TimeoutError(
        f"Job {job_id} did not finish within {timeout}s (last state: {last})"
    )
