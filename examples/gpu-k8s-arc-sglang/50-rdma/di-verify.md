# Verifying DI (distributed inference) with sglang's own DI tests

sglang ships a distributed-inference test we can reuse to validate this RoCE fabric once it's up:
**`.github/workflows/nightly-amd-mi355x-disagg.yml`** — "Nightly Test (AMD MI355X 2N 1P1D Disagg)",
a **2-node, 1-prefill / 1-decode disaggregation** benchmark on **exactly our hardware** (MI355X +
8× Pensando RoCE NICs, KV-cache transferred over RDMA via MoRI/Mooncake). Runner label:
`linux-sglang-mi35x-di`. It is driven via **Slurm**: the runner `salloc`s two MI355X nodes and runs
`scripts/ci/slurm/launch_mi355x.sh` (+ `generate_matrix.py` / `process_result.py` / `summarize.py`).

## Two ways to run it against our cluster

**Path 1 — Slurm-native via SPUR (natural fit; reuses sglang's DI CI almost unchanged).**
SPUR is Slurm-CLI compatible, so sglang's `salloc`/`sbatch`-based DI CI can target **SPUR** as the
scheduler — SPUR allocates the 2 MI355X nodes on the RoCE fabric and launches the 1P1D benchmark
directly on the nodes (bare, not k8s pods; RDMA over the backend fabric). Register a DI runner scale
set named `linux-sglang-mi35x-di` (a CPU/login-style runner that shells out to `salloc` against
SPUR). This is the strongest demonstration of SPUR's value — GPU-aware Slurm scheduling — *and*
validates the fabric.

> Reconciles with M2's "drain GPU nodes from the SPUR scheduler": the drain is not permanent. Use a
> **partition/time split** — nodes serve the k8s/ARC path by default, and a SPUR partition reclaims
> them for a DI benchmark run (or vice-versa). Permanent-drain + SPUR-salloc are mutually exclusive;
> pick per run. (This is exactly the SPUR↔k8s coexistence the roadmap is about.)

**Path 2 — k8s-native.**
Deploy the 2-node sglang as pods (`sglang-2node-tp.yaml`, or a prefill+decode pair for 1P1D) and run
sglang's disaggregation benchmark **client** against the served endpoints. More porting (sglang's DI
CI is Slurm-shaped, not k8s-shaped), but keeps DI inside the k8s serving model.

## The one gotcha sglang's own CI documents (applies directly to us)

The disagg workflow skips a node because *"its ionic RDMA driver ABI mismatches the container, so
**MoRI reports 'no active RDMA device'**."* These hosts run a recent `ionic_rdma` (e.g. 25.08.x) — the serving
/ benchmark **container's RDMA userspace (rdma-core + the ionic provider) must match that host ABI**,
or RDMA silently fails to initialize. **Verify first:** from inside the container, `ibv_devices`
must list `ionic_0..7` and `ibv_devinfo -d ionic_0` must show `PORT_ACTIVE`; if not, rebuild the
image against the matching ionic userspace before trusting any DI benchmark number.

## Acceptance for the RDMA milestone

1. `ibv_devinfo` inside a pod/job shows all 8 `ionic_*` PORT_ACTIVE (ABI matches).
2. A RoCE loopback/bandwidth test between two nodes (`ib_write_bw`/`perftest`) hits ~line rate.
3. sglang's 2N 1P1D disagg benchmark completes and its throughput/latency numbers are in range.
