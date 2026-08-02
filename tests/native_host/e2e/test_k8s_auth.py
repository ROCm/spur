# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Authorization for native k0s cluster ops (SPUR-115).

The admin gate rejects a non-admin caller before any k0s work, so these deny
paths need no real control plane. Identity is whoami-derived, exercised via
`cli_as_user` (same seam as the reservation owner tests)."""

import pytest


class TestK8sAuth:
    def _require_sudo_second_identity(self, cluster):
        submit_user = cluster.nodes[0].user
        if submit_user == "root":
            pytest.skip("need a non-root SSH user to test non-admin rejection")
        probe = cluster.cli_as_user("root", ["spur", "k8s", "status"])
        if "sudo" in probe.lower() and (
            "password" in probe.lower() or "not allowed" in probe.lower()
        ):
            pytest.skip(f"sudo -u unavailable in this environment: {probe.strip()}")
        return submit_user

    def test_non_admin_cannot_up_or_down(self, cluster):
        submit_user = self._require_sudo_second_identity(cluster)

        up = cluster.cli_as_user(submit_user, ["spur", "k8s", "up"])
        assert "permission" in up.lower(), f"non-admin up must be denied: {up}"

        down = cluster.cli_as_user(submit_user, ["spur", "k8s", "down"])
        assert "permission" in down.lower(), f"non-admin down must be denied: {down}"

    def test_non_admin_cannot_fetch_admin_kubeconfig(self, cluster):
        submit_user = self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user(submit_user, ["spur", "k8s", "kubeconfig", "--admin"])
        assert "permission" in out.lower(), f"non-admin --admin must be denied: {out}"

    def test_non_admin_cannot_target_another_user(self, cluster):
        submit_user = self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user(
            submit_user, ["spur", "k8s", "kubeconfig", "--user", "someone-else"]
        )
        assert "permission" in out.lower(), f"non-admin --user X must be denied: {out}"

    def test_root_passes_the_admin_gate(self, cluster):
        # Root clears the gate: up proceeds past authz to the k0s layer, so the
        # failure (if any) is never PermissionDenied.
        self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user("root", ["spur", "k8s", "up"])
        assert "permission" not in out.lower(), f"root must clear the admin gate: {out}"
