# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Authorization for native k0s cluster ops (SPUR-115); deny paths need no
control plane. Identity is whoami-derived, exercised via `cli_as_user`."""

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
        assert "requires cluster admin" in up.lower(), f"non-admin up must be denied: {up}"

        down = cluster.cli_as_user(submit_user, ["spur", "k8s", "down"])
        assert "requires cluster admin" in down.lower(), f"non-admin down must be denied: {down}"

    def test_non_admin_cannot_fetch_admin_kubeconfig(self, cluster):
        submit_user = self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user(submit_user, ["spur", "k8s", "kubeconfig", "--admin"])
        assert "requires cluster admin" in out.lower(), f"non-admin --admin must be denied: {out}"

    def test_non_admin_cannot_target_another_user(self, cluster):
        submit_user = self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user(
            submit_user, ["spur", "k8s", "kubeconfig", "--user", "someone-else"]
        )
        assert "may only request their own" in out.lower(), f"non-admin --user X must be denied: {out}"

    def test_root_passes_the_admin_gate(self, cluster):
        # Root clears the gate; the specific admin-gate rejection must not appear.
        self._require_sudo_second_identity(cluster)
        out = cluster.cli_as_user("root", ["spur", "k8s", "up"])
        assert "requires cluster admin" not in out.lower(), f"root must clear the admin gate: {out}"
