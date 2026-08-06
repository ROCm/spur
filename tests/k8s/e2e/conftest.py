# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Pytest fixtures for Spur Kubernetes E2E tests."""

import pytest

from k8s_cluster import (
    ClusterFixture,
    FixtureConfig,
    SuiteContext,
    assert_leader_elected,
)


@pytest.fixture(scope="session")
def k8s_suite():
    suite = SuiteContext.setup()
    yield suite
    suite.teardown()


@pytest.fixture(scope="class")
def cluster(k8s_suite):
    c = ClusterFixture.deploy(k8s_suite, FixtureConfig.single_node())
    yield c
    c.teardown_workloads()


@pytest.fixture(scope="class")
def ha_cluster(k8s_suite):
    c = ClusterFixture.deploy(k8s_suite, FixtureConfig.raft_ha())
    yield c
    c.teardown_workloads()


@pytest.fixture(scope="class")
def quota_cluster(k8s_suite):
    """Operator deployed with --enable-quota for the quota projection tests."""
    c = ClusterFixture.deploy(k8s_suite, FixtureConfig.with_quota())
    yield c
    c.teardown_workloads()


@pytest.fixture(autouse=True)
def _cleanup_between_tests(request):
    yield
    for name in ("cluster", "quota_cluster"):
        if name in request.fixturenames:
            request.getfixturevalue(name).cleanup_test_workloads()
            return
    if "ha_cluster" in request.fixturenames:
        fixture = request.getfixturevalue("ha_cluster")
        fixture.cleanup_test_workloads()
        fixture.ensure_controllers_ready()
        if fixture.config.replicas > 1:
            assert_leader_elected(fixture.namespace, fixture.config.replicas)
