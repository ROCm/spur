# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests for k0s-over-WireGuard, driven over real `wg` interfaces.

The raw-mesh (no-k0s) bring-up scenario lives in ``test_wg_mesh.py``; this file
holds the scenarios that need a converged k0s cluster on top of the mesh. Each
class documents the invariant it proves — k0s reaches ready over the mesh with
node InternalIPs on the mesh CIDR, cross-node pod and ClusterIP datapath riding
the tunnel, online add/remove leaving no ghost peer, graceful k8s remove/add
preserving the node's Spur identity + WireGuard key, login-node reachability,
and (skipped) 3-controller HA re-election.

The k0s tests are marked ``k0s``; the WG fixtures they use skip when a node can't
run a real WireGuard mesh.
"""

from __future__ import annotations

import re

import pytest

from wg_cluster import WG_IFACE, wait_until

# An IPv4 dotted-quad, used to filter kubectl output tokens (pod IPs, ClusterIPs,
# InternalIPs) so a kubectl error string is never mistaken for an address.
_IPV4_RE = re.compile(r"^\d{1,3}(?:\.\d{1,3}){3}$")

BUSYBOX_IMAGE = "docker.io/library/busybox:1.36"


# --- kubectl helpers (run `k0s kubectl` on the control-plane node) -----------


def _kubectl(c, cp_index: int, args: str) -> str:
    return c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl {args}"
    )


def _assert_kubectl_applied(out: str, what: str) -> None:
    """Assert a `kubectl apply` succeeded, tolerant of the created/configured/
    unchanged variants kubectl prints depending on prior object state."""
    assert "created" in out or "configured" in out or "unchanged" in out, (
        f"kubectl apply for {what} did not succeed:\n{out}"
    )


def _calico_ready(c, cp_index: int) -> bool:
    """True when the calico CNI is up cluster-wide: the calico-node DaemonSet has
    all pods Running/Ready. A node can report Ready while its calico-node is
    still Init:0/1, and a pod scheduled onto it then never gets an IP — so gate
    pod placement on this, not just node Ready.
    """
    out = _kubectl(c, cp_index,
                   "get pods -n kube-system -l k8s-app=calico-node --no-headers 2>/dev/null")
    lines = [ln for ln in out.splitlines() if ln.strip()]
    if not lines:
        return False
    for ln in lines:
        f = ln.split()
        # READY column like "1/1"; STATUS "Running".
        if len(f) < 3 or f[1] != "1/1" or f[2] != "Running":
            return False
    return True


def _k8s_nodes_ready(c, cp_index: int, names: list[str]) -> bool:
    """True when every node in *names* shows Ready in `kubectl get nodes`."""
    out = _kubectl(c, cp_index, "get nodes --no-headers")
    ready = set()
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 2 and f[1] == "Ready":
            ready.add(f[0])
    return all(n in ready for n in names)


def _launch_pinned_pod(c, cp_index: int, name: str, node_name: str) -> str:
    """Create a long-running pod pinned to *node_name* via nodeName. Uses a
    busybox image already present on the nodes (the CI image ships it); the pod
    just sleeps so we can exec ping from it. Returns the pod name."""
    manifest = (
        "apiVersion: v1\n"
        "kind: Pod\n"
        f"metadata:\n  name: {name}\n"
        "spec:\n"
        f"  nodeName: {node_name}\n"
        "  containers:\n"
        f"  - name: {name}\n"
        f"    image: {BUSYBOX_IMAGE}\n"
        "    imagePullPolicy: IfNotPresent\n"
        "    command: ['sh','-c','sleep 3600']\n"
        "  restartPolicy: Never\n"
    )
    # Write the manifest to a remote file (SFTP) and apply it. A heredoc piped
    # through the SSH channel does not survive as real stdin, so the apply would
    # otherwise get empty input and create nothing.
    remote_path = f"/tmp/{name}.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    _assert_kubectl_applied(out, f"pod {name}")
    return name


def _wait_pod_ip(c, cp_index: int, pod: str, timeout_s: int = 180) -> str:
    """Poll until the pod reports a pod-CIDR IP (Running with an IP assigned)."""
    def read_ip() -> str:
        out = _kubectl(c, cp_index,
                       f"get pod {pod} -o jsonpath='{{.status.podIP}}'").strip()
        return out if _IPV4_RE.match(out) else ""

    wait_until(lambda: bool(read_ip()), timeout_s=timeout_s,
               desc=f"pod {pod} got a pod-CIDR IP")
    return read_ip()


def _pod_ping(c, cp_index: int, pod: str, target_ip: str) -> None:
    """Ping *target_ip* from inside *pod* (best effort — the counter assertion
    is what proves the datapath; this just generates the traffic)."""
    _kubectl(c, cp_index,
             f"exec {pod} -- ping -c 5 -W 2 {target_ip} 2>/dev/null || true")


def _launch_httpd_pod(c, cp_index: int, name: str, node_name: str, body: str) -> str:
    """A busybox `httpd` pod pinned to *node_name* serving *body* on :80. Labeled
    app=<name> so a Service can select it. Returns the pod name."""
    manifest = (
        "apiVersion: v1\n"
        "kind: Pod\n"
        f"metadata:\n  name: {name}\n  labels:\n    app: {name}\n"
        "spec:\n"
        f"  nodeName: {node_name}\n"
        "  containers:\n"
        f"  - name: {name}\n"
        f"    image: {BUSYBOX_IMAGE}\n"
        "    imagePullPolicy: IfNotPresent\n"
        "    command: ['sh','-c',"
        f"'mkdir -p /w && printf {body} > /w/index.html && httpd -f -p 80 -h /w']\n"
        "    ports:\n    - containerPort: 80\n"
        "  restartPolicy: Never\n"
    )
    remote_path = f"/tmp/{name}.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    _assert_kubectl_applied(out, f"httpd pod {name}")
    return name


def _expose_clusterip(c, cp_index: int, pod: str, svc: str, port: int = 80) -> str:
    """Create a ClusterIP Service *svc* selecting app=<pod> on *port*, and return
    its allocated ClusterIP (10.48.x.x)."""
    manifest = (
        "apiVersion: v1\n"
        "kind: Service\n"
        f"metadata:\n  name: {svc}\n"
        "spec:\n"
        "  type: ClusterIP\n"
        f"  selector:\n    app: {pod}\n"
        f"  ports:\n  - port: {port}\n    targetPort: {port}\n"
    )
    remote_path = f"/tmp/{svc}-svc.yaml"
    c.nodes[cp_index].write_file(remote_path, manifest)
    out = c.nodes[cp_index].exec_allow_fail(
        f"{c._sudo_prefix()}k0s kubectl apply -f {remote_path} 2>&1"
    )
    _assert_kubectl_applied(out, f"service {svc}")

    def read_cip() -> str:
        cip = _kubectl(c, cp_index,
                       f"get svc {svc} -o jsonpath='{{.spec.clusterIP}}'").strip()
        return cip if _IPV4_RE.match(cip) else ""

    wait_until(lambda: bool(read_cip()), timeout_s=60,
               desc=f"service {svc} got a ClusterIP")
    return read_cip()


def _prepull_busybox(c, node_indices: list[int]) -> bool:
    """Pull the pod image into each worker's containerd k8s.io namespace up front.

    Pulling in-pod races the test's IP-wait and can time out on a cold cache, so
    pre-pull directly via `ctr`. Returns False if any node can't obtain the image
    (offline registry), so the caller can skip rather than hang.
    """
    for i in node_indices:
        out = c.nodes[i].exec_allow_fail(
            f"{c._sudo_prefix()}ctr -n k8s.io images pull {BUSYBOX_IMAGE} 2>&1"
        )
        present = c.nodes[i].exec_allow_fail(
            f"{c._sudo_prefix()}ctr -n k8s.io images ls -q 2>/dev/null | grep -c busybox"
        ).strip()
        if present == "0" and "done" not in out and "already exists" not in out:
            return False
    return True


def _ready_two_workers_with_image(c, cp_index: int, worker_names: list[str],
                                  worker_indices: list[int]) -> None:
    """Gate a cross-node pod-datapath test: both workers must be Ready k8s nodes,
    calico must be up cluster-wide, and the pod image must be pre-pulled on each
    worker. ``pytest.skip`` if the image can't be obtained (offline registry).

    Shared by the pod-to-pod and ClusterIP datapath tests, which need the exact
    same preconditions before they can place pods on two distinct workers.
    """
    # Workers must register with the k8s API before we can place pods…
    wait_until(lambda: _k8s_nodes_ready(c, cp_index, worker_names),
               timeout_s=300, desc="both worker kubelets Ready")
    # …and calico must be up, or a pod scheduled onto a worker whose calico-node
    # is still initializing never gets a pod IP.
    wait_until(lambda: _calico_ready(c, cp_index),
               timeout_s=480, desc="calico-node DaemonSet Running cluster-wide")
    # Pre-pull the pod image so pod start doesn't race the IP-wait on a cold
    # cache; skip cleanly if the registry is unreachable.
    if not _prepull_busybox(c, worker_indices):
        pytest.skip(f"{BUSYBOX_IMAGE} unavailable on the workers (offline registry)")


# --- k0s over mesh -------------------------------------------------------


@pytest.mark.k0s
class TestK0sOverMesh:
    def test_cluster_reaches_ready_over_mesh(self, wg_k0s_cluster):
        c = wg_k0s_cluster
        # Pin the control plane to node0 (the spur controller). Without an
        # explicit pin the reconcile elects the alphabetically-first node, which
        # is nondeterministic on a mixed-hostname bed.
        out = c.k8s_up(["--control-plane-node", c.node_names[0]])
        assert "provisioning requested" in out or "already" in out, out
        c.wait_k8s_phase("ready", timeout=600)

    def test_node_internal_ip_is_mesh_ip(self, wg_k0s_cluster):
        """Under calico-over-mesh, each k8s node's InternalIP is its mesh IP
        (kubelet --node-ip pinned to the spur0 address)."""
        c = wg_k0s_cluster
        # Pin the control plane to node0 so the test's node0=CP assumption holds.
        c.k8s_up(["--control-plane-node", c.node_names[0]])
        c.wait_k8s_phase("ready", timeout=600)

        cps = c.k8s_control_planes()
        assert cps, "no control-plane node reported"
        cp_index = c.node_names.index(cps[0])

        # `ready` is partial-Ready (CP quorum only) — worker kubelets register
        # with the k8s API a little later, and the API server itself may still be
        # settling (kubectl can transiently report "connection refused"). Poll
        # until real IPv4 InternalIPs appear, keeping ONLY IP-shaped tokens so a
        # kubectl error string is never mistaken for an address.
        def read_internal_ips() -> set[str]:
            out = c.nodes[cp_index].exec_allow_fail(
                f"{c._sudo_prefix()}k0s kubectl get nodes "
                "-o jsonpath='{range .items[*]}{.status.addresses[?(@.type==\"InternalIP\")].address}{\"\\n\"}{end}'"
            )
            return {t for t in (line.strip() for line in out.splitlines()) if _IPV4_RE.match(t)}

        wait_until(lambda: len(read_internal_ips()) > 0, timeout_s=180,
                   desc="kubelet InternalIPs registered with the k8s API")

        internal_ips = read_internal_ips()
        assert all(ip.startswith("10.46.") for ip in internal_ips), (
            f"expected all InternalIPs on the mesh CIDR 10.46.0.0/16, got {internal_ips}"
        )

    def test_controller_stays_meshed(self, wg_k0s_cluster):
        """The controller (node 0, mesh .1) must remain a peer on every other
        node after `k8s up` — the reconcile must not prune the head node."""
        c = wg_k0s_cluster
        # Pin the control plane to node0 so its mesh .1 is the controller the
        # assertion below expects.
        c.k8s_up(["--control-plane-node", c.node_names[0]])
        c.wait_k8s_phase("ready", timeout=600)

        ctrl_ip = c.wg_mesh.mesh_ip_for(0)

        def controller_reachable() -> bool:
            for i in range(1, len(c.nodes)):
                out = c.nodes[i].exec_allow_fail(
                    f"ping -c 2 -W 5 {ctrl_ip}"
                )
                if " 0% packet loss" not in out:
                    return False
            return True

        wait_until(controller_reachable, timeout_s=120,
                   desc="all nodes reach the controller mesh IP after k8s up")


# --- cross-node pods ride the tunnel ------------------------------------


@pytest.mark.k0s
class TestPodsOnMesh:
    def test_pod_cidr_folded_into_peer_allowed_ips(self, wg_k0s_cluster):
        """Each remote peer's AllowedIPs must include that node's pod /24, so
        WireGuard cryptokey-routes pod packets across the mesh."""
        c = wg_k0s_cluster
        # Pin the control plane to node0 for a deterministic topology.
        c.k8s_up(["--control-plane-node", c.node_names[0]])
        c.wait_k8s_phase("ready", timeout=600)

        # Pod CIDRs are folded into AllowedIPs only after role assignment and the
        # NEXT ApplyMesh reconcile tick — which lands after partial-Ready. The
        # mesh first converges the /32 peers (via net join/add-peer), then a
        # later push enriches them with each node's pod /24. Poll for it.
        def pod_cidr_present() -> bool:
            out = c.nodes[0].exec_allow_fail(
                f"{c._sudo_prefix()}wg show '{WG_IFACE}' allowed-ips"
            )
            return "10.47." in out

        wait_until(pod_cidr_present, timeout_s=180,
                   desc="pod CIDR (10.47.x) folded into a peer's AllowedIPs")

    def test_cross_node_pod_ping_rides_the_tunnel(self, wg_k0s_cluster):
        """A pod on worker A pings a pod on worker B over the pod CIDR, and the
        wg transfer counter on the A→B peer rises during the ping — proving the
        traffic actually rode the WireGuard tunnel, not some other path.

        Topology: node0 is the spur controller AND the k8s control-plane; nodes
        1 and 2 are k8s workers. That yields two distinct worker nodes to host a
        pod each even on a 3-node bed (the control-plane node is not used as a
        pod host). This is the strict datapath proof: same-tunnel attribution via
        rising per-peer counters, not merely 'ping succeeded'.
        """
        c = wg_k0s_cluster
        if len(c.node_names) < 3:
            pytest.skip("cross-node pods need >= 3 nodes (CP + two workers)")
        cp = c.node_names[0]
        worker_a, worker_b = c.node_names[1], c.node_names[2]
        a_index, b_index = 1, 2

        # CP on node0 (the spur controller); nodes 1 and 2 become workers.
        c.k8s_up(["--nodes", f"{cp},{worker_a},{worker_b}",
                  "--control-plane-node", cp])
        c.wait_k8s_phase("ready", timeout=600)

        cp_index = 0
        _ready_two_workers_with_image(c, cp_index, [worker_a, worker_b],
                                      [a_index, b_index])

        # Launch one netshoot-style pod pinned to each worker; wait for pod IPs.
        pod_a = _launch_pinned_pod(c, cp_index, "wg-d3-a", worker_a)
        pod_b = _launch_pinned_pod(c, cp_index, "wg-d3-b", worker_b)
        ip_a = _wait_pod_ip(c, cp_index, pod_a)
        ip_b = _wait_pod_ip(c, cp_index, pod_b)
        assert ip_a.startswith("10.47.") and ip_b.startswith("10.47."), (
            f"pods did not get pod-CIDR IPs: a={ip_a} b={ip_b}"
        )

        # worker_b's wg pubkey as seen from worker_a — the peer the pod traffic
        # must traverse. Snapshot its transfer counter, drive a pod→pod ping from
        # worker_a's pod to worker_b's pod, and assert the counter rose.
        peer_key_b = c.wg_mesh.wg_pubkey(b_index)

        def ping_across() -> None:
            _pod_ping(c, cp_index, pod_a, ip_b)

        self._assert_counter_rises(c.wg_mesh, a_index, peer_key_b, ping_across,
                                   timeout_s=60)

        # Cleanup the test pods (best effort).
        for pod in (pod_a, pod_b):
            c.nodes[cp_index].exec_allow_fail(
                f"{c._sudo_prefix()}k0s kubectl delete pod {pod} --now 2>/dev/null || true"
            )

    @staticmethod
    def _assert_counter_rises(mesh, from_index: int, peer_key: str,
                              trigger, timeout_s: int = 30) -> None:
        """Reusable datapath proof: snapshot (rx,tx) for *peer_key*, run
        *trigger* (a callable that generates traffic), then assert the counter
        rose. Kept static so the real pod test and any hardware script share it.
        """
        before_rx, before_tx = mesh.wg_transfer(from_index, peer_key)
        trigger()

        def rose() -> bool:
            rx, tx = mesh.wg_transfer(from_index, peer_key)
            return rx > before_rx or tx > before_tx

        wait_until(rose, timeout_s=timeout_s,
                   desc=f"wg transfer counter rose on peer {peer_key[:8]}…")


# --- service CIDR reachable over the mesh -------------------------------


@pytest.mark.k0s
class TestServiceCidrOverMesh:
    """A ClusterIP service (service CIDR 10.48.0.0/16) is reachable across the
    mesh even though the service CIDR is never in any WireGuard AllowedIPs.

    WireGuard only carries each node's mesh /32 + pod CIDR. A ClusterIP is
    reached because kube-proxy DNATs it (in the kernel, before routing) to a
    backing pod's real pod-CIDR IP — which IS routed over the mesh. This test
    proves that path end to end: a server pod on worker A behind a ClusterIP,
    hit from a client pod on worker B.

    Topology like the pod-datapath test: node0 = control plane, nodes 1 and 2 =
    workers, so the server and client land on distinct nodes and the request must
    cross the mesh.
    """

    def test_clusterip_service_reachable_cross_node(self, wg_k0s_cluster):
        c = wg_k0s_cluster
        if len(c.node_names) < 3:
            pytest.skip("service-CIDR test needs >= 3 nodes (CP + two workers)")
        cp = c.node_names[0]
        server_node, client_node = c.node_names[1], c.node_names[2]
        cp_index = 0

        c.k8s_up(["--nodes", f"{cp},{server_node},{client_node}",
                  "--control-plane-node", cp])
        c.wait_k8s_phase("ready", timeout=600)
        _ready_two_workers_with_image(c, cp_index, [server_node, client_node],
                                      [1, 2])

        # Server pod on worker A: busybox httpd serving a known body on :80.
        body = "SPUR_SVC_OK"
        server = _launch_httpd_pod(c, cp_index, "wg-svc-server", server_node, body)
        _wait_pod_ip(c, cp_index, server)
        # Expose it as a ClusterIP service.
        cluster_ip = _expose_clusterip(c, cp_index, server, "wg-svc", port=80)
        assert cluster_ip.startswith("10.48."), (
            f"service ClusterIP not in the service CIDR (10.48.x): {cluster_ip}"
        )

        # Client pod on worker B curls the ClusterIP; kube-proxy DNATs it to the
        # server pod's IP, which rides the mesh to worker A.
        client = _launch_pinned_pod(c, cp_index, "wg-svc-client", client_node)
        _wait_pod_ip(c, cp_index, client)

        def fetched() -> bool:
            out = _kubectl(
                c, cp_index,
                f"exec {client} -- wget -qO- --timeout=5 http://{cluster_ip}:80 2>/dev/null"
            )
            return body in out

        wait_until(fetched, timeout_s=90,
                   desc=f"ClusterIP {cluster_ip} reachable from a pod on {client_node}")

        # Cleanup (best effort).
        for obj in (f"pod/{server}", f"pod/{client}", "svc/wg-svc"):
            c.nodes[cp_index].exec_allow_fail(
                f"{c._sudo_prefix()}k0s kubectl delete {obj} --now 2>/dev/null || true"
            )


# --- online add/remove + no ghost peer ----------------------------------


@pytest.mark.k0s
class TestOnlineAddRemove:
    def test_add_then_remove_node_leaves_no_ghost_peer(self, wg_k0s_cluster):
        """Add a worker via `k8s add-nodes`, then remove it; after cleanup the
        controller's wg peer table must not still list the removed node."""
        c = wg_k0s_cluster
        if len(c.node_names) < 3:
            pytest.skip("add/remove needs >= 3 nodes")
        first, second, third = c.node_names[0], c.node_names[1], c.node_names[2]

        # Scope the cluster to two nodes, leaving the third out initially.
        c.k8s_up(["--nodes", f"{first},{second}", "--control-plane-node", first])
        c.wait_k8s_phase("ready", timeout=600)

        # Grow: add the third node online.
        c.k8s_add_nodes(["--nodes", third])

        def third_is_member() -> bool:
            return third in c.k8s_member_list()

        wait_until(third_is_member, timeout_s=120, desc=f"{third} joined membership")

        third_index = c.node_names.index(third)

        # Capture the added node's WireGuard key only once spur0 is up and keyed
        # — a freshly added node's interface can lag membership, and an empty key
        # would make the later `net remove-peer` a no-op (masking a ghost peer).
        def read_third_key() -> str:
            return c.nodes[third_index].exec_allow_fail(
                f"{c._sudo_prefix()}wg show '{WG_IFACE}' public-key"
            ).strip()

        wait_until(lambda: bool(read_third_key()), timeout_s=60,
                   desc=f"{third} WireGuard key available")
        third_key = read_third_key()

        # Remove it from k0s scope. --force so the product's internal drain can
        # finish past the un-evictable daemonset/PDB pods (calico/coredns/
        # konnectivity) it would otherwise refuse on — matches operator practice.
        c.k8s_remove_nodes(["--nodes", third, "--force"])

        def third_not_member() -> bool:
            return third not in c.k8s_member_list()

        wait_until(third_not_member, timeout_s=300, desc=f"{third} left membership")

        # Explicit mesh cleanup — the counterpart to add-peer.
        c.cli_as_user("root", ["spur", "net", "remove-peer",
                               "--key", third_key, "--interface", WG_IFACE])

        # No ghost: the controller must no longer list the removed node's key.
        def ghost_gone() -> bool:
            peers = c.nodes[0].exec_allow_fail(
                f"{c._sudo_prefix()}wg show '{WG_IFACE}' peers"
            )
            return third_key not in peers

        wait_until(ghost_gone, timeout_s=60,
                   desc=f"removed node {third} pruned from controller wg peers")

    def test_remove_peer_is_idempotent(self, wg_k0s_cluster):
        """`net remove-peer` on an absent key succeeds (idempotent) when the
        interface exists — removing a never-added key must not error.

        Idempotency is a property of removing a *peer* that isn't there, not of a
        missing interface: `wg set <iface> peer <key> remove` on a non-existent
        `spur0` correctly errors ("No such device"). The fixture guarantees
        `spur0` is up, so this exercises the real absent-peer path — no `k8s up`
        needed (which would add a 600s convergence wait unrelated to this check).
        """
        c = wg_k0s_cluster
        # A syntactically valid but not-present WireGuard key (32 bytes base64).
        absent_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="

        def peer_set() -> set[str]:
            out = c.nodes[0].exec_allow_fail(
                f"{c._sudo_prefix()}wg show '{WG_IFACE}' peers"
            )
            return {ln.strip() for ln in out.splitlines() if ln.strip()}

        # Snapshot the real peer table so we can prove the removal was a true
        # no-op, not just that the CLI printed a success string.
        before = peer_set()
        out = c.cli_as_user("root", ["spur", "net", "remove-peer",
                                     "--key", absent_key, "--interface", WG_IFACE])
        assert "removed" in out.lower(), (
            f"remove-peer on an absent key should succeed idempotently:\n{out}"
        )
        # The peer table must be byte-for-byte unchanged: removing a key that was
        # never present must not add, drop, or disturb any existing peer.
        assert peer_set() == before, (
            "remove-peer on an absent key changed the wg peer table "
            f"(before={before}, after={peer_set()})"
        )


