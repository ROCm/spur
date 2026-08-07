# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `spur image`.

Image management is entirely local: no controller RPC, just squashfs files in a
directory. Every test points `SPUR_IMAGE_DIR` at a scratch path so it never
touches `/var/spool/spur/images` or the invoking user's `~/.spur/images`.

Registry pulls are not exercised — they need outbound network access that a
test cluster cannot be assumed to have.
"""

import time

import pytest

from cluster import wait_job, parse_job_id

IMAGE_NAME = "demo"
# Images are stored under the canonical OCI reference, so a bare name lands as
# docker.io/library/<name>:latest with `/` and `:` folded to `+`. The CLI is
# still driven with the bare name, which is what a user types.
IMAGE_STEM = "docker.io+library+demo+latest"


@pytest.fixture
def image_dir(cluster):
    """A scratch image directory, created and torn down per test."""
    cluster.image_preflight()
    path = f"{cluster.remote_dir}/images"
    cluster.nodes[0].exec(f"mkdir -p '{path}'")
    yield path
    cluster.nodes[0].exec_allow_fail(f"rm -rf '{path}'")


def image_cli(cluster, image_dir: str, args: list[str]) -> tuple[int, str]:
    return cluster.cli_with_env(["spur", "image"] + args, {"SPUR_IMAGE_DIR": image_dir})


class TestListing:
    def test_an_empty_directory_reports_no_images(self, cluster, image_dir):
        code, out = image_cli(cluster, image_dir, ["list"])
        assert code == 0, out
        assert "No images imported yet." in out, out

    def test_an_imported_image_is_listed(self, cluster, image_dir, tmp_path):
        """`import` is not the only way an image lands in the directory, and
        `list` has to reflect whatever is actually there."""
        _place_image(cluster, image_dir, tmp_path)
        code, out = image_cli(cluster, image_dir, ["list"])
        assert code == 0, out
        assert IMAGE_NAME in out, out

    def test_the_listing_has_a_header(self, cluster, image_dir, tmp_path):
        _place_image(cluster, image_dir, tmp_path)
        _, out = image_cli(cluster, image_dir, ["list"])
        assert "IMAGE" in out and "SIZE" in out, out

    def test_the_listing_reports_a_size(self, cluster, image_dir, tmp_path):
        _place_image(cluster, image_dir, tmp_path)
        _, out = image_cli(cluster, image_dir, ["list"])
        row = next((ln for ln in out.splitlines() if IMAGE_NAME in ln), "")
        assert "MB" in row or "GB" in row, f"no size on the image row: {out}"


class TestRemoval:
    def test_removing_a_missing_image_fails(self, cluster, image_dir):
        code, out = image_cli(cluster, image_dir, ["remove", "nosuchimage"])
        assert code != 0, out
        assert "not found" in out, out

    def test_removing_an_image_deletes_it(self, cluster, image_dir, tmp_path):
        _place_image(cluster, image_dir, tmp_path)
        code, out = image_cli(cluster, image_dir, ["remove", IMAGE_NAME])
        assert code == 0, out
        assert "Removed" in out, out

        _, listing = image_cli(cluster, image_dir, ["list"])
        assert "No images imported yet." in listing, listing

    def test_removing_twice_fails_the_second_time(self, cluster, image_dir, tmp_path):
        _place_image(cluster, image_dir, tmp_path)
        assert image_cli(cluster, image_dir, ["remove", IMAGE_NAME])[0] == 0
        code, out = image_cli(cluster, image_dir, ["remove", IMAGE_NAME])
        assert code != 0, out
        assert "not found" in out, out


class TestExport:
    def test_exporting_an_unknown_container_fails(self, cluster, image_dir):
        """`export` works on named running containers, not on images; the error
        has to say so or users will read it as a missing-image message."""
        code, out = image_cli(cluster, image_dir, ["export", "nosuchcontainer"])
        assert code != 0, out
        assert "not found" in out, out
        assert "--container-name" in out, out


class TestImageDirectorySelection:
    def test_the_env_var_selects_the_directory(self, cluster, image_dir, tmp_path):
        """Two directories must stay independent, otherwise a shared default
        would leak images between users."""
        other = f"{cluster.remote_dir}/images-other"
        cluster.nodes[0].exec(f"mkdir -p '{other}'")
        try:
            _place_image(cluster, image_dir, tmp_path)
            _, listing = image_cli(cluster, other, ["list"])
            assert "No images imported yet." in listing, listing
        finally:
            cluster.nodes[0].exec_allow_fail(f"rm -rf '{other}'")


class TestJobIntegration:
    def test_a_job_runs_against_an_imported_image(self, cluster, tmp_path):
        """The payoff for import: a job can name the image and get its
        filesystem."""
        cluster.container_preflight()
        remote_image = cluster.build_container_image(tmp_path)

        out_path = f"{cluster.remote_dir}/image-job.out"
        script = cluster.write_file(
            "image-job.sh", "#!/bin/bash\necho IMAGE_JOB_OK\n"
        )
        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J",
                    "image-job",
                    f"--container-image={remote_image}",
                    "-o",
                    out_path,
                    script,
                ]
            )
        )
        assert job_id is not None
        assert wait_job(cluster, job_id, timeout=180) in ("CD", "GONE"), (
            cluster.debug_job(job_id)
        )
        assert "IMAGE_JOB_OK" in cluster.read_output_on_any_node(out_path)

    def test_an_unknown_image_keeps_the_job_out_of_running(self, cluster):
        """A missing image is not terminal: the launch fails, the job returns
        to pending as JobLaunchFailure and is retried. What must hold is that
        it never runs and that the reason says why.
        """
        script = cluster.write_file("image-missing.sh", "#!/bin/bash\ntrue\n")
        job_id = parse_job_id(
            cluster.sbatch(
                [
                    "-J",
                    "image-missing",
                    "--container-image=/nonexistent/nope.sqsh",
                    script,
                ]
            )
        )
        assert job_id is not None
        try:
            deadline = time.time() + 60
            while time.time() < deadline:
                detail = cluster.scontrol("show", "job", str(job_id))
                if "Reason=JobLaunchFailure" in detail:
                    break
                assert "JobState=RUNNING" not in detail, (
                    f"a job with a missing image must not run:\n{detail}"
                )
                time.sleep(3)
            else:
                raise AssertionError(
                    f"job {job_id} never reported a launch failure:\n"
                    f"{cluster.scontrol('show', 'job', str(job_id))}"
                )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


def _place_image(cluster, image_dir: str, tmp_path) -> str:
    """Put a real squashfs image in *image_dir* under IMAGE_STEM.

    `spur image import` needs a container runtime or a registry, neither of
    which a test cluster is guaranteed to have, so the file is built the same
    way the container tests build theirs and dropped in directly.
    """
    cluster.container_preflight()
    built = cluster.build_container_image(tmp_path)
    target = f"{image_dir}/{IMAGE_STEM}.sqsh"
    cluster.nodes[0].exec(f"cp '{built}' '{target}'")
    return target
