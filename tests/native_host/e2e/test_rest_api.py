# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the REST API surface."""

import pytest


# Enable the REST API (off by default) and bind it to loopback only.
# Tests hit it via curl from the controller node itself.
_REST_CONFIG = {
    "rest_api": {"enabled": True},
    "controller": {"rest_addr": "127.0.0.1:6820"},
}

REST_URL = "http://127.0.0.1:6820"


@pytest.fixture
def cluster_config_overrides():
    return _REST_CONFIG


class TestRestApiSecurity:
    def test_submit_without_user_returns_400(self, cluster):
        """POST /job/submit with no user field must fail fast with 400.

        Without the guard, Default::default() sets uid: 0 on the JobSpec.
        The job would be accepted and queued, the agent would refuse it
        asynchronously, and the HTTP caller would never see an error.
        """
        node = cluster.nodes[0]
        out = node.exec(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"-H 'Content-Type: application/json' "
            f"-d '{{\"job\":{{\"script\":\"#!/bin/bash\\necho hi\\n\"}}}}' "
            f"{REST_URL}/slurm/v0.0.42/job/submit"
        )
        assert out.strip() == "400", (
            f"expected 400 for missing user field, got {out.strip()!r}"
        )

    def test_submit_without_user_error_message(self, cluster):
        """The 400 response body must name the missing field so the caller knows what to fix."""
        node = cluster.nodes[0]
        out = node.exec(
            f"curl -s -X POST "
            f"-H 'Content-Type: application/json' "
            f"-d '{{\"job\":{{\"script\":\"#!/bin/bash\\necho hi\\n\"}}}}' "
            f"{REST_URL}/slurm/v0.0.42/job/submit"
        )
        assert "job.user" in out, (
            f"expected error mentioning 'job.user', got: {out!r}"
        )

    def test_submit_with_user_is_accepted(self, cluster):
        """A well-formed submission with a user field must reach the queue (not be rejected at the REST layer)."""
        node = cluster.nodes[0]
        out = node.exec(
            f"curl -s -w '\\n%{{http_code}}' -X POST "
            f"-H 'Content-Type: application/json' "
            f"-d '{{\"job\":{{\"script\":\"#!/bin/bash\\ntrue\\n\",\"user\":\"root\"}}}}' "
            f"{REST_URL}/slurm/v0.0.42/job/submit"
        )
        # Split body and status code
        *body_lines, status = out.strip().splitlines()
        assert status == "200", (
            f"expected 200 for valid submission, got {status!r}\nbody: {''.join(body_lines)}"
        )