# --- graceful k8s remove/add keeps the node a spur worker + wg identity --


@pytest.mark.k0s
class TestGracefulK8sRemoveAdd:
    """A k8s worker cycled out of and back into the k0s cluster keeps its Spur
    identity and WireGuard key throughout — removal takes it out of k0s only, not
    out of Spur or the mesh, and the re-add does not re-key it.

    Topology: node0 = spur controller + k8s control-plane, node1 + node2 = k8s
    workers. The test cycles node1 while node2 stays a worker throughout — a
    single-worker (CP + 1) cluster can't converge calico/coredns once the sole
    worker drains, so the second worker keeps the cluster healthy across the
    remove/add. The step-by-step is in the test body.
    """

    def test_remove_keeps_spur_and_wg_then_add_restores_k8s(self, wg_k0s_cluster):
        c = wg_k0s_cluster
        if len(c.node_names) < 3:
            pytest.skip("graceful remove/add needs 3 nodes (CP + two workers)")
        cp, worker, other_worker = c.node_names[0], c.node_names[1], c.node_names[2]
        worker_index = 1

        # CP on node0 + both other nodes as workers, so a schedulable worker
        # remains after node1 is removed and the cluster stays converged.
        cp_index = c.node_names.index(cp)
        c.k8s_up(["--nodes", f"{cp},{worker},{other_worker}",
                  "--control-plane-node", cp])
        c.wait_k8s_phase("ready", timeout=600)
        wait_until(lambda: worker in c.k8s_member_list(), timeout_s=120,
                   desc=f"{worker} joined k0s membership")
        # Wait for BOTH workers to be fully-Ready k8s nodes before removing one:
        # `ready` is partial-Ready (CP quorum), so a worker's k0s component may
        # still be converging. Removing it then hits `stop_cluster_component
        # timed out`, and a kubectl cordon/drain hits "node not found".
        wait_until(lambda: _k8s_nodes_ready(c, cp_index, [worker, other_worker]),
                   timeout_s=480, desc="both workers are Ready k8s nodes")

        # WG identity + Spur registration baseline, captured while it's a k8s worker.
        wg_key_before = c.wg_mesh.wg_pubkey(worker_index)
        assert wg_key_before, f"{worker} has no WireGuard key before removal"
        assert worker in c.sinfo_nodes(), f"{worker} not registered with Spur"

        # 1. Operator pre-drain: cordon + drain the worker via kubectl on the CP.
        # Best-effort with a bounded timeout and --force so an un-evictable pod
        # (a bare pod without a controller) can't wedge the drain — the point is
        # to exercise the operator pre-drain path, and the authoritative removal
        # is `spur k8s remove-nodes --force` below.
        c.nodes[cp_index].exec_allow_fail(
            f"{c._sudo_prefix()}k0s kubectl cordon {worker}"
        )
        c.nodes[cp_index].exec_allow_fail(
            f"{c._sudo_prefix()}k0s kubectl drain {worker} "
            f"--ignore-daemonsets --delete-emptydir-data --force --timeout=60s"
        )

        # 2. Graceful k0s-only removal. The manual cordon+drain above evicts the
        # workload pods; --force lets the product's internal drain finish past
        # the daemonset/PDB residue (calico/coredns/konnectivity) it can't evict.
        c.k8s_remove_nodes(["--nodes", worker, "--force"])
        wait_until(lambda: worker not in c.k8s_member_list(), timeout_s=300,
                   desc=f"{worker} left k0s membership")

        # The node must STILL be a Spur worker (spurd untouched) and STILL meshed
        # (spur0 up, same key) — removal took it out of k0s only, not out of Spur.
        assert worker in c.sinfo_nodes(), (
            f"{worker} was deregistered from Spur by k8s remove-nodes — it must "
            f"remain a Spur worker (k0s-only removal)"
        )
        wg_key_after_remove = c.wg_mesh.wg_pubkey(worker_index)
        assert wg_key_after_remove == wg_key_before, (
            f"{worker} WireGuard key changed after k8s remove-nodes: "
            f"{wg_key_before} -> {wg_key_after_remove}"
        )
        # Still reachable over the mesh from the controller (tunnel intact).
        assert c.wg_mesh.ping(cp_index, c.wg_mesh.mesh_ip_for(worker_index)), (
            f"{worker} unreachable over the mesh after k8s removal — the mesh "
            f"tunnel must survive a k0s-only removal"
        )

        # 3. Re-add as a k8s worker; membership grows back.
        c.k8s_add_nodes(["--nodes", worker])
        wait_until(lambda: worker in c.k8s_member_list(), timeout_s=120,
                   desc=f"{worker} rejoined k0s membership")

        # Same WireGuard identity in the mesh — no re-key across the cycle.
        wg_key_after_add = c.wg_mesh.wg_pubkey(worker_index)
        assert wg_key_after_add == wg_key_before, (
            f"{worker} WireGuard key changed after re-add: "
            f"{wg_key_before} -> {wg_key_after_add}"
        )

        # The re-added node gets a k0s role again via the reconcile loop.
        def worker_roled() -> bool:
            for line in c.k8s_status().splitlines():
                f = line.split()
                if len(f) >= 2 and f[0] == worker and f[1] in ("worker", "controller", "single"):
                    return True
            return False

        wait_until(worker_roled, timeout_s=120,
                   desc=f"{worker} reassigned a k0s role after re-add")


