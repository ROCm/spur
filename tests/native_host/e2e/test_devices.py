# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""spur-devices E2E tests: CDI auto-detect, GRES scheduling, and GPU injection.

Complements test_gpu.py (HIP smoke without --gres) and test_container.py
(container lifecycle without GPUs). Each suite here validates one concern.
"""

from __future__ import annotations

import re
import time

import pytest

from cluster import SpurCluster, job_state, parse_job_id, wait_job


def _parse_probe(content: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in content.splitlines():
        if "=" not in line or line.startswith(" ") or line.startswith("\x1b"):
            continue
        key, _, val = line.partition("=")
        out[key.strip()] = val.strip()
    return out


def _probe_script(cluster: SpurCluster) -> str:
    remote = cluster.ship_fixture("gpu_env_probe.sh")
    for node in cluster.nodes:
        node.exec(f"chmod +x '{remote}'")
    return remote


def _submit_probe(
    cluster: SpurCluster,
    probe: str,
    gres: str,
    *,
    extra_args: list[str] | None = None,
    job_name: str = "dev-probe",
    timeout: int = 120,
) -> tuple[int, dict[str, str], str]:
    out_path = f"{cluster.remote_dir}/{job_name}.out"
    args = ["-J", job_name, "-N", "1", f"--gres={gres}", "-o", out_path, probe]
    if extra_args:
        args = extra_args + args
    sb = cluster.sbatch(args)
    job_id = parse_job_id(sb)
    assert job_id is not None, f"sbatch failed: {sb}"
    wait_job(cluster, job_id, timeout=timeout)
    content = cluster.wait_output(out_path, "PROBE_OK", timeout=timeout)
    return job_id, _parse_probe(content), content


def _gpu_type_on_node(cluster: SpurCluster, node_index: int = 0) -> str | None:
    out = cluster.scontrol_show_node(cluster.node_names[node_index])
    match = re.search(r"gpu:([^:,]+)", out)
    return match.group(1) if match else None


def _max_gpus_per_node(cluster: SpurCluster) -> int:
    return max(cluster.node_gpu_count(name) for name in cluster.node_names)



class TestCdiAutoDetect:
    def test_autodetect_and_metadata(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(len(cluster.nodes))
        cluster.require_nodes(1)

        for i in range(len(cluster.nodes)):
            cluster.assert_spurd_registry(i, min_gpus=1)
        cluster.assert_sinfo_gpus(min_per_node=1)

        out = cluster.scontrol_show_node(cluster.node_names[0])
        known_types = (
            r"mi100|mi210|mi250x|mi300[xa]|mi308x|mi325x|mi350x|mi355x"
            r"|rx9070(?:xt)?|amdgpu-0x[0-9a-f]+"
        )
        assert re.search(rf"gpu:(?:{known_types})", out, re.IGNORECASE), (
            f"expected a known GPU type in scontrol output:\n{out}"
        )

        before = cluster.spurd_registry_gpu_count(0)
        assert before is not None and before >= 1
        cluster.restart_agent(0)
        cluster.assert_spurd_registry(0, min_gpus=before)
        after = cluster.spurd_registry_gpu_count(0)
        assert after == before, f"GPU count changed after restart: {before} -> {after}"



class TestGresScheduling:
    def test_gres_gpu_allocation(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        probe = _probe_script(cluster)

        _, parsed, content = _submit_probe(cluster, probe, "gpu:1")
        assert "PROBE_OK" in content
        assert parsed.get("VISIBLE_COUNT") == "1", content
        assert parsed.get("SPUR_COUNT") == "1", content

        if _max_gpus_per_node(cluster) >= 2:
            _, parsed, content = _submit_probe(
                cluster, probe, "gpu:2", job_name="gres-2"
            )
            assert "PROBE_OK" in content
            assert parsed.get("VISIBLE_COUNT") == "2", content
            assert parsed.get("SPUR_COUNT") == "2", content

    def test_gres_negative_scheduling(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        probe = _probe_script(cluster)

        gpu_type = _gpu_type_on_node(cluster, 0)
        assert gpu_type, "could not determine GPU type from scontrol"

        _, parsed, content = _submit_probe(
            cluster, probe, f"gpu:{gpu_type}:1", job_name="gres-type-ok"
        )
        assert "PROBE_OK" in content, content

        # Wrong GPU type should stay pending
        out_path = f"{cluster.remote_dir}/gres-type-bad.out"
        sb = cluster.sbatch(
            ["-J", "gres-type-bad", "-N", "1", "--gres=gpu:a100:1", "-o", out_path, probe]
        )
        bad_id = parse_job_id(sb)
        assert bad_id is not None
        deadline = time.time() + 30
        state = "PD"
        while time.time() < deadline:
            sq = cluster.squeue_all()
            state = job_state(sq, bad_id) or "GONE"
            if state in ("CD", "F", "CA", "TO", "GONE"):
                break
            time.sleep(2)
        assert state == "PD", (
            f"wrong GPU type should stay pending, got {state}\n{cluster.debug_job(bad_id)}"
        )
        cluster.scancel(str(bad_id))

        # Requesting more GPUs than available should stay pending
        n = _max_gpus_per_node(cluster)
        out_path = f"{cluster.remote_dir}/gres-exhaust.out"
        sb = cluster.sbatch(
            ["-J", "gres-exhaust", "-N", "1", f"--gres=gpu:{n + 1}", "-o", out_path, probe]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        deadline = time.time() + 30
        state = "PD"
        while time.time() < deadline:
            sq = cluster.squeue_all()
            state = job_state(sq, job_id) or "GONE"
            if state in ("CD", "F", "CA", "TO", "GONE"):
                break
            time.sleep(2)
        assert state == "PD", (
            f"over-allocating GPUs should stay pending, got {state}\n"
            f"{cluster.debug_job(job_id)}"
        )
        cluster.scancel(str(job_id))

    def test_gres_release_after_complete(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        n = min(2, _max_gpus_per_node(cluster))
        if n < 1:
            pytest.skip("no GPUs on node")

        probe = _probe_script(cluster)
        _, _, content1 = _submit_probe(cluster, probe, f"gpu:{n}", job_name="gres-rel-1")
        assert "PROBE_OK" in content1

        _, _, content2 = _submit_probe(cluster, probe, f"gpu:{n}", job_name="gres-rel-2")
        assert "PROBE_OK" in content2



class TestNativeInjection:
    def test_native_env_visibility(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        probe = _probe_script(cluster)

        if _max_gpus_per_node(cluster) >= 2:
            _, parsed, content = _submit_probe(
                cluster, probe, "gpu:2", job_name="native-env"
            )
            assert "PROBE_OK" in content
            rocr = set(parsed.get("ROCR_VISIBLE_DEVICES", "").split(",")) - {""}
            spur = set(parsed.get("SPUR_JOB_GPUS", "").split(",")) - {""}
            assert rocr == spur, f"ROCR {rocr} != SPUR_JOB_GPUS {spur}\n{content}"

        _, parsed, content = _submit_probe(
            cluster, probe, "gpu:1", job_name="native-smi"
        )
        assert "PROBE_OK" in content
        assert parsed.get("VISIBLE_COUNT") == "1", content

    def test_native_hip_respects_allocation(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if _max_gpus_per_node(cluster) < 2:
            pytest.skip("need >= 2 GPUs")

        gpu_bin = cluster.compile_hip_fixture("gpu_alloc_test.hip")
        script = cluster.write_file(
            "native-hip.sh", f"#!/bin/bash\n'{gpu_bin}'\n"
        )
        out_path = f"{cluster.remote_dir}/native-hip.out"
        sb = cluster.sbatch(
            ["-J", "native-hip", "-N", "1", "--gres=gpu:2", "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        content = cluster.wait_output(out_path, "ALLOC_OK", timeout=30)
        assert "GPU count: 2" in content, content
        assert "ALLOC_OK 2/2" in content, content

    @pytest.mark.rootful
    def test_native_device_namespace_isolation(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        assert cluster.spurd_agent_user(0) == "root", (
            f"expected rootful spurd, got user {cluster.spurd_agent_user(0)!r}"
        )

        script = cluster.write_file(
            "native-devs.sh",
            "#!/bin/bash\n"
            "echo \"RENDER_COUNT=$(ls /dev/dri/renderD* 2>/dev/null | wc -l)\"\n"
            "ls /dev/kfd >/dev/null && echo KFD=yes || echo KFD=no\n"
            "echo PROBE_OK\n",
        )
        out_path = f"{cluster.remote_dir}/native-devs.out"
        sb = cluster.sbatch(
            ["-J", "native-devs", "-N", "1", "--gres=gpu:1", "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id)
        content = cluster.wait_output(out_path, "PROBE_OK")
        assert "RENDER_COUNT=1" in content.replace(" ", ""), content
        assert "KFD=yes" in content, content

    def test_prolog_spur_job_gpus(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if _max_gpus_per_node(cluster) < 2:
            pytest.skip("need >= 2 GPUs")

        rd = cluster.remote_dir
        prolog_body = f"""\
