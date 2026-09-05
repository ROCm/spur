# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Container E2E tests for the Spur scheduler.

Tests rootless container execution: launch, exit codes, cancel, DNS,
/dev/shm, PID namespace, env vars, bind mounts, and multi-node containers.
"""

import time

import pytest

from cluster import parse_job_id, job_state, wait_job


@pytest.fixture
def container_cluster(cluster, tmp_path):
    """
    Single-node container cluster fixture.
    Runs container_preflight and builds the test image.
    Attaches `container_image` attribute to the cluster.
    """
    cluster.container_preflight()
    cluster.container_image = cluster.build_container_image(tmp_path)
    return cluster


@pytest.fixture
def multi_container_cluster(multi_node_cluster, tmp_path):
    """
    Multi-node container cluster fixture.
    Runs container_preflight and builds the test image.
    """
    multi_node_cluster.container_preflight()
    multi_node_cluster.container_image = multi_node_cluster.build_container_image(tmp_path)
    return multi_node_cluster


@pytest.fixture
def imported_image_cluster(cluster, tmp_path):
    """
    Single-node cluster whose image went through `spur image import`, so the
    image config recorded at import time is present when a job launches.
    """
    cluster.container_preflight()
    cluster.container_image = cluster.build_imported_container_image(
        tmp_path,
        image_env=[
            f"PATH={cluster.IMAGE_ONLY_BIN_DIR}:/usr/bin",
            "IMAGE_MARKER=from-image",
        ],
    )
    return cluster


@pytest.fixture
def registry_image_cluster(cluster):
    """
    Single-node cluster running a real public image pulled from its registry,
    so the config recorded by the pull path is what seeds the container.
    """
    cluster.container_preflight()
    cluster.container_image = cluster.pull_container_image()
    return cluster


class TestContainerSingleNode:
    def test_container_launch_and_exit(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file(
            "c1.sh",
            "#!/bin/bash\nhostname >/dev/null || exit 1\nid >/dev/null || exit 1\n",
        )
        sb = cluster.sbatch(["-J", "c1-launch", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), f"container job must complete, got {state}\n{diag}"

    def test_container_exit_code_propagation(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file("c2.sh", "#!/bin/bash\nexit 42\n")

        sb = cluster.sbatch(["-J", "c2-exit", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        assert state == "F", f"exit 42 should mark job failed, got {state}"

    def test_container_cancel(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file("c3.sh", "#!/bin/bash\nsleep 3600\n")

        sb = cluster.sbatch(["-J", "c3-cancel", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        # Wait for it to start running
        for _ in range(15):
            sq = cluster.squeue_all()
            if job_state(sq, job_id) == "R":
                break
            time.sleep(1)

        cluster.scancel(str(job_id))
        time.sleep(3)

        state = wait_job(cluster, job_id, timeout=30)
        assert state in ("CA", "F", "GONE"), (
            f"cancelled container job should be CA/F, got {state}"
        )

    def test_container_dns_resolution(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/c4.out"
        script = cluster.write_file(
            "c4.sh",
            "#!/bin/bash\n"
            "grep -q '127.0.0.53' /etc/resolv.conf && exit 1\n"
            "getent hosts google.com >/dev/null 2>&1 || exit 2\n"
            "echo DNS_OK\n",
        )
        sb = cluster.sbatch([
            "-J", "c4-dns", "-N", "1", "-o", out_path,
            f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"DNS test failed (1=loopback in resolv.conf, 2=getent failed), "
            f"state={state}\n{diag}"
        )

    def test_container_dev_shm(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file(
            "c5.sh",
            "#!/bin/bash\n"
            "echo shm_test > /dev/shm/spur_ctest || exit 1\n"
            "rm -f /dev/shm/spur_ctest\n"
            "df /dev/shm >/dev/null 2>&1 || exit 2\n",
        )
        sb = cluster.sbatch(["-J", "c5-shm", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"/dev/shm test failed (1=write failed, 2=not mounted), state={state}\n{diag}"
        )

    def test_container_pid_namespace(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file(
            "c6.sh",
            '#!/bin/bash\n[ "$$" = "1" ] || exit 1\n[ -r /proc/self/status ] || exit 2\n',
        )
        sb = cluster.sbatch(["-J", "c6-pidns", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"PID namespace test failed (1=not PID 1, 2=/proc missing), state={state}\n{diag}"
        )

    def test_container_env_vars(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image
        script = cluster.write_file(
            "c7.sh",
            '#!/bin/bash\n[ -n "$SPUR_JOB_ID" ] || exit 1\n[ -n "$OMP_NUM_THREADS" ] || exit 2\n',
        )
        sb = cluster.sbatch(["-J", "c7-env", "-N", "1", f"--container-image={img}", script])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"env var test failed (1=no SPUR_JOB_ID, 2=no OMP_NUM_THREADS), "
            f"state={state}\n{diag}"
        )

    def test_container_bind_mount_readonly(self, container_cluster):
        cluster = container_cluster
        img = cluster.container_image

        bind_dir = f"{cluster.remote_dir}/bind-test"
        for node in cluster.nodes:
            node.exec(f"mkdir -p '{bind_dir}' && echo bind_mount_ci_test > '{bind_dir}/data.txt'")

        script = cluster.write_file(
            "c8.sh",
            "#!/bin/bash\n"
            '[ "$(cat /mnt/data/data.txt 2>/dev/null)" = "bind_mount_ci_test" ] || exit 1\n'
            "touch /mnt/data/write_test 2>/dev/null && exit 2\n"
            "exit 0\n",
        )
        mount_spec = f"{bind_dir}:/mnt/data:ro"
        sb = cluster.sbatch([
            "-J", "c8-bind", "-N", "1",
            f"--container-image={img}",
            f"--container-mounts={mount_spec}",
            script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"bind mount test failed (1=content wrong, 2=ro not enforced), "
            f"state={state}\n{diag}"
        )

    def test_container_starts_from_image_env(self, imported_image_cluster):
        """A binary on the image's own PATH must be runnable, host PATH kept."""
        cluster = imported_image_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/c10-image-env.out"
        script = cluster.write_file(
            "c10-image-env.sh",
            "#!/bin/bash\n"
            f"{cluster.IMAGE_PAYLOAD} {cluster.IMAGE_PAYLOAD_MARKER} || exit 1\n"
            'echo "CONTAINER_PATH=$PATH"\n'
            'echo "MARKER=$IMAGE_MARKER"\n',
        )
        # The import ran on node 0, so the .sqsh exists only there.
        sb = cluster.sbatch([
            "-J", "c10-image-env", "-N", "1", "-w", cluster.node_names[0],
            "-o", out_path, f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=120)
        output = cluster.read_output_all_nodes(out_path)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"job running the image's own binary must complete, got {state}\n"
            f"{diag}\noutput:\n{output}"
        )
        assert cluster.IMAGE_PAYLOAD_MARKER in output, (
            f"{cluster.IMAGE_PAYLOAD} is only on the image's PATH and must "
            f"still resolve\noutput:\n{output}"
        )
        assert f"CONTAINER_PATH={cluster.IMAGE_ONLY_BIN_DIR}:" in output, (
            f"image PATH entries must come first\noutput:\n{output}"
        )
        assert f"{cluster.bin_dir}" in output, (
            f"job PATH entries must still be appended\noutput:\n{output}"
        )
        assert "MARKER=from-image" in output, (
            f"non-PATH image variables must reach the container\noutput:\n{output}"
        )

    def test_container_without_image_config_keeps_job_env(self, container_cluster):
        """An image with no recorded config must run on the job env, unchanged."""
        cluster = container_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/c11-no-image-config.out"
        script = cluster.write_file(
            "c11-no-image-config.sh",
            "#!/bin/bash\n"
            "[ -e /etc/spur/image-config.json ] && exit 1\n"
            'echo "CONTAINER_PATH=$PATH"\n'
            "echo NO_IMAGE_CONFIG_OK\n",
        )
        sb = cluster.sbatch([
            "-J", "c11-no-cfg", "-N", "1", "-o", out_path,
            f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        output = cluster.read_output_all_nodes(out_path)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"image without a recorded config must still run (1=config present), "
            f"got {state}\n{diag}\noutput:\n{output}"
        )
        assert "NO_IMAGE_CONFIG_OK" in output, f"job did not run:\n{output}"
        assert f"CONTAINER_PATH={cluster.bin_dir}:" in output, (
            f"job PATH must pass through untouched\noutput:\n{output}"
        )

    def test_container_starts_from_pulled_image_env(self, registry_image_cluster):
        """A real registry image must run on its own config.Env."""
        cluster = registry_image_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/c12-pulled-image.out"
        script = cluster.write_file(
            "c12-pulled-image.sh",
            "#!/bin/bash\n"
            "python3 -c 'print(\"PULLED_IMAGE_OK\")' || exit 1\n"
            'echo "CONTAINER_PATH=$PATH"\n'
            'echo "PY=$PYTHON_VERSION"\n',
        )
        # The import ran on node 0, so the .sqsh exists only there.
        sb = cluster.sbatch([
            "-J", "c12-pulled", "-N", "1", "-w", cluster.node_names[0],
            "-o", out_path, f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=120)
        output = cluster.read_output_all_nodes(out_path)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), (
            f"job in a pulled image must complete, got {state}\n{diag}\noutput:\n{output}"
        )
        assert "PULLED_IMAGE_OK" in output, f"the image's python did not run:\n{output}"
        assert f"CONTAINER_PATH={cluster.TEST_IMAGE_PATH_HEAD}:" in output, (
            f"the image's PATH must come first\noutput:\n{output}"
        )
        assert cluster.bin_dir in output, (
            f"job PATH entries must still be appended\noutput:\n{output}"
        )
        # Set only in the image config, so it is empty without the fix. LANG is
        # not usable here: the image sets it too, but the job environment has its
        # own and job values win for everything except the search paths.
        assert "PY=3.11" in output, f"PYTHON_VERSION must reach the job\noutput:\n{output}"


class TestContainerMultiNode:
    def test_two_node_container_job(self, multi_container_cluster):
        cluster = multi_container_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/ct-2n.out"
        script = cluster.write_file(
            "ct-2n.sh",
            "#!/bin/bash\n"
            'echo "CONTAINER_NODE=$(hostname)"\n'
            'echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"\n'
            'echo "SPUR_NNODES=${SPUR_NNODES}"\n'
            "echo CONTAINER_2N_OK\n",
        )
        sb = cluster.sbatch([
            "-J", "ct-2node", "-N", "2", "-o", out_path,
            f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        diag = cluster.debug_job(job_id)
        assert "CONTAINER_2N_OK" in all_output, (
            f"2-node container job must report CONTAINER_2N_OK\n{diag}\noutput:\n{all_output}"
        )
        assert "SPUR_NNODES=2" in all_output, f"must see SPUR_NNODES=2\noutput:\n{all_output}"

    def test_two_node_container_env_vars(self, multi_container_cluster):
        # Spur exposes allocation topology through SPUR_* inside containers; it
        # does not inject the PyTorch rendezvous vars (matching Slurm).
        cluster = multi_container_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/ct-2n-env.out"
        script = cluster.write_file(
            "ct-2n-env.sh",
            "#!/bin/bash\n"
            'echo "SPUR_NNODES=${SPUR_NNODES}"\n'
            'echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"\n'
            'echo "SPUR_JOB_ID=${SPUR_JOB_ID}"\n'
            'echo "TORCH_RANK=[${RANK}]"\n'
            'echo "TORCH_WORLD_SIZE=[${WORLD_SIZE}]"\n'
            'echo "TORCH_MASTER_ADDR=[${MASTER_ADDR}]"\n'
            "echo CT_ENV_OK\n",
        )
        sb = cluster.sbatch([
            "-J", "ct-2n-env", "-N", "2", "-o", out_path,
            f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        wait_job(cluster, job_id, timeout=90)
        all_output = cluster.read_output_all_nodes(out_path)
        assert "CT_ENV_OK" in all_output, f"missing CT_ENV_OK:\n{all_output}"
        assert "SPUR_NNODES=2" in all_output, f"missing SPUR_NNODES=2:\n{all_output}"
        assert "SPUR_NODE_RANK=0" in all_output, f"missing rank 0:\n{all_output}"
        assert "SPUR_NODE_RANK=1" in all_output, f"missing rank 1:\n{all_output}"
        # The torch names must be empty inside the container too. Spur no
        # longer injects them (matching Slurm).
        assert "TORCH_RANK=[]" in all_output, f"RANK should be unset:\n{all_output}"
        assert "TORCH_WORLD_SIZE=[]" in all_output, (
            f"WORLD_SIZE should be unset:\n{all_output}"
        )
        assert "TORCH_MASTER_ADDR=[]" in all_output, (
            f"MASTER_ADDR should be unset:\n{all_output}"
        )

    def test_two_node_container_dns(self, multi_container_cluster):
        cluster = multi_container_cluster
        img = cluster.container_image
        out_path = f"{cluster.remote_dir}/ct-2n-dns.out"
        script = cluster.write_file(
            "ct-2n-dns.sh",
            "#!/bin/bash\n"
            "getent hosts google.com >/dev/null 2>&1 || exit 1\n"
            'echo "DNS_OK_$(hostname)"\n',
        )
        sb = cluster.sbatch([
            "-J", "ct-2n-dns", "-N", "2", "-o", out_path,
            f"--container-image={img}", script,
        ])
        job_id = parse_job_id(sb)
        assert job_id is not None

        state = wait_job(cluster, job_id, timeout=60)
        diag = cluster.debug_job(job_id)
        assert state in ("CD", "GONE"), f"2-node container DNS failed, state={state}\n{diag}"
