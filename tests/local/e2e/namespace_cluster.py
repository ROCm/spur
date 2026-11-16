# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Process driver for the disposable mount/PID namespace used by local E2E tests."""

import concurrent.futures
import ctypes
import fcntl
import json
import os
from pathlib import Path
import resource
import select
import signal
import socket
import struct
import subprocess
import sys
import threading

import grpc


class ControllerProxy(grpc.GenericRpcHandler):
    """Forward real RPCs, with an explicit fault/barrier on the spool query."""

    def __init__(self, address):
        self.channel = grpc.insecure_channel(address)
        self.mode = "pass"
        self.requests = []
        self.heartbeats = 0
        self.condition = threading.Condition()
        self.release = threading.Event()
        self.release.set()

    def service(self, details):
        method = details.method

        def forward(request, context):
            if method.endswith("/Heartbeat"):
                with self.condition:
                    self.heartbeats += 1
                    self.condition.notify_all()
            if method.endswith("/GetJobs"):
                with self.condition:
                    self.requests.append(request.hex())
                    mode = self.mode
                    self.condition.notify_all()
                if mode == "blocked":
                    self.release.wait()
                elif mode == "unavailable":
                    context.abort(grpc.StatusCode.UNAVAILABLE, "fixture query failure")
                elif mode == "legacy":
                    return b""
                elif mode == "legacy-terminal":
                    # A non-authoritative response claiming job 1000 is terminal with full visibility.
                    return b"\x0a\x05\x08\xe8\x07\x38\x05\x10\x01"
            try:
                return self.channel.unary_unary(method)(request, timeout=10)
            except grpc.RpcError as error:
                context.abort(error.code(), error.details())

        return grpc.unary_unary_rpc_method_handler(forward)


