# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Operator health, readiness, and metrics endpoints.

Kubernetes drives the operator's restart and endpoint membership entirely from
these three routes, so their exact status codes matter more than their bodies:
a /healthz that never fails means a wedged operator is never restarted, and a
/readyz that never fails means traffic keeps flowing to one that cannot reach
spurctld.
"""

from k8s_cluster import assert_eventually, service_http_get
import pytest

pytestmark = pytest.mark.suite_k8s_core

HEALTH_PORT = "health"
OPERATOR_SERVICE = "spur-k8s-operator"


def health_get(cluster, path: str) -> tuple[int, str]:
    return service_http_get(cluster.namespace, OPERATOR_SERVICE, HEALTH_PORT, path)


class TestLiveness:
    def test_healthz_is_ok(self, cluster):
        status, body = health_get(cluster, "/healthz")
        assert status == 200, body
        assert body.strip() == "ok"

    def test_healthz_does_not_depend_on_spurctld(self, cluster):
        """Liveness must stay green while dependencies are down, otherwise a
        controller outage turns into an operator restart loop."""
        cluster.apps_v1.patch_namespaced_stateful_set_scale(
            "spurctld", cluster.namespace, {"spec": {"replicas": 0}}
        )
        try:
            assert_eventually(
                120,
                5,
                "readiness never dropped after scaling spurctld to zero",
                lambda: health_get(cluster, "/readyz")[0] == 503,
            )
            status, body = health_get(cluster, "/healthz")
            assert status == 200, body
        finally:
            cluster.apps_v1.patch_namespaced_stateful_set_scale(
                "spurctld", cluster.namespace, {"spec": {"replicas": 1}}
            )
            cluster.ensure_controllers_ready(timeout=180)

    def test_unknown_paths_are_not_served(self, cluster):
        status, _ = health_get(cluster, "/does-not-exist")
        assert status == 404


class TestReadiness:
    def test_readyz_is_ok_when_the_cluster_is_healthy(self, cluster):
        status, body = health_get(cluster, "/readyz")
        assert status == 200, body
        assert body.strip() == "ok"

    def test_readyz_reports_spurctld_as_the_reason_when_it_is_gone(self, cluster):
        """The body names the failing dependency, which is the only signal an
        operator gets from a probe failure."""
        cluster.apps_v1.patch_namespaced_stateful_set_scale(
            "spurctld", cluster.namespace, {"spec": {"replicas": 0}}
        )
        try:
            reasons: list[str] = []

            def unavailable() -> bool:
                status, body = health_get(cluster, "/readyz")
                if status == 503:
                    reasons.append(body)
                    return True
                return False

            assert_eventually(
                120, 5, "readyz stayed green with spurctld scaled to zero", unavailable
            )
            assert "spurctld-unreachable" in reasons[-1], reasons[-1]
        finally:
            cluster.apps_v1.patch_namespaced_stateful_set_scale(
                "spurctld", cluster.namespace, {"spec": {"replicas": 1}}
            )
            cluster.ensure_controllers_ready(timeout=180)

    def test_readyz_recovers_once_spurctld_returns(self, cluster):
        cluster.ensure_controllers_ready(timeout=180)
        assert_eventually(
            120,
            5,
            "readyz did not recover after spurctld came back",
            lambda: health_get(cluster, "/readyz")[0] == 200,
        )

    def test_the_operator_endpoint_is_in_service(self, cluster):
        """A failing readiness probe would strip the pod from the Service, so
        reaching it through the Service at all proves the probe is passing."""
        endpoints = cluster.core_v1.read_namespaced_endpoints(
            OPERATOR_SERVICE, cluster.namespace
        )
        addresses = [a for s in (endpoints.subsets or []) for a in (s.addresses or [])]
        assert addresses, "operator Service has no ready endpoints"


class TestMetrics:
    def test_metrics_returns_prometheus_text(self, cluster):
        status, body = health_get(cluster, "/metrics")
        assert status == 200, body
        assert "# TYPE spur_k8s_operator_up gauge" in body
        assert "spur_k8s_operator_up 1" in body

    def test_metrics_carries_a_timestamp_gauge(self, cluster):
        _, body = health_get(cluster, "/metrics")
        line = next(
            (
                ln
                for ln in body.splitlines()
                if ln.startswith("spur_k8s_operator_timestamp_seconds ")
            ),
            None,
        )
        assert line is not None, body
        assert float(line.split()[1]) > 0

    def test_every_metric_has_a_help_and_type_line(self, cluster):
        """Prometheus tolerates missing metadata but scrapers and dashboards
        key off it, so a bare sample line is a regression."""
        _, body = health_get(cluster, "/metrics")
        samples = {
            ln.split()[0]
            for ln in body.splitlines()
            if ln and not ln.startswith("#")
        }
        assert samples
        for name in samples:
            assert f"# HELP {name} " in body, f"{name} has no HELP line"
            assert f"# TYPE {name} " in body, f"{name} has no TYPE line"

    def test_metrics_is_stable_across_scrapes(self, cluster):
        first = health_get(cluster, "/metrics")[1]
        second = health_get(cluster, "/metrics")[1]
        assert "spur_k8s_operator_up 1" in first
        assert "spur_k8s_operator_up 1" in second


class TestProbeConfiguration:
    def test_the_deployment_probes_point_at_the_health_port(self, cluster):
        """A probe aimed at the wrong port silently disables restart and
        endpoint management, and nothing else in the suite would catch it."""
        dep = cluster.apps_v1.read_namespaced_deployment(
            "spur-k8s-operator", cluster.namespace
        )
        container = next(
            c for c in dep.spec.template.spec.containers if c.name == "operator"
        )
        assert container.liveness_probe.http_get.path == "/healthz"
        assert container.readiness_probe.http_get.path == "/readyz"

    def test_the_operator_recovers_readiness_after_a_restart(self, cluster):
        cluster.restart_operator()
        assert_eventually(
            120,
            5,
            "operator did not become ready again after a restart",
            lambda: health_get(cluster, "/readyz")[0] == 200,
        )
