# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Pytest fixtures for Spur Kubernetes E2E tests."""

import pytest

from k8s_cluster import (
    JOB_IMAGE,
    ClusterFixture,
    FixtureConfig,
    SuiteContext,
    assert_leader_elected,
    core_v1_api,
    prepull_image,
)
from k8s_diagnostics import failure_sections, register_namespaces, registered_namespaces, wants_diagnostics


@pytest.fixture(scope="session")
def k8s_suite():
    suite = SuiteContext.setup()
    try:
        register_namespaces(suite.namespace)
        try:
            prepull_image(suite.core_v1, suite.namespace, JOB_IMAGE)
        except (RuntimeError, TimeoutError) as exc:
            # A fixture error would repeat for every test; one clear stop is enough.
            pytest.exit(f"session setup failed, could not pre-pull {JOB_IMAGE}: {exc}", returncode=1)
        yield suite
    finally:
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


@pytest.fixture(autouse=True)
def _cleanup_between_tests(request):
    yield
    if "cluster" in request.fixturenames:
        request.getfixturevalue("cluster").cleanup_test_workloads()
    elif "ha_cluster" in request.fixturenames:
        fixture = request.getfixturevalue("ha_cluster")
        fixture.cleanup_test_workloads()
        fixture.ensure_controllers_ready()
        if fixture.config.replicas > 1:
            assert_leader_elected(fixture.namespace, fixture.config.replicas)


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    report = outcome.get_result()
    if not wants_diagnostics(report.when, report.failed):
        return
    # A failing test must keep its own failure, so a broken collector is shown as
    # a section instead of raised.
    try:
        report.sections.extend(
            failure_sections(report.when, report.failed, registered_namespaces(), core_v1_api)
        )
    except Exception as err:
        report.sections.append(("diagnostics", f"collection failed: {type(err).__name__}: {err}"))
