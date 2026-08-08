# 50-rdma — expose the backend RoCEv2 fabric to pods (distributed inference)

The GPU workers each have **8× AMD Pensando (ionic) 400 GbE RoCEv2 NICs** — one per MI355X GPU,
rail-optimized (3.2 Tb/s/node), separate from the 200 G frontend NIC that carries `spur0`. This
fabric is the **data plane for distributed inference** (RCCL collectives for multi-node TP/PP, and
RDMA KV-cache transfer for prefill/decode disaggregation via Mooncake/MoRI).

**Critical rule: RDMA does NOT go over `spur0`.** WireGuard can't carry RDMA and would destroy
bandwidth/latency. The RoCE fabric is attached to pods as a **separate, direct** path (host NICs or
Multus secondary interfaces); only the TCP rendezvous/bootstrap uses the pod network.

This is AMD's documented MI355X + 8×-Pensando config — see the ROCm guides for
[SGLang PD-disaggregation](https://rocm.docs.amd.com/projects/ai-developer-hub/en/latest/notebooks/inference/SGlang_PD_Disagg_On_AMD_GPU.html)
and [SGLang distributed with Mooncake](https://rocm.docs.amd.com/en/latest/how-to/rocm-for-ai/inference/benchmark-docker/sglang-distributed.html).

## Prerequisites (host / fabric — confirm with the provider)

1. **Assign RoCEv2 rail IPs** to the 8 backend netdevs on every node (they are currently un-IP'd;
   RoCEv2 v2 GIDs are derived from the netdev IP). Rail-optimized: one subnet per rail index across
   nodes (e.g. rail-0 = `enP2p0s9` on every node in `10.200.0.0/24`, rail-1 in `10.200.1.0/24`, …).
   The provider may already own this; confirm before assuming.
2. **Lossless/managed RoCE**: PFC or DCQCN/ECN configured on the NICs **and** the switches. This is
   a fabric-wide setting — confirm your provider's fabric config.
3. **GPUDirect RDMA**: present here (amdgpu dmabuf + `ionic_rdma` peer/ATS). The serving image must
   ship RCCL + the RDMA transport (see env below).
4. **RDMA netns mode**: host is `shared`. For per-pod isolation (fractional/multi-tenant), switch to
   `rdma system set netns exclusive`; for whole-node jobs, `shared` + hostNetwork is fine.

## Two exposure models

- **A. Whole-node (simplest, recommended for distributed serving):** `hostNetwork: true` — the pod
  sees all 8 backend NICs + `/dev/infiniband` directly. No Multus needed. Best for a node dedicated
  to one multi-GPU serving job. Add the `amd.com/roce` device-plugin resource only if you want the
  scheduler to gate on RDMA availability.
- **B. Isolated / fractional (Multus + SR-IOV):** keep Calico as the primary CNI; add **Multus** +
  the **SR-IOV Network Device Plugin** (`isRdma: true`) advertising `amd.com/roce_ionic`, and a
  **NetworkAttachmentDefinition** per rail. Pods request the RDMA resource + a `k8s.v1.cni.cncf.io/networks`
  annotation and get only their allocated rails + RDMA cdevs. Required for multi-tenant nodes.

Files here (`sriov-rdma-device-plugin.yaml`, `roce-net-attach-def.yaml`) implement model B; the
`sglang-disagg-mori.yaml` (PD-disaggregation) and `sglang-2node-tp.yaml` (tensor-parallel) examples
show both a hostNetwork variant and the annotations.

## RCCL / RoCE env (in the serving pod)

```
NCCL_IB_DISABLE=0
NCCL_IB_HCA=ionic_0,ionic_1,ionic_2,ionic_3,ionic_4,ionic_5,ionic_6,ionic_7   # the 8 RoCE devices
NCCL_IB_GID_INDEX=3            # RoCEv2 GID (confirm the v2 gid index via `show_gids`)
NCCL_SOCKET_IFNAME=eth0        # bootstrap/rendezvous over the POD network (Calico), not the RoCE NICs
NCCL_IB_PCI_RELAXED_ORDERING=1
```
For sglang PD-disaggregation, also pass `--disaggregation-ib-device ionic_0,…` and use Mooncake for
the KV-cache RDMA transport (per the ROCm guide).

## SPUR angle

SPUR should become **rail/topology-aware** — model NIC↔GPU affinity (which ionic NIC is PCIe-local
to which GPU) so multi-GPU jobs get rail-aligned NICs (GPU-topology scheduling on the SPUR roadmap; the
proto already carries `peer_gpus`/`link_type`). The k8s exposure above works independently of that;
topology-awareness is an optimization SPUR adds later.
