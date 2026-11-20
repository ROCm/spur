# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Local E2E fixtures; no SSH, service installation, or host state changes."""

import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys

import pytest


class LocalCluster:
    def __init__(self, process, root):
        self.process = process
        self.root = root
        self.spool = root / "var-spool/spur"
        self.agent = None

    def read_response(self):
        ready, _, _ = select.select([self.process.stdout], [], [], 45)
        assert ready, "local namespace driver did not respond"
        line = self.process.stdout.readline()
        assert line, f"local namespace driver exited ({self.process.poll()})"
        response = json.loads(line)
        assert "error" not in response, response.get("error")
        return response

    def call(self, operation, **arguments):
        self.process.stdin.write(json.dumps({"operation": operation, **arguments}) + "\n")
        self.process.stdin.flush()
        return self.read_response().get("result")

    def cli(self, *arguments, check=True):
        result = self.call("cli", arguments=list(map(str, arguments)))
        if check:
            assert result["returncode"] == 0, result["stdout"] + result["stderr"]
        return result["stdout"] if check else result

    def start_agent(self, *, wait=True, deny_openat2=False):
        self.agent = self.call("start_agent", deny_openat2=deny_openat2)
        if wait:
            return self.wait_agent()
        return self.agent

    def wait_agent(self):
        log = self.call("wait_file", path=self.agent["log"],
                        text="agent gRPC server listening", process=self.agent["name"])
        assert log.index("startup job spool cleanup finished") < log.index("agent gRPC server listening")
        assert log.count("startup job spool cleanup finished") == 1
        return log

    def stop_agent(self):
        assert self.call("stop", name=self.agent["name"]) == 0

    def restart_agent(self):
        self.stop_agent()
        return self.start_agent()

    def cgroups(self, enabled):
        config = self.root / "spur.conf"
        content = config.read_text()
        before = "[cgroup]\nenabled = " + str(not enabled).lower()
        after = "[cgroup]\nenabled = " + str(enabled).lower()
        config.write_text(content.replace(before, after))
        assert after in config.read_text()

    def make_spool(self, job_id):
        path = self.spool / f"job{job_id}"
        path.mkdir(parents=True, exist_ok=True)
        (path / "sentinel").write_text(f"job {job_id}\n")
        return path

    def wait_file(self, path, text):
        return self.call("wait_file", path=str(path), text=text)


@pytest.fixture
def local_cluster(tmp_path):
    if sys.platform != "linux" or not shutil.which("unshare") or not shutil.which("mount"):
        pytest.skip("local daemon E2E requires Linux user, mount, and PID namespaces")
    pytest.importorskip("grpc", reason="local daemon fault injection requires grpcio")
    probe = subprocess.run(["unshare", "--user", "--map-root-user", "--mount", "--pid", "--net", "--fork",
                            "--mount-proc", "true"], capture_output=True, text=True)
    if probe.returncode:
        pytest.skip(f"local namespaces unavailable: {probe.stderr.strip()}")
    repo = Path(__file__).resolve().parents[3]
    binaries = Path(os.environ.get("SPUR_TEST_BINARIES_DIR", repo / "target/release")).resolve()
    missing = [name for name in ["spurd", "spurctld", "spur", "spurstepd"]
               if not os.access(binaries / name, os.X_OK)]
    if missing:
        pytest.skip(f"build local E2E binaries in {binaries}: {', '.join(missing)}")
    driver = Path(__file__).with_name("namespace_cluster.py")
    errors = (tmp_path / "namespace.log").open("w")
    process = subprocess.Popen(["unshare", "--user", "--map-root-user", "--mount", "--pid", "--net", "--fork",
                                "--kill-child", "--mount-proc", sys.executable, str(driver),
                                str(tmp_path), str(binaries)], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=errors, text=True, bufsize=1)
    cluster = LocalCluster(process, tmp_path)
    try:
        assert cluster.read_response().get("ready")
        cluster.call("start_controller")
        yield cluster
    finally:
        # Killing PID 1 reaps every daemon/workload in this private namespace.
        process.kill()
        process.wait(timeout=15)
        process.stdin.close()
        process.stdout.close()
        errors.close()
