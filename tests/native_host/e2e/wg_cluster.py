# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""WireGuard-mesh helpers for the native-host WG E2E tests.

:class:`WgMesh` is a thin *composition* over ``SpurCluster``, not a subclass —
the mesh lifecycle (``net init``/``join``, ``wg show`` parsing) is orthogonal to
the daemon lifecycle, and keeping them separate avoids overriding the base
class's private start helpers. Tests that need the daemon stack (k0s scenarios)
drive the base cluster directly; tests that only need the raw mesh use this.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass

# cluster.py lives in native_host/e2e, which pytest.ini puts on pythonpath.
from cluster import SshNode

# The mesh test CIDR. .1 is the head/controller; workers get .2, .3, ...
MESH_CIDR = "10.44.0.0/16"
MESH_PREFIX = 16
WG_PORT = 51820
WG_IFACE = "spur0"


@dataclass
class WgPeerView:
    """One peer as seen in `wg show <iface> dump` on a node."""

    public_key: str
    endpoint: str | None
    allowed_ips: list[str]
    rx_bytes: int
    tx_bytes: int


class WgMesh:
    """Drives a raw WireGuard mesh across a set of SSH nodes.

    Owns no daemons — it runs the ``spur net`` CLI and ``wg`` directly on each
    node over SSH. The controller node is index 0 (``.1``); every other node
    joins it. All ``wg`` calls go through sudo (the interface is root-owned).
    """

    def __init__(self, nodes: list[SshNode], node_names: list[str], bin_dir: str,
                 sudo_prefix: str, iface: str = WG_IFACE,
                 wg_addresses: dict[int, str] | None = None):
        self.nodes = nodes
        self.node_names = node_names
        self.bin_dir = bin_dir
        self.iface = iface
        self._sudo = sudo_prefix
        # Filled by init_mesh(): index -> public key.
        self.pubkeys: dict[int, str] = {}
        # index -> mesh IP. Caller-supplied (mirrors spur-toolkit's per-node
        # `spur_wg_address` inventory var) or the k0s-matching default.
        self.mesh_ips: dict[int, str] = wg_addresses or self._default_mesh_ips()

    def _default_mesh_ips(self) -> dict[int, str]:
        """Default mesh-IP map used when the caller passes no explicit
        ``wg_addresses``.

        Node 0 is pinned to ``.1`` — the reserved bootstrap-controller address
        the head brings up with ``net init`` — and the remaining nodes take
        ``.2``, ``.3`` … The controller does not re-derive these: once a node is
        meshed it adopts that node's advertised ``spur0`` address as
        authoritative, so the only invariant this map must preserve is that no
        node collides with node 0's reserved ``.1``. Sorting the rest by name
        just makes the assignment stable across runs; it is not tied to any
        k0s-internal pool ordering.
        """
        mapping = {0: "10.44.0.1"}  # node 0 is the bootstrap controller -> reserved .1
        rest = sorted(range(1, len(self.node_names)),
                      key=lambda i: self.node_names[i])
        for pos, idx in enumerate(rest):
            mapping[idx] = f"10.44.0.{pos + 2}"
        return mapping

    def mesh_ip_for(self, index: int) -> str:
        """This node's assigned mesh IP (explicit or k0s-matching default)."""
        return self.mesh_ips[index]

    # --- spur net CLI (per node) ---

    def _spur(self, node: SshNode, args: list[str], *, sudo: bool = True) -> str:
        """Run `spur <args>` on a specific node. wg ops need root, hence sudo."""
        prefix = self._sudo if sudo else ""
        quoted = " ".join(f"'{a}'" for a in args)
        return node.exec(f"{prefix}'{self.bin_dir}/spur' {quoted}")

    def net_init(self, index: int = 0) -> str:
        """`spur net init` on the head node — assigns .1 and brings up the iface."""
        node = self.nodes[index]
        out = self._spur(node, [
            "net", "init", "--cidr", MESH_CIDR, "--interface", self.iface,
            "--port", str(WG_PORT),
        ])
        self.pubkeys[index] = self.wg_pubkey(index)
        return out

    def net_join(self, index: int, server_endpoint: str, server_key: str) -> str:
        """`spur net join` on a worker node — brings up its iface toward the head."""
        node = self.nodes[index]
        out = self._spur(node, [
            "net", "join",
            "--endpoint", server_endpoint,
            "--server-key", server_key,
            "--address", self.mesh_ip_for(index),
            "--prefix-len", str(MESH_PREFIX),
            "--interface", self.iface,
        ])
        self.pubkeys[index] = self.wg_pubkey(index)
        return out

    def net_add_peer(self, on_index: int, peer_key: str, allowed_ip: str, *,
                     endpoint: str | None = None, pod_cidr: str | None = None,
                     program_routes: bool = False) -> str:
        args = ["net", "add-peer", "--key", peer_key, "--allowed-ip", allowed_ip,
                "--interface", self.iface]
        if endpoint:
            args += ["--endpoint", endpoint]
        if pod_cidr:
            args += ["--pod-cidr", pod_cidr]
        if program_routes:
            args += ["--program-routes"]
        return self._spur(self.nodes[on_index], args)

    # --- wg introspection ---

    def wg_pubkey(self, index: int) -> str:
        return self.nodes[index].exec(
            f"{self._sudo}wg show '{self.iface}' public-key"
        ).strip()

    def wg_peer_keys(self, index: int) -> list[str]:
        out = self.nodes[index].exec_allow_fail(
            f"{self._sudo}wg show '{self.iface}' peers"
        )
        return [line.strip() for line in out.splitlines() if line.strip()]

    def wg_dump(self, index: int) -> list[WgPeerView]:
        """Parse `wg show <iface> dump` into peer views (skips the interface line).

        Dump format, tab-separated, one line per peer after the first:
          <pubkey> <psk> <endpoint> <allowed-ips> <latest-hs> <rx> <tx> <keepalive>
        """
        out = self.nodes[index].exec_allow_fail(
            f"{self._sudo}wg show '{self.iface}' dump"
        )
        peers: list[WgPeerView] = []
        for line in out.splitlines()[1:]:  # first line is the interface itself
            f = line.split("\t")
            if len(f) < 8:
                continue
            endpoint = f[2].strip()
            peers.append(WgPeerView(
                public_key=f[0].strip(),
                endpoint=None if endpoint in ("", "(none)") else endpoint,
                allowed_ips=[a for a in f[3].split(",") if a and a != "(none)"],
                rx_bytes=int(f[5]) if f[5].isdigit() else 0,
                tx_bytes=int(f[6]) if f[6].isdigit() else 0,
            ))
        return peers

    def wg_peer(self, index: int, peer_key: str) -> WgPeerView | None:
        for p in self.wg_dump(index):
            if p.public_key == peer_key:
                return p
        return None

    def wg_transfer(self, index: int, peer_key: str) -> tuple[int, int]:
        """(rx_bytes, tx_bytes) for a specific peer, or (0, 0) if absent."""
        p = self.wg_peer(index, peer_key)
        return (p.rx_bytes, p.tx_bytes) if p else (0, 0)

    # --- connectivity ---

    def ping(self, from_index: int, to_mesh_ip: str, count: int = 3,
             timeout_s: int = 5) -> bool:
        """Ping a mesh IP from a node. Returns True on any reply."""
        out = self.nodes[from_index].exec_allow_fail(
            f"ping -c {count} -W {timeout_s} {to_mesh_ip}"
        )
        return " 0% packet loss" in out or re.search(r"[1-9]\d* received", out) is not None

    def assert_all_to_all(self, indices: list[int], settle_s: int = 30) -> None:
        """Every node in *indices* must ping every other over the mesh IP.

        WireGuard is lazy: a freshly added worker↔worker peer has no live tunnel
        until the first packet triggers a handshake, so an immediate ping can
        drop even though the peer/endpoint config is correct. Each pair is
        therefore retried for up to *settle_s* seconds (the ping itself sends the
        handshake-triggering traffic) before it is considered unreachable.
        """
        for src in indices:
            for dst in indices:
                if src == dst:
                    continue
                target = self.mesh_ip_for(dst)
                wait_until(
                    lambda s=src, t=target: self.ping(s, t),
                    timeout_s=settle_s, interval_s=3,
                    desc=(f"node {self.node_names[src]} reaching {target} "
                          f"({self.node_names[dst]}) over the mesh"),
                )

    def full_mesh_by_add_peer(self, indices: list[int]) -> None:
        """Wire a full mesh the plain way: every node adds every other as a peer
        with its underlay endpoint. Mirrors what `spur k8s up`'s reconcile does,
        but driven directly so a mesh-only (no-k0s) test can exercise
        worker↔worker connectivity."""
        for on in indices:
            for other in indices:
                if on == other:
                    continue
                self.net_add_peer(
                    on_index=on,
                    peer_key=self.pubkeys[other],
                    allowed_ip=f"{self.mesh_ip_for(other)}/32",
                    endpoint=f"{self.nodes[other].host}:{WG_PORT}",
                )

    def bring_up(self, indices: list[int]) -> None:
        """Stand up a hub-and-spoke then promote to full mesh across *indices*.

        indices[0] is the head (`net init`); the rest `net join` it, the head
        adds each as a peer, then every node adds every other (full mesh).
        """
        # Idempotent pre-clean: `net init`/`join` run `wg-quick up spur0`, which
        # errors ("`spur0' already exists") if a prior mesh is still up on the
        # node. Tear any pre-existing interface down first so bring-up is
        # repeatable and does not depend on a pristine node.
        self.teardown(indices)
        head = indices[0]
        self.net_init(head)
        head_key = self.pubkeys[head]
        head_endpoint = f"{self.nodes[head].host}:{WG_PORT}"
        for i in indices[1:]:
            self.net_join(i, head_endpoint, head_key)
            # Head learns the worker (its underlay endpoint) so the tunnel is bidirectional.
            self.net_add_peer(
                on_index=head,
                peer_key=self.pubkeys[i],
                allowed_ip=f"{self.mesh_ip_for(i)}/32",
                endpoint=f"{self.nodes[i].host}:{WG_PORT}",
            )
        # Promote hub-and-spoke to full mesh. The loop above already wired every
        # head↔worker pair (net_join + the head's add-peer), so only the
        # worker↔worker pairs are still missing — scope the full-mesh pass to the
        # non-head nodes instead of re-issuing the head pairs.
        self.full_mesh_by_add_peer(indices[1:])

    def teardown(self, indices: list[int]) -> None:
        """Best-effort: tear each node's interface down and remove its conf.

        This suite shares CI nodes/shards with ``native_host/e2e``, so every WG
        fixture MUST run this on exit: a leftover ``spur0`` (or its
        ``/etc/wireguard`` conf) would otherwise outlive the test and could trip
        a later non-WG test that assumes a clean network. Removing the link +
        conf here keeps the node indistinguishable from one that never ran a WG
        test.
        """
        for i in indices:
            self.nodes[i].exec_allow_fail(f"{self._sudo}wg-quick down '{self.iface}' 2>/dev/null || true")
            self.nodes[i].exec_allow_fail(f"{self._sudo}ip link del '{self.iface}' 2>/dev/null || true")
            self.nodes[i].exec_allow_fail(
                f"{self._sudo}rm -f '/etc/wireguard/{self.iface}.conf' 2>/dev/null || true"
            )