class NamespaceCluster:
    def __init__(self, root, binaries):
        self.root = Path(root)
        self.binaries = Path(binaries)
        self.processes = {}
        self.files = []
        self.agent_number = 0
        self.config = self.root / "spur.conf"
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("SPUR_", "SLURM_", "SYSTEMD_", "DBUS_")) and key != "RUST_LOG"
        }
        self.environment.update({
            "PATH": f"{self.binaries}:{os.environ.get('PATH', '')}",
            "TMPDIR": str(self.root / "tmp"),
            "TERM": "dumb",
            "NO_COLOR": "1",
            "USER": "root",
            "LOGNAME": "root",
            "SPUR_CONF": str(self.config),
            "RUST_MIN_STACK": "2097152",
        })
        for name in ["var-spool/spur", "cgroup", "tmp/spur", "logs", "controller",
                     "etc/systemd/system", "run", "cache"]:
            (self.root / name).mkdir(parents=True, exist_ok=True)
        for name in ["var-spool/spur", "tmp/spur"]:
            (self.root / name).chmod(0o755)
        (self.root / "cgroup/cgroup.controllers").write_text("cpu memory pids cpuset\n")
        (self.root / "etc/passwd").write_text("root:x:0:0:fixture root:/root:/bin/bash\n")
        (self.root / "etc/group").write_text("root:x:0:\n")
        (self.root / "etc/nsswitch.conf").write_text("passwd: files\ngroup: files\nhosts: files\n")
        (self.root / "etc/hosts").write_text("127.0.0.1 localhost local-e2e\n")
        (self.root / "etc/mtab").symlink_to("/proc/mounts")
        subprocess.run(["mount", "--make-rprivate", "/"], check=True)
        for source, target in [("etc", "/etc"), ("run", "/run"), ("cache", "/var/cache"),
                               ("var-spool", "/var/spool")]:
            subprocess.run(["mount", "--bind", str(self.root / source), target], check=True)
        subprocess.run(["mount", "--bind", str(self.root / "cgroup"), "/sys/fs/cgroup"], check=True)
        subprocess.run(["mount", "-o", "remount,bind,ro", "/sys/fs/cgroup"], check=True)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as control:
            interface = struct.pack("16sH22x", b"lo", 0)
            flags = fcntl.ioctl(control, 0x8913, interface)
            active = struct.unpack("16sH22x", flags)[1] | 1
            fcntl.ioctl(control, 0x8914, struct.pack("16sH22x", b"lo", active))
        reservations = [socket.socket() for _ in range(3)]
        for reservation in reservations:
            reservation.bind(("127.0.0.1", 0))
        self.controller_port, self.agent_port, raft_port = [s.getsockname()[1] for s in reservations]
        self.controller_address = f"127.0.0.1:{self.controller_port}"
        self.proxy = ControllerProxy(self.controller_address)
        self.server = grpc.server(concurrent.futures.ThreadPoolExecutor(max_workers=8))
        self.server.add_generic_rpc_handlers((self.proxy,))
        self.proxy_port = self.server.add_insecure_port("127.0.0.1:0")
        self.server.start()
        for reservation in reservations:
            reservation.close()
        self.environment["SPUR_CONTROLLER_ADDR"] = f"http://{self.controller_address}"
        self.config.write_text(f'''cluster_name = "local-e2e"
[controller]
listen_addr = "{self.controller_address}"
raft_listen_addr = "127.0.0.1:{raft_port}"
state_dir = {json.dumps(str(self.root / "controller"))}
first_job_id = 1000
heartbeat_timeout_secs = 90
[scheduler]
interval_secs = 1
[auth]
plugin = "none"
allow_root_jobs = true
[network]
wg_enabled = false
agent_port = {self.agent_port}
[metrics]
enabled = false
[cgroup]
enabled = true
required = false
[[nodes]]
names = "local-e2e"
cpus = 4
memory_mb = 4096
[[partitions]]
name = "default"
default = true
nodes = "local-e2e"
state = "UP"
max_time = "00:05:00"
''')

    def start(self, name, arguments, environment=None):
        log = self.root / "logs" / f"{name}.log"
        output = log.open("w")
        self.files.append(output)
        process = subprocess.Popen(arguments, cwd=self.root, env=environment or self.environment,
                                   stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
        self.processes[name] = process
        return {"pid": process.pid, "log": str(log), "name": name}

    def wait_file(self, path, text, process=None):
        # inotify waits for actual writes; no guessed daemon startup delay.
        libc = ctypes.CDLL(None, use_errno=True)
        descriptor = libc.inotify_init1(os.O_CLOEXEC | os.O_NONBLOCK)
        if descriptor < 0:
            raise OSError(ctypes.get_errno(), "inotify_init1")
        pidfd = os.pidfd_open(self.processes[process].pid) if process else None
        try:
            watch = libc.inotify_add_watch(descriptor, os.fsencode(Path(path).parent), 0x100 | 0x2 | 0x8)
            if watch < 0:
                raise OSError(ctypes.get_errno(), "inotify_add_watch")
            while True:
                content = Path(path).read_text(errors="replace") if Path(path).exists() else ""
                if text in content:
                    return content
                if process and self.processes[process].poll() is not None:
                    raise RuntimeError(f"{process} exited before {text!r}:\n{content}")
                ready, _, _ = select.select([descriptor, *([pidfd] if pidfd is not None else [])], [], [], 30)
                if pidfd in ready:
                    raise RuntimeError(f"{process} exited before {text!r}:\n{content}")
                if not ready:
                    raise RuntimeError(f"no file event while waiting for {text!r}:\n{content}")
                os.read(descriptor, 65536)
        finally:
            os.close(descriptor)
            if pidfd is not None:
                os.close(pidfd)

    def dispatch(self, request):
        operation = request["operation"]
        if operation == "start_controller":
            arguments = [str(self.binaries / "spurctld"), "-D", "-f", str(self.config),
                         "--state-dir", str(self.root / "controller")]
            result = self.start("spurctld", arguments)
            self.wait_file(result["log"], "become leader", "spurctld")
            grpc.channel_ready_future(self.proxy.channel).result(timeout=15)
            return result
        if operation == "start_agent":
            self.agent_number += 1
            name = f"spurd-{self.agent_number}"
            arguments = [str(self.binaries / "spurd"), "-D", "-f", str(self.config),
                         "--controller", f"http://127.0.0.1:{self.proxy_port}",
                         "--hostname", "local-e2e", "--address", "127.0.0.1",
                         "--listen", f"127.0.0.1:{self.agent_port}",
                         "--state-dir", str(self.root / "agent")]
            if request.get("deny_openat2"):
                arguments = [sys.executable, str(Path(__file__).with_name("deny_openat2.py")), *arguments]
            return self.start(name, arguments)
        if operation == "openat2_fault_available":
            result = subprocess.run([sys.executable, str(Path(__file__).with_name("deny_openat2.py")),
                                     "--probe"], env=self.environment, capture_output=True, text=True)
            if result.returncode not in (0, 77):
                raise RuntimeError(f"openat2 fault probe failed: {result.stderr}")
            return {"available": result.returncode == 0, "reason": result.stderr.strip()}
        if operation == "stop":
            process = self.processes[request["name"]]
            process.send_signal(signal.SIGTERM)
            code = process.wait(timeout=15)
            children = Path(f"/proc/self/task/{os.getpid()}/children").read_text().split()
            owned = {process.pid for process in self.processes.values()}
            for child in map(int, children):
                if child not in owned:
                    os.waitpid(child, os.WNOHANG)
            return code
        if operation == "wait_file":
            return self.wait_file(request["path"], request["text"], request.get("process"))
        if operation == "cli":
            result = subprocess.run([str(self.binaries / "spur"), *request["arguments"]],
                                    cwd=self.root, env=self.environment, capture_output=True,
                                    text=True, timeout=30)
            return {"returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
        if operation == "start_cli":
            environment = dict(self.environment)
            environment.update(request.get("environment", {}))
            return self.start(request["name"], [str(self.binaries / "spur"), *request["arguments"]], environment)
        if operation == "start_holder":
            environment = dict(self.environment)
            environment.update(request.get("environment", {}))
            return self.start(request["name"], [sys.executable, "-c", request["script"]], environment)
        if operation == "proxy_mode":
            with self.proxy.condition:
                self.proxy.mode = request["mode"]
                if request["mode"] == "blocked":
                    self.proxy.release.clear()
                else:
                    self.proxy.release.set()
            return None
        if operation == "wait_query":
            with self.proxy.condition:
                if not self.proxy.condition.wait_for(lambda: len(self.proxy.requests) >= request["count"], 30):
                    raise RuntimeError("agent did not issue the expected spool query")
            return list(self.proxy.requests)
        if operation == "queries":
            with self.proxy.condition:
                return list(self.proxy.requests)
        if operation == "wait_heartbeat":
            with self.proxy.condition:
                if not self.proxy.condition.wait_for(lambda: self.proxy.heartbeats >= request["count"], 35):
                    raise RuntimeError("agent did not issue the expected heartbeat")
            return self.proxy.heartbeats
        if operation == "pid_exists":
            # The driver is PID 1 here and reaps late, so a dead child lingers as a zombie.
            try:
                stat = Path(f"/proc/{request['pid']}/stat").read_text()
            except FileNotFoundError:
                return False
            return stat.rsplit(")", 1)[1].split()[0] != "Z"
        if operation == "wait_pid_exit":
            try:
                descriptor = os.pidfd_open(request["pid"])
            except ProcessLookupError:
                return None
            try:
                ready, _, _ = select.select([descriptor], [], [], 30)
                if not ready:
                    raise RuntimeError("supervisor did not exit")
            finally:
                os.close(descriptor)
            return None
        if operation == "wait_process":
            return self.processes[request["name"]].wait(timeout=30)
        if operation == "mount_tmpfs":
            subprocess.run(["mount", "-t", "tmpfs", "tmpfs", request["path"]], check=True)
            return None
        if operation == "mounted":
            mounts = Path("/proc/self/mountinfo").read_text().splitlines()
            return any(line.split(" ")[4] == request["path"] for line in mounts)
        if operation == "make_deep_tree":
            descriptor = os.open(request["path"], os.O_RDONLY | os.O_DIRECTORY)
            try:
                for _ in range(request["depth"]):
                    os.mkdir("d", dir_fd=descriptor)
                    child = os.open("d", os.O_RDONLY | os.O_DIRECTORY, dir_fd=descriptor)
                    os.close(descriptor)
                    descriptor = child
            finally:
                os.close(descriptor)
            return None
        if operation == "remove_deep_tree":
            path = Path(request["path"])
            if not path.exists():
                return None
            descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            depth = 0
            try:
                while True:
                    try:
                        child = os.open("d", os.O_RDONLY | os.O_DIRECTORY, dir_fd=descriptor)
                    except FileNotFoundError:
                        break
                    os.close(descriptor)
                    descriptor = child
                    depth += 1
                for _ in range(depth):
                    parent = os.open("..", os.O_RDONLY | os.O_DIRECTORY, dir_fd=descriptor)
                    os.rmdir("d", dir_fd=parent)
                    os.close(descriptor)
                    descriptor = parent
            finally:
                os.close(descriptor)
            for child in path.iterdir():
                child.unlink()
            path.rmdir()
            return None
        raise ValueError(operation)


def main():
    signal.signal(signal.SIGTERM, lambda *_: os._exit(0))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    cluster = NamespaceCluster(*sys.argv[1:])
    print(json.dumps({"ready": True}), flush=True)
    for line in sys.stdin:
        try:
            result = cluster.dispatch(json.loads(line))
            print(json.dumps({"result": result}), flush=True)
        except Exception as error:
            print(json.dumps({"error": f"{type(error).__name__}: {error}"}), flush=True)


if __name__ == "__main__":
    main()
