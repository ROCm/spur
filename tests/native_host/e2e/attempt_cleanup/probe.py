# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Agent launch-fence regression probe; run only in a disposable Docker container.

Old supervisors and their workloads are real. Recovery requests fail temporarily
so controller-driven cancellation cannot mask the launch fence.
"""

import argparse
from concurrent import futures
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import threading
import time
import traceback

import grpc
from google.protobuf import empty_pb2
import slurm_pb2 as pb
import slurm_pb2_grpc as rpc


JOB = 7001
NEW_ATTEMPT = 4
BATCH = 0xFFFFFFFB
EXTERN = 0xFFFFFFFC
STATE = Path("/state")
CGROUP = Path("/sys/fs/cgroup/spur")
EVIDENCE = Path("/evidence")
CONTROLLER = "127.0.0.1:16817"
AGENT = "127.0.0.1:16818"
NODE = "attempt-node"


def wait_for(label, predicate, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {label}")


def process_identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return fields[0], int(fields[19])
    except FileNotFoundError:
        return None


def alive(pid, ticks):
    identity = process_identity(pid)
    return bool(identity and identity[0] not in {"Z", "X"} and identity[1] == ticks)


def descriptor(attempt, step):
    path = STATE / "runtime" / f"{JOB}.{attempt}.{step}" / "descriptor.json"
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def control(desc, operation):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(10)
        sock.connect(desc["socket_path"])
        stream = sock.makefile("rwb")
        hello = {
            "operation": "hello", "protocol_version": 1,
            "capability": desc["capability"], "spurd_instance_id": "live-fence-probe",
            "run_attempt": desc["run_attempt"], "step_id": desc["step_id"],
        }
        stream.write(json.dumps(hello).encode() + b"\n")
        stream.flush()
        response = json.loads(stream.readline())
        assert response["result"] == "hello", response
        stream.write(json.dumps({"operation": operation}).encode() + b"\n")
        stream.flush()
        return json.loads(stream.readline())


class Controller(rpc.SlurmControllerServicer):
    def __init__(self):
        self.lock = threading.Lock()
        self.recovered = set()
        self.heartbeats = []

    def RegisterAgent(self, request, context):
        return pb.RegisterAgentResponse(accepted=True)

    def Heartbeat(self, request, context):
        with self.lock:
            self.heartbeats.append([(j.job_id, j.run_attempt) for j in request.running_jobs])
        return pb.HeartbeatResponse()

    def ReportStepdRecovery(self, request, context):
        with self.lock:
            self.recovered.add((request.run_attempt, request.step_id))
        context.abort(grpc.StatusCode.UNAVAILABLE, "recovery withheld by the fault fixture")

    def ReportJobStatus(self, request, context):
        return empty_pb2.Empty()

    def CompleteJobStep(self, request, context):
        return empty_pb2.Empty()

    def DeregisterAgent(self, request, context):
        return empty_pb2.Empty()


def enable_children(path):
    supported = set((path / "cgroup.controllers").read_text().split())
    required = {"cpuset", "cpu", "memory", "pids"}
    assert required <= supported, (path, supported)
    (path / "cgroup.subtree_control").write_text(" ".join("+" + c for c in sorted(required)))


def setup_cgroup_namespace():
    assert Path("/.dockerenv").exists(), "this probe must run inside Docker"
    assert os.getuid() == 0
    subprocess.run(["mount", "-t", "cgroup2", "none", "/sys/fs/cgroup"], check=True)
    root = CGROUP.parent
    assert subprocess.check_output(["stat", "-f", "-c", "%T", str(root)], text=True).strip() == "cgroup2fs"
    pids = (root / "cgroup.procs").read_text().split()
    assert str(os.getpid()) in pids, "the cgroup namespace must be rooted at this container"
    daemon = root / "daemon"
    daemon.mkdir()
    for pid in pids:
        try:
            (daemon / "cgroup.procs").write_text(pid)
        except ProcessLookupError:
            pass
    enable_children(root)
    CGROUP.mkdir()
    enable_children(CGROUP)


class Fixture:
    def __init__(self, scenario, mode):
        self.scenario = scenario
        self.mode = mode
        self.children = []
        self.logs = []
        self.mounts = []
        self.old = []
        self.sentinels = []
        self.controller = Controller()
        self.server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
        rpc.add_SlurmControllerServicer_to_server(self.controller, self.server)
        assert self.server.add_insecure_port(CONTROLLER)
        self.server.start()

    def spawn(self, name, argv):
        log = (EVIDENCE / f"{name}.log").open("w")
        self.logs.append(log)
        child = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
        self.children.append(child)
        return child

    def start_old_step(self, attempt, step):
        parent = CGROUP / f"job_{JOB}_{attempt}"
        if step != BATCH and not parent.exists():
            parent.mkdir()
            enable_children(parent)
        spec = {
            "job_id": JOB, "run_attempt": attempt, "step_id": step,
            "script": "#!/bin/bash\nexec sleep 600\n",
            "work_dir": "/work", "name": "old-run", "user": "nobody", "node": NODE,
            "environment": {"PATH": "/usr/bin:/bin"},
            "stdout_path": f"/work/old-{attempt}-{step}.out",
            "stderr_path": f"/work/old-{attempt}-{step}.err", "stdin_path": "",
            "cpus": 1, "memory_mb": 64, "cpu_ids": [min(os.sched_getaffinity(0))], "gpu_devices": [],
            "uid": 65534, "gid": 65534, "partition": "test", "nodelist": NODE,
            "memlock": "Inherit", "open_mode": None,
            "cgroup": {"enabled": True, "required": True},
            "resources": {"cpus": 1, "memory_mb": 64, "partition": "test", "nodelist": NODE,
                          "cpu_ids": [min(os.sched_getaffinity(0))]},
            "controller_addr": "http://" + CONTROLLER, "reporting_node": NODE,
        }
        path = STATE / f"launch-{attempt}-{step}.json"
        path.write_text(json.dumps(spec))
        child = self.spawn(f"old-{attempt}-{step}", [
            "/opt/spur/spurstepd", str(STATE), str(JOB), str(attempt), str(path),
        ])
        desc = wait_for(f"supervisor {attempt}/{step}", lambda: descriptor(attempt, step))
        assert control(desc, "start")["result"] == "acknowledged"

        def launched():
            assert child.poll() is None, f"old supervisor {attempt}/{step} exited"
            published = descriptor(attempt, step)
            return published if published and published["workload_pid"] else None

        desc = wait_for(f"old workload {attempt}/{step}", launched)
        assert alive(desc["workload_pid"], desc["workload_start_ticks"])
        path = Path(desc["cgroup_path"])
        assert path.parent == parent, desc["cgroup_path"]
        assert str(desc["workload_pid"]) in (path / "cgroup.procs").read_text().split()
        self.old.append(desc)

    def prepare(self):
        STATE.mkdir(mode=0o700)
        Path("/work").mkdir(mode=0o750)
        os.chown("/work", 65534, 65534)
        for attempt in (1, 2, 3):
            shared = self.scenario in {"shared-slots", "orphaned", "nested-orphans"}
            steps = (BATCH, 0) if shared else (attempt * 10, attempt * 10 + 1)
            for step in steps:
                self.start_old_step(attempt, step)
        if self.scenario in {"orphaned", "nested-orphans"}:
            hidden = STATE / "hidden"
            hidden.mkdir()
            for desc in self.old:
                if desc["run_attempt"] < 3:
                    session = Path(desc["socket_path"]).parent
                    shutil.move(str(session), str(hidden / session.name))
        if self.scenario == "nested-orphans":
            for desc in self.old:
                leaf = Path(desc["cgroup_path"]) / "descendants"
                leaf.mkdir()
                (leaf / "cgroup.procs").write_text(str(desc["workload_pid"]))
        for job, attempt in ((JOB + 1, 1), (JOB, NEW_ATTEMPT + 1)):
            parent = CGROUP / f"job_{job}_{attempt}"
            parent.mkdir()
            enable_children(parent)
            leaf = parent / "step_0"
            leaf.mkdir()
            child = self.spawn(f"sentinel-{job}-{attempt}", ["sleep", "600"])
            (leaf / "cgroup.procs").write_text(str(child.pid))
            self.sentinels.append((leaf, child.pid, process_identity(child.pid)[1]))
        if self.scenario == "parent-busy":
            self.bind_mount(CGROUP / f"job_{JOB}_1")
        if self.scenario == "leaf-busy":
            for desc in self.old:
                self.bind_mount(Path(desc["cgroup_path"]))

        config = STATE / "spur.conf"
        config.write_text('''cluster_name = "live-cgroup-fence"
[auth]
plugin = "none"
mode = "disabled"
[network]
wg_enabled = false
[devices]
auto_detect = false
[cgroup]
enabled = true
required = true
[rlimits]
memlock = "inherit"
[update]
check_on_startup = false
auto_update = false
[metrics]
enabled = false
[rest_api]
enabled = false
''')
        agent = self.spawn("spurd", [
            "/opt/spur/spurd", "-f", str(config), "--state-dir", str(STATE),
            "--controller", "http://" + CONTROLLER, "--hostname", NODE,
            "--address", "127.0.0.1", "--listen", AGENT, "--log-level", "info", "-D",
        ])
        self.channel = grpc.insecure_channel(AGENT)
        grpc.channel_ready_future(self.channel).result(timeout=30)
        assert agent.poll() is None
        self.client = rpc.SlurmAgentStub(self.channel)
        self.client.GetNodeResources(empty_pb2.Empty(), timeout=10)
        expected = {(d["run_attempt"], d["step_id"]) for d in self.old
                    if self.scenario not in {"orphaned", "nested-orphans"} or d["run_attempt"] == 3}
        wait_for("recovery reports", lambda: expected <= self.controller.recovered)
        assert all(alive(d["workload_pid"], d["workload_start_ticks"]) for d in self.old)

    def bind_mount(self, path):
        subprocess.run(["mount", "--bind", str(path), str(path)], check=True)
        self.mounts.append(path)

    def snapshot(self):
        attempts = []
        for attempt in (1, 2, 3):
            root = CGROUP / f"job_{JOB}_{attempt}"
            steps = [d for d in self.old if d["run_attempt"] == attempt]
            attempts.append({
                "attempt": attempt, "parent_exists": root.exists(),
                "remaining_cgroups": [str(p.relative_to(CGROUP)) for p in root.rglob("*")
                                      if p.is_dir()] if root.exists() else [],
                "workloads_alive": [d["step_id"] for d in steps if alive(d["workload_pid"], d["workload_start_ticks"])],
                "supervisors_alive": [d["step_id"] for d in steps if alive(d["pid"], d["process_start_ticks"])],
            })
        return attempts

    def dispatch(self):
        try:
            if self.mode == "batch":
                result = self.client.LaunchJob(pb.LaunchJobRequest(
                    job_id=JOB, run_attempt=NEW_ATTEMPT, target_node=NODE, peer_nodes=[NODE],
                    spec=pb.JobSpec(
                        name="new-run", user="nobody", uid=65534, gid=65534,
                        script="#!/bin/bash\nexec sleep 600\n", work_dir="/work",
                        stdout_path="/work/new.out", stderr_path="/work/new.err",
                        num_nodes=1, num_tasks=1, cpus_per_task=1, partition="test", nodelist=NODE,
                    ), allocated=pb.ResourceAllocations(cpus=1, memory_mb=64),
                ), timeout=30)
                return {"accepted": result.success, "error": result.error}
            result = self.client.RegisterJobAllocation(pb.RegisterJobAllocationRequest(
                job_id=JOB, run_attempt=NEW_ATTEMPT, user="nobody", uid=65534, gid=65534,
                cpus=1, memory_mb=64, work_dir="/work", partition="test", nodelist=NODE,
                allocated=pb.ResourceAllocations(cpus=1, memory_mb=64),
            ), timeout=30)
            return {"accepted": not result.error and result.failure_kind == 0, "error": result.error}
        except grpc.RpcError as error:
            return {"accepted": False, "status": error.code().name, "error": error.details()}

    def verify_new(self):
        step = BATCH if self.mode == "batch" else EXTERN
        if self.mode == "batch":
            self.client.StartJob(pb.AgentStartJobRequest(job_id=JOB, run_attempt=NEW_ATTEMPT), timeout=10)

        def started():
            desc = descriptor(NEW_ATTEMPT, step)
            if not desc:
                return None
            if self.mode == "batch" and not desc["workload_pid"]:
                return None
            return desc

        desc = wait_for("new attempt started", started)
        assert alive(desc["pid"], desc["process_start_ticks"])
        assert Path(desc["cgroup_path"]).is_dir()
        if self.mode == "batch":
            assert alive(desc["workload_pid"], desc["workload_start_ticks"])
            assert str(desc["workload_pid"]) in (Path(desc["cgroup_path"]) / "cgroup.procs").read_text().split()
        result = {"attempt": NEW_ATTEMPT, "supervisor_alive": True,
                  "workload_alive": self.mode == "batch", "cgroup": desc["cgroup_path"]}
        if self.mode == "allocation":
            probe = self.client.RunCommand(pb.RunCommandRequest(
                job_id=JOB, step_id=99, uid=65534, gid=65534, work_dir="/work",
                command=["/bin/cat", "/proc/self/cgroup"], num_tasks=1, step_num_tasks=1,
            ), timeout=30)
            assert probe.exit_code == 0, probe
            assert f"/spur/job_{JOB}_{NEW_ATTEMPT}/step_99" in probe.stdout, probe
            result["step_cgroup_probe"] = probe.stdout.strip()
        return result

    def clear_mounts(self):
        for path in reversed(self.mounts):
            subprocess.run(["umount", str(path)], check=True)
        self.mounts.clear()

    def verify_preserved(self):
        for path, pid, ticks in self.sentinels:
            assert path.is_dir(), f"unrelated cgroup removed: {path}"
            assert alive(pid, ticks), f"unrelated process killed: {pid}"
            assert str(pid) in (path / "cgroup.procs").read_text().split()

    def close(self):
        self.clear_mounts()
        for child in reversed(self.children):
            if child.poll() is None:
                child.kill()
            child.wait()
        if hasattr(self, "channel"):
            self.channel.close()
        self.server.stop(0).wait()
        for log in self.logs:
            log.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", choices=[
        "distinct-slots", "shared-slots", "orphaned", "nested-orphans", "parent-busy", "leaf-busy",
    ], required=True)
    parser.add_argument("--mode", choices=["batch", "allocation"], required=True)
    args = parser.parse_args()
    report = {"scenario": args.scenario, "mode": args.mode}
    fixture = None
    try:
        EVIDENCE.mkdir(exist_ok=True)
        setup_cgroup_namespace()
        fixture = Fixture(args.scenario, args.mode)
        fixture.prepare()
        report["before"] = fixture.snapshot()
        report["recovered"] = sorted(fixture.controller.recovered)
        report["tracked_before"] = [
            [d["run_attempt"], d["step_id"]] for d in fixture.old
            if fixture.client.ProbeStepd(pb.StepdProbeRequest(
                job_id=JOB, run_attempt=d["run_attempt"], step_id=d["step_id"],
            ), timeout=15).active
        ]
        report["dispatch"] = fixture.dispatch()
        report["at_dispatch_return"] = fixture.snapshot()
        fixture.verify_preserved()
        if args.scenario in {"parent-busy", "leaf-busy"}:
            assert not report["dispatch"]["accepted"], report["dispatch"]
            assert report["dispatch"].get("status") == "UNAVAILABLE", report["dispatch"]
            assert not (CGROUP / f"job_{JOB}_{NEW_ATTEMPT}").exists()
            assert report["at_dispatch_return"][0]["parent_exists"]
            fixture.clear_mounts()
            report["retry"] = fixture.dispatch()
            assert report["retry"]["accepted"], report["retry"]
        else:
            assert report["dispatch"]["accepted"], report["dispatch"]
        report["at_acceptance"] = fixture.snapshot()
        for attempt in report["at_acceptance"]:
            assert not attempt["parent_exists"], attempt
            assert not attempt["workloads_alive"], attempt
            assert not attempt["remaining_cgroups"], attempt
        report["new"] = fixture.verify_new()
        fixture.verify_preserved()
        wait_for("old supervisors to exit", lambda: all(
            not alive(d["pid"], d["process_start_ticks"]) for d in fixture.old
        ))
        report["after_start"] = fixture.snapshot()
        report["cleanup_before_acceptance"] = True
        report["probe_completed"] = True
    except Exception:
        report["probe_completed"] = False
        report["error"] = traceback.format_exc()
    finally:
        if fixture:
            fixture.close()
        EVIDENCE.mkdir(exist_ok=True)
        (EVIDENCE / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2), flush=True)
    return 0 if report.get("probe_completed") and report.get("cleanup_before_acceptance") else 1


if __name__ == "__main__":
    raise SystemExit(main())