def wg_available(node: SshNode, sudo_prefix: str) -> tuple[bool, str]:
    """Check a node can run a real WireGuard mesh: `wg` tool + module loadable.

    Returns (ok, reason). Reason is human-readable for a pytest.skip message.
    """
    if "OK" not in node.exec_allow_fail("command -v wg >/dev/null && echo OK"):
        return False, "wireguard-tools (`wg`) not installed"
    if "OK" not in node.exec_allow_fail("command -v wg-quick >/dev/null && echo OK"):
        return False, "wireguard-tools (`wg-quick`) not installed"
    # Module either built-in, loadable, or provided by wireguard-go (userspace).
    probe = node.exec_allow_fail(
        f"{sudo_prefix}modprobe wireguard 2>/dev/null && echo KMOD || "
        "{ command -v wireguard-go >/dev/null && echo USERSPACE; } || echo NONE"
    )
    if "KMOD" in probe or "USERSPACE" in probe:
        return True, ""
    return False, "no WireGuard kernel module and no wireguard-go userspace fallback"


def wait_until(predicate, timeout_s: int, interval_s: int = 3,
               desc: str = "condition") -> None:
    """Poll *predicate* until true or raise TimeoutError."""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if predicate():
            return
        time.sleep(interval_s)
    raise TimeoutError(f"{desc} not met within {timeout_s}s")