#!/bin/bash
mkdir -p '{rd}/hook-out'
echo "SPUR_JOB_GPUS=$SPUR_JOB_GPUS" > '{rd}/hook-out/prolog-gpu.log'
"""
        cluster.write_file("hooks/prolog-gpu.sh", prolog_body, all_nodes=True)
        cluster.stop()
        cluster.start(
            {
                **SpurCluster.devices_config(auto_detect=True),
                "hooks": {"prolog": f"{rd}/hooks/prolog-gpu.sh"},
            }
        )

        probe = _probe_script(cluster)
        out_path = f"{rd}/prolog-job.out"
        sb = cluster.sbatch(
            [
                "-J", "prolog-gpu", "-N", "1", "--gres=gpu:2",
                f"--nodelist={cluster.node_names[0]}",
                "-o", out_path, probe,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id)
        hook_log = cluster.nodes[0].read_file(f"{rd}/hook-out/prolog-gpu.log")
        assert "SPUR_JOB_GPUS=" in hook_log, hook_log
        spur_gpus = hook_log.strip().split("=", 1)[1]
        assert len(spur_gpus.split(",")) == 2, hook_log



class TestConcurrentAllocation:
    def test_concurrent_disjoint_gpu_sets(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        n = _max_gpus_per_node(cluster)
        if n < 8:
            pytest.skip("need >= 8 GPUs for two concurrent gpu:4 jobs")

        probe = _probe_script(cluster)
        target = cluster.node_names[0]
        out_a = f"{cluster.remote_dir}/conc-a.out"
        out_b = f"{cluster.remote_dir}/conc-b.out"
        cluster.sbatch(
            ["-J", "conc-a", "-N", "1", "-w", target, "--gres=gpu:4", "-o", out_a, probe]
        )
        cluster.sbatch(
            ["-J", "conc-b", "-N", "1", "-w", target, "--gres=gpu:4", "-o", out_b, probe]
        )

        content_a = cluster.wait_output(out_a, "PROBE_OK", timeout=180)
        content_b = cluster.wait_output(out_b, "PROBE_OK", timeout=180)

        set_a = set(_parse_probe(content_a).get("SPUR_JOB_GPUS", "").split(",")) - {""}
        set_b = set(_parse_probe(content_b).get("SPUR_JOB_GPUS", "").split(",")) - {""}
        assert len(set_a) == 4, content_a
        assert len(set_b) == 4, content_b
        assert set_a.isdisjoint(set_b), f"disjoint sets required: {set_a} vs {set_b}"

    def test_concurrent_exhaustion(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        n = _max_gpus_per_node(cluster)
        if n < 8:
            pytest.skip("need >= 8 GPUs")

        chunk = n // 2
        hold = cluster.write_file("hold.sh", "#!/bin/bash\nsleep 600\n")
        target = cluster.node_names[0]
        out_a = f"{cluster.remote_dir}/ex-a.out"
        out_b = f"{cluster.remote_dir}/ex-b.out"
        sb_a = cluster.sbatch(
            [
                "-J",
                "ex-a",
                "-N",
                "1",
                "-w",
                target,
                f"--gres=gpu:{chunk}",
                "-o",
                out_a,
                hold,
            ]
        )
        sb_b = cluster.sbatch(
            [
                "-J",
                "ex-b",
                "-N",
                "1",
                "-w",
                target,
                f"--gres=gpu:{chunk}",
                "-o",
                out_b,
                hold,
            ]
        )
        id_a = parse_job_id(sb_a)
        id_b = parse_job_id(sb_b)
        assert id_a and id_b

        running = False
        for _ in range(30):
            sq = cluster.squeue_all()
            if job_state(sq, id_a) == "R" and job_state(sq, id_b) == "R":
                running = True
                break
            time.sleep(2)
        assert running, "both hold jobs should reach running state"

        sb_c = cluster.sbatch(
            [
                "-J",
                "ex-c",
                "-N",
                "1",
                "-w",
                target,
                f"--gres=gpu:{chunk}",
                "-o",
                f"{cluster.remote_dir}/ex-c.out",
                hold,
            ]
        )
        id_c = parse_job_id(sb_c)
        assert id_c

        deadline = time.time() + 30
        state = "PD"
        while time.time() < deadline:
            sq = cluster.squeue_all()
            state = job_state(sq, id_c) or "GONE"
            if state in ("CD", "F", "CA", "TO", "GONE", "R"):
                break
            time.sleep(2)
        assert state == "PD", f"third job should stay pending, got {state}"

        cluster.scancel(str(id_a))
        cluster.scancel(str(id_b))
        cluster.scancel(str(id_c))



@pytest.fixture
def gpu_container_cluster(gpu_cluster, tmp_path):
    cluster = gpu_cluster
    cluster.gpu_preflight(1)
    cluster.container_preflight()
    cluster.container_image = cluster.build_container_image(tmp_path)
    return cluster


class TestContainerInjection:
    def test_container_gpu_injection(self, gpu_container_cluster):
        cluster = gpu_container_cluster
        if _max_gpus_per_node(cluster) < 2:
            pytest.skip("need >= 2 GPUs")

        probe = _probe_script(cluster)
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/ctr-env.out"
        sb = cluster.sbatch(
            [
                "-J",
                "ctr-env",
                "-N",
                "1",
                "--gres=gpu:2",
                f"--container-image={img}",
                "-o",
                out_path,
                probe,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        content = cluster.wait_output(out_path, "PROBE_OK")
        parsed = _parse_probe(content)
        assert parsed.get("VISIBLE_COUNT") == "2", content
        assert parsed.get("SPUR_COUNT") == "2", content
        assert parsed.get("KFD") == "yes", f"expected KFD=yes in container\n{content}"
        assert parsed.get("RENDER_COUNT") == "2", (
            f"expected RENDER_COUNT=2 in container\n{content}"
        )




class TestMultiNodeSpread:
    def test_multitask_per_node_gpu_partition(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(1)
        if _max_gpus_per_node(cluster) < 4:
            pytest.skip("need >= 4 GPUs on one node")

        probe = _probe_script(cluster)
        out_path = f"{cluster.remote_dir}/mtask.out"
        sb = cluster.sbatch(
            [
                "-J",
                "mtask",
                "-N",
                "1",
                "--ntasks-per-node=4",
                "--gres=gpu:4",
                "-o",
                out_path,
                probe,
            ]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        content = cluster.wait_output(out_path, "PROBE_OK")
        assert content.count("PROBE_OK") >= 4, content
        rocr_lines = [
            ln.split("=", 1)[1]
            for ln in content.splitlines()
            if ln.startswith("ROCR_VISIBLE_DEVICES=")
        ]
        assert len(rocr_lines) >= 4, content
        assert len(set(rocr_lines)) >= 4, f"expected distinct per-task ROCR values\n{content}"

    def test_two_node_gres_spread(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(2)
        cluster.require_nodes(2)

        probe = _probe_script(cluster)
        out_path = f"{cluster.remote_dir}/2n-spread.out"
        sb = cluster.sbatch(
            ["-J", "2n-spread", "-N", "2", "--gres=gpu:1", "-o", out_path, probe]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=180)
        content = cluster.read_output_all_nodes(out_path)
        assert content.count("PROBE_OK") >= 2, content

    def test_four_node_gres_spread(self, gpu_cluster):
        cluster = gpu_cluster
        cluster.gpu_preflight(4)
        cluster.require_nodes(4)

        probe = _probe_script(cluster)
        out_path = f"{cluster.remote_dir}/4n-spread.out"
        sb = cluster.sbatch(
            ["-J", "4n-spread", "-N", "4", "--gres=gpu:1", "-o", out_path, probe]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None
        wait_job(cluster, job_id, timeout=300)
        content = cluster.read_output_all_nodes(out_path)
        assert content.count("PROBE_OK") >= 4, content



class TestConfigModes:
    def test_config_autodetect_default(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.gpu_preflight(1)
        cluster.start(SpurCluster.devices_config(auto_detect=True))
        cluster.assert_spurd_registry(0, min_gpus=1)

    def test_config_on_disk_cdi_spec(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.gpu_preflight(1)

        cdi_dir = f"{cluster.remote_dir}/cdi-specs"
        cluster.nodes[0].exec(f"mkdir -p '{cdi_dir}'")
        # Minimal on-disk CDI spec: auto_detect must be ignored when cache is non-empty.
        spec = """{
  "cdiVersion": "0.6.0",
  "kind": "amd.com/gpu",
  "devices": [{"name": "0", "containerEdits": {"deviceNodes": [{"path": "/dev/dri/renderD128"}]}}]
}"""
        cluster.nodes[0].write_file(f"{cdi_dir}/amd-gpu.json", spec)
        cluster.start(
            SpurCluster.devices_config(auto_detect=True, cdi_spec_dirs=[cdi_dir])
        )
        log = cluster.spurd_log(0)
        assert "device registry initialized" in log, log[-2000:]
        # On-disk spec registers at least one injectable GPU.
        assert cluster.spurd_registry_gpu_count(0) is not None

    def test_config_gres_file_fallback(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.gpu_preflight(1)

        # Discover render nodes present on the node for a minimal GRES file entry.
        render_probe = cluster.nodes[0].exec_allow_fail(
            "ls /dev/dri/renderD* 2>/dev/null | head -2 | xargs -I{} basename {} | "
            "paste -sd, - || true"
        )
        if not render_probe.strip():
            pytest.skip("no renderD devices for GRES file fallback test")

        first = render_probe.strip().split(",")[0]
        minor = re.search(r"renderD(\d+)", first)
        if not minor:
            pytest.skip("could not parse renderD minor")

        render_minor = minor.group(1)
        cluster.start(
            {
                "devices": {
                    "auto_detect": False,
                    "gres": [
                        {
                            "name": "gpu",
                            "file": f"/dev/dri/renderD{render_minor}",
                            "flags": ["amd_gpu_env"],
                        }
                    ],
                }
            }
        )
        cluster.assert_spurd_registry(0, min_gpus=1)
        assert cluster.node_gpu_count(cluster.node_names[0]) >= 1