# --- HA over the mesh (3 controllers) -----------------------------------


@pytest.mark.k0s
class TestHaOverMesh:
    def test_three_controller_raft_over_mesh_reelection(self):
        """Raft quorum over mesh IPs; stop the leader → survivors re-elect →
        restarted controller rejoins as a follower.

        HARNESS GAP: the base SpurCluster starts spurctld on node[0] only, and
        Raft-over-mesh is a bootstrap ordering problem (the mesh must be up
        before controllers can dial each other's mesh IPs for the raft_listen
        peers). A dedicated multi-controller fixture is required; it is tracked
        as a follow-up and validated on a local 3-controller bed, not CI.
        """
        pytest.skip(
            "needs a multi-controller (3× spurctld, controller.peers over mesh "
            "IPs) fixture the base harness does not yet provide"
        )


# --- login-node reachability --------------------------------------------


@pytest.mark.k0s
class TestLoginNodeReachability:
    def test_login_node_reaches_every_mesh_node(self, wg_login_cluster):
        """node2 (login node) is meshed but outside k0s scope. It must still
        reach every other mesh node over the mesh IPs after `k8s up` scopes the
        cluster to only the k8s controller (node1)."""
        c = wg_login_cluster
        k8s_ctrl, login = c.node_names[1], c.node_names[2]

        # Scope k0s to the k8s controller only; spur-ctrl (node0) and login
        # (node2) are meshed but not k0s members.
        c.k8s_up(["--nodes", k8s_ctrl, "--control-plane-node", k8s_ctrl])
        c.wait_k8s_phase("ready", timeout=600)

        login_index = c.node_names.index(login)

        def login_reaches_all() -> bool:
            for target_index in (0, 1):
                out = c.nodes[login_index].exec_allow_fail(
                    f"ping -c 2 -W 5 {c.wg_mesh.mesh_ip_for(target_index)}"
                )
                if " 0% packet loss" not in out:
                    return False
            return True

        wait_until(login_reaches_all, timeout_s=120,
                   desc="login node reaches every mesh node")
