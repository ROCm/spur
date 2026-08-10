# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the spurctld REST API.

The REST server is on by default (`rest_api.enabled`, `[::]:6820`), so these
need no config override. Requests are issued with curl over SSH on the
controller node, which keeps them independent of the pytest runner's network
reachability.

The API is served under two prefixes -- `/api/v1` and the Slurm-compatible
`/slurm/v0.0.42` -- and both must expose the same routes.
"""

import json
import time

import pytest

from cluster import REST_PORT, job_state, wait_job_state

pytestmark = pytest.mark.suite_api

PREFIXES = ["/api/v1", "/slurm/v0.0.42"]


def _json(status: int, body: str, path: str) -> dict:
    assert status == 200, f"GET {path} returned {status}:\n{body}"
    try:
        return json.loads(body)
    except json.JSONDecodeError as exc:
        raise AssertionError(f"GET {path} returned non-JSON:\n{body}") from exc


def _get_json(cluster, path: str) -> dict:
    status, body = cluster.http_get(path)
    return _json(status, body, path)


def _submit(cluster, name: str, script: str, **fields) -> tuple[int, str]:
    payload = {"job": {"name": name, "script": script, "nodes": 1, "ntasks": 1, **fields}}
    return cluster.http_post("/api/v1/job/submit", json.dumps(payload))


@pytest.fixture
def rest_cluster(cluster):
    cluster.curl_preflight()
    status, body = cluster.http_get("/api/v1/ping")
    if status == 0:
        pytest.skip(f"REST API not reachable on port {REST_PORT}: {body.strip()}")
    return cluster


class TestPing:
    def test_ping_reports_up_and_primary(self, rest_cluster):
        data = _get_json(rest_cluster, "/api/v1/ping")
        pings = data["ping"]
        assert pings, f"ping returned no entries: {data}"
        assert pings[0]["pinged"] == "UP", f"unexpected ping payload: {pings[0]}"
        assert pings[0]["mode"] == "primary", (
            f"a single-controller cluster is always the leader: {pings[0]}"
        )

    @pytest.mark.parametrize("prefix", PREFIXES)
    def test_both_prefixes_serve_ping(self, rest_cluster, prefix):
        data = _get_json(rest_cluster, f"{prefix}/ping")
        assert data["ping"][0]["pinged"] == "UP"

    def test_response_carries_slurm_version_meta(self, rest_cluster):
        data = _get_json(rest_cluster, "/api/v1/ping")
        version = data["meta"]["Slurm"]["version"]
        assert version == {"major": 0, "minor": 0, "micro": 42}, (
            f"unexpected version meta: {data['meta']}"
        )


class TestJobRoutes:
    def test_submit_get_and_cancel(self, rest_cluster):
        status, body = _submit(
            rest_cluster, "rest-lifecycle", "#!/bin/bash\nsleep 120\n"
        )
        assert status == 200, f"submit returned {status}:\n{body}"
        job_id = json.loads(body)["job_id"]
        assert job_id > 0, f"submit returned no job id: {body}"

        wait_job_state(rest_cluster, job_id, "R", timeout=90)

        data = _get_json(rest_cluster, f"/api/v1/job/{job_id}")
        assert data["jobs"][0]["job_id"] == job_id
        assert data["jobs"][0]["job_state"] == "RUNNING", (
            f"REST job state must track the running job: {data['jobs'][0]}"
        )

        status, body = rest_cluster.http_delete(f"/api/v1/job/{job_id}")
        assert status == 200, f"cancel returned {status}:\n{body}"

        deadline = time.time() + 60
        while time.time() < deadline:
            if job_state(rest_cluster.squeue_all(), job_id) == "CA":
                return
            time.sleep(2)
        raise AssertionError(
            f"job {job_id} was not cancelled via REST:\n{rest_cluster.squeue_all()}"
        )

    def test_jobs_list_includes_submitted_job(self, rest_cluster):
        status, body = _submit(rest_cluster, "rest-list", "#!/bin/bash\nsleep 60\n")
        assert status == 200, body
        job_id = json.loads(body)["job_id"]

        try:
            data = _get_json(rest_cluster, "/api/v1/jobs")
            assert any(j["job_id"] == job_id for j in data["jobs"]), (
                f"job {job_id} missing from /jobs: {[j['job_id'] for j in data['jobs']]}"
            )
        finally:
            rest_cluster.http_delete(f"/api/v1/job/{job_id}")

    def test_jobs_name_filter(self, rest_cluster):
        status, body = _submit(
            rest_cluster, "rest-named", "#!/bin/bash\nsleep 60\n"
        )
        assert status == 200, body
        job_id = json.loads(body)["job_id"]

        try:
            data = _get_json(rest_cluster, "/api/v1/jobs?name=rest-named")
            names = {j["name"] for j in data["jobs"]}
            assert names == {"rest-named"}, f"name filter leaked other jobs: {names}"

            empty = _get_json(rest_cluster, "/api/v1/jobs?name=no-such-job")
            assert empty["jobs"] == [], f"expected no matches, got {empty['jobs']}"
        finally:
            rest_cluster.http_delete(f"/api/v1/job/{job_id}")

    def test_jobs_partition_and_state_filters(self, rest_cluster):
        status, body = _submit(
            rest_cluster,
            "rest-filters",
            "#!/bin/bash\nsleep 60\n",
            partition="default",
        )
        assert status == 200, body
        job_id = json.loads(body)["job_id"]

        try:
            wait_job_state(rest_cluster, job_id, "R", timeout=90)

            by_partition = _get_json(
                rest_cluster, "/api/v1/jobs?partition=default"
            )
            assert any(j["job_id"] == job_id for j in by_partition["jobs"]), (
                f"partition filter dropped job {job_id}"
            )

            by_state = _get_json(rest_cluster, "/api/v1/jobs?state=RUNNING")
            assert any(j["job_id"] == job_id for j in by_state["jobs"]), (
                f"state filter dropped running job {job_id}"
            )
        finally:
            rest_cluster.http_delete(f"/api/v1/job/{job_id}")

    def test_unknown_job_returns_404_envelope(self, rest_cluster):
        status, body = rest_cluster.http_get("/api/v1/job/99999999")
        assert status == 404, f"expected 404, got {status}:\n{body}"
        payload = json.loads(body)
        assert payload["errors"], f"404 must carry an error entry: {payload}"
        assert "not found" in payload["errors"][0]["error"].lower()

    def test_invalid_state_filter_returns_400(self, rest_cluster):
        status, body = rest_cluster.http_get("/api/v1/jobs?state=NOTASTATE")
        assert status == 400, f"expected 400, got {status}:\n{body}"
        assert json.loads(body)["errors"], f"400 must carry an error entry:\n{body}"

    def test_malformed_submit_body_is_rejected(self, rest_cluster):
        status, body = rest_cluster.http_post("/api/v1/job/submit", '{"nope":1}')
        assert status >= 400, f"a body without a job object must fail:\n{body}"

    def test_submit_rejects_unknown_partition(self, rest_cluster):
        status, body = _submit(
            rest_cluster,
            "rest-badpart",
            "#!/bin/bash\ntrue\n",
            partition="no-such-partition",
        )
        assert status == 400, f"expected 400, got {status}:\n{body}"
        assert "not found" in body.lower(), f"expected a partition error:\n{body}"


class TestClusterRoutes:
    def test_nodes_lists_every_registered_node(self, rest_cluster):
        data = _get_json(rest_cluster, "/api/v1/nodes")
        names = {n["name"] for n in data["nodes"]}
        assert set(rest_cluster.node_names) <= names, (
            f"expected {rest_cluster.node_names} in /nodes, got {sorted(names)}"
        )

    def test_node_detail_matches_the_named_node(self, rest_cluster):
        target = rest_cluster.node_names[0]
        data = _get_json(rest_cluster, f"/api/v1/node/{target}")
        assert len(data["nodes"]) == 1, f"expected one node, got {data['nodes']}"
        node = data["nodes"][0]
        assert node["name"] == target
        assert node["cpus"] > 0, f"node must report CPUs: {node}"

    def test_unknown_node_returns_404(self, rest_cluster):
        status, body = rest_cluster.http_get("/api/v1/node/no-such-node")
        assert status == 404, f"expected 404, got {status}:\n{body}"
        assert json.loads(body)["errors"], f"404 must carry an error entry:\n{body}"

    def test_partitions_include_the_default_partition(self, rest_cluster):
        data = _get_json(rest_cluster, "/api/v1/partitions")
        by_name = {p["name"]: p for p in data["partitions"]}
        assert "default" in by_name, f"expected the default partition: {by_name}"
        assert by_name["default"]["is_default"] is True

    @pytest.mark.parametrize("prefix", PREFIXES)
    @pytest.mark.parametrize("route", ["/jobs", "/nodes", "/partitions"])
    def test_collection_routes_on_both_prefixes(self, rest_cluster, prefix, route):
        status, body = rest_cluster.http_get(f"{prefix}{route}")
        assert status == 200, f"GET {prefix}{route} returned {status}:\n{body}"

    @pytest.mark.parametrize("route", ["/jobs/", "/nodes/", "/partitions/"])
    def test_trailing_slash_variants_are_served(self, rest_cluster, route):
        status, body = rest_cluster.http_get(f"/api/v1{route}")
        assert status == 200, f"GET /api/v1{route} returned {status}:\n{body}"

    def test_unknown_route_returns_404(self, rest_cluster):
        status, _ = rest_cluster.http_get("/api/v1/nope")
        assert status == 404, f"an unrouted path must 404, got {status}"


class TestRaftFollowerWrites:
    def test_follower_rejects_submit_with_503(self, raft_cluster):
        """Writes go through the leader; a follower must say so, not silently
        accept and drop them."""
        leader = raft_cluster.wait_raft_leader()
        followers = [i for i in raft_cluster.controller_indices if i != leader]
        assert followers, "a 3-node Raft cluster must have followers"

        payload = json.dumps(
            {"job": {"name": "raft-follower", "script": "#!/bin/bash\ntrue\n"}}
        )
        status, body = raft_cluster.http_post(
            "/api/v1/job/submit", payload, node_index=followers[0]
        )
        assert status == 503, (
            f"a follower must reject submits with 503, got {status}:\n{body}"
        )
        assert "leader" in body.lower(), f"expected a leader hint:\n{body}"

    def test_follower_still_serves_reads(self, raft_cluster):
        """Reads are served from local state, so a follower answers them."""
        leader = raft_cluster.wait_raft_leader()
        follower = next(i for i in raft_cluster.controller_indices if i != leader)

        status, body = raft_cluster.http_get("/api/v1/nodes", node_index=follower)
        assert status == 200, f"a follower must serve reads, got {status}:\n{body}"
        names = {n["name"] for n in json.loads(body)["nodes"]}
        assert set(raft_cluster.node_names) <= names, (
            f"follower read is missing nodes: {sorted(names)}"
        )

    def test_follower_reports_replica_mode(self, raft_cluster):
        leader = raft_cluster.wait_raft_leader()
        follower = next(i for i in raft_cluster.controller_indices if i != leader)
        assert raft_cluster.raft_role(follower) == "replica", (
            f"controller {follower} should report replica mode"
        )
