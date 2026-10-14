# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the auto-recovery hook.

When a node enters a system-initiated drain (health-check failure, epilog
failure), the controller dispatches an admin-configured recovery script.
The script owns its phase state via ``spur.recovery/*`` labels and the final
return-to-service decision via ``spur node undrain --self``.

Configured via ``[recovery]`` in spur.conf.
"""

import time


def _passing_recovery(bin_dir: str, controller_addr: str, conf_path: str) -> str:
    return (
        f"#!/bin/bash\n"
        f"set -euo pipefail\n"
        f"export SPUR_CONTROLLER_ADDR={controller_addr}\n"
        f"export SPUR_CONF={conf_path}\n"
        f"{bin_dir}/spur node label --self \"$SPUR_NODE_NAME\" \"spur.recovery/phase-\" || true\n"
        f"{bin_dir}/spur node undrain --self \"$SPUR_NODE_NAME\"\n"
    )


FAILING_RECOVERY = "#!/bin/bash\nexit 1\n"

PERMANENT_FAIL_RECOVERY = "#!/bin/bash\nexit 42\n"


def _phased_recovery(bin_dir: str, controller_addr: str, conf_path: str) -> str:
    return (
        f'#!/bin/bash\n'
        f'set -euo pipefail\n'
        f'export SPUR_CONTROLLER_ADDR={controller_addr}\n'
        f'export SPUR_CONF={conf_path}\n'
        f'case "$SPUR_RECOVERY_PHASE" in\n'
        f'  "")\n'
        f'    {bin_dir}/spur node label --self "$SPUR_NODE_NAME" spur.recovery/phase=verify\n'
        f'    ;;\n'
        f'  verify)\n'
        f'    {bin_dir}/spur node label --self "$SPUR_NODE_NAME" "spur.recovery/phase-"\n'
        f'    {bin_dir}/spur node undrain --self "$SPUR_NODE_NAME"\n'
        f'    ;;\n'
        f'esac\n'
    )


FAILING_HEALTH = "#!/bin/bash\necho 'gpu fault' >&2\nexit 1\n"


def _recovery_overrides(cluster, recovery_body, health_body: str = FAILING_HEALTH,
                        max_attempts: int = 3, **recovery_kw) -> dict:
    """Write health + recovery scripts and return config overrides.

    recovery_body can be a string or a callable(bin_dir, controller_addr, conf_path) -> str.
    """
    rd = cluster.remote_dir
    bin_dir = cluster.bin_dir
    controller_addr = cluster.controller_addr
    conf_path = f"{rd}/etc/spur.conf"

    if callable(recovery_body):
        recovery_body = recovery_body(bin_dir, controller_addr, conf_path)

    cluster.write_file("health/check.sh", health_body, all_nodes=True)
    cluster.write_file("recovery/recover.sh", recovery_body, all_nodes=True)

    # spurstepd needs a writable spool dir; create /var/spool/spur or the
    # temp fallback so job scripts actually execute.
    for node in cluster.nodes:
        node.exec("sudo mkdir -p /var/spool/spur && sudo chmod 1777 /var/spool/spur || "
                  "mkdir -p /tmp/spur && chmod 777 /tmp/spur || true")
    uid = int(cluster.nodes[0].exec("id -u").strip())
    gid = int(cluster.nodes[0].exec("id -g").strip())
    user = cluster.nodes[0].exec("id -un").strip()
    check = {
        "program": f"{rd}/health/check.sh",
        "nodes": 1,
        "interval_secs": 3,
        "timeout_secs": 10,
        "max_wait_secs": 0,
        "user": user,
        "uid": uid,
        "gid": gid,
    }
    recovery = {
        "program": f"{rd}/recovery/recover.sh",
        "trigger_on": ["drain", "down"],
        "max_attempts": max_attempts,
        "timeout_secs": 30,
        "user": user,
        "uid": uid,
        "gid": gid,
        "reboot_timeout_secs": 60,
        "concurrency_cap": "100%",
    }
    recovery.update(recovery_kw)
    return {
        "health": {"checks": [check]},
        "recovery": recovery,
    }


def _wait_node_drained(cluster, node_name: str, timeout: int = 60) -> str:
    deadline = time.time() + timeout
    last = {}
    while time.time() < deadline:
        last = cluster.sinfo_nodes()
        state = last.get(node_name, "")
        if state.lower().startswith("drain"):
            return state
        time.sleep(1)
    raise AssertionError(f"node {node_name} did not drain within {timeout}s: {last}")


def _wait_node_idle(cluster, node_name: str, timeout: int = 60) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        states = cluster.sinfo_nodes()
        state = states.get(node_name, "")
        if state.lower() == "idle":
            return state
        time.sleep(1)
    raise AssertionError(f"node {node_name} did not return to idle within {timeout}s")


def _wait_reason_contains(cluster, node_name: str, needle: str, timeout: int = 30) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        show = cluster.scontrol_show_node(node_name)
        if needle.lower() in show.lower():
            return show
        time.sleep(1)
    raise AssertionError(f"node {node_name} reason never contained {needle!r}")


class TestRecovery:
    def test_health_fail_triggers_recovery_and_returns_idle(self, unstarted_cluster):
        """Health check fails -> drain -> recovery runs -> clears labels -> undrains -> idle."""
        cluster = unstarted_cluster
        overrides = _recovery_overrides(cluster, _passing_recovery)
        cluster.start(overrides)
        node = list(cluster.sinfo_nodes().keys())[0]

        _wait_node_drained(cluster, node)
        _wait_node_idle(cluster, node, timeout=120)

    def test_phased_recovery(self, unstarted_cluster):
        """Recovery runs phase 1 (sets verify), exits 0, runs phase 2 (clears + undrains)."""
        cluster = unstarted_cluster
        overrides = _recovery_overrides(cluster, _phased_recovery)
        cluster.start(overrides)
        node = list(cluster.sinfo_nodes().keys())[0]

        _wait_node_drained(cluster, node)
        _wait_node_idle(cluster, node, timeout=60)

    def test_recovery_exhausts_max_attempts(self, unstarted_cluster):
        """FAILING_RECOVERY exhausts max_attempts=2, node stays drained with exhaustion reason."""
        cluster = unstarted_cluster
        overrides = _recovery_overrides(cluster, FAILING_RECOVERY, max_attempts=2)
        cluster.start(overrides)
        node = list(cluster.sinfo_nodes().keys())[0]

        _wait_node_drained(cluster, node)
        _wait_reason_contains(cluster, node, "auto-recovery exhausted", timeout=60)

    def test_admin_drain_not_auto_recovered(self, unstarted_cluster):
        """Admin-drained node is not auto-recovered even with [recovery] configured."""
        cluster = unstarted_cluster
        # Use a passing health check so the node starts idle
        overrides = _recovery_overrides(
            cluster,
            _passing_recovery,
            health_body="#!/bin/bash\nexit 0\n",
        )
        cluster.start(overrides)
        node = list(cluster.sinfo_nodes().keys())[0]

        # Admin drain
        cluster.scontrol("update", f"NodeName={node}", "State=DRAIN", "Reason=maintenance")
        _wait_node_drained(cluster, node)

        # Wait and confirm recovery does NOT return the node
        time.sleep(15)
        states = cluster.sinfo_nodes()
        assert states[node].lower().startswith("drain"), \
            f"expected admin-drained node to stay drained, got {states[node]}"

    def test_permanent_failure_exit_42(self, unstarted_cluster):
        """Exit code 42 causes immediate exhaustion regardless of remaining attempts."""
        cluster = unstarted_cluster
        overrides = _recovery_overrides(cluster, PERMANENT_FAIL_RECOVERY, max_attempts=5)
        cluster.start(overrides)
        node = list(cluster.sinfo_nodes().keys())[0]

        _wait_node_drained(cluster, node)
        _wait_reason_contains(cluster, node, "permanent", timeout=60)
