# gpu-k8s-arc-sglang — reproducible SPUR-hosted GPU K8s cluster + ARC + sglang

Goal: **every change we make to the cluster is captured as code or logged**, so bringing up a
similar cluster later is `git clone && ./bootstrap.sh` (and eventually `spur k8s up`). This is the
`gpu-k8s-arc-sglang` example in the **`spur-examples`** repo (e.g. `github.com/rocm/spur-examples`),
kept separate from the core `rocm/spur` code; it is the concrete seed of the SPUR `spur cluster` /
`spur k8s up` capability.

**Distro: k0s** — bootstrapped declaratively via `k0sctl` from `10-cluster/k0sctl.yaml`,
which doubles as the reproducible cluster spec SPUR will template. The **ARC + sglang integration is
the worked example** in this tree: `30-arc/` (controller + GPU dind runner scale set + CPU deploy
scale set for `github.com/powderluv/sglang`) and `40-serving/` (the AMD sglang serving manifest +
the fork CI/CD workflow that tests on GPU and auto-deploys on green). Follow it to reproduce the
whole GitHub-Actions-on-GPU-k8s loop, or swap `40-serving` for a different workload.

## Principles

1. **Declarative first.** Prefer a checked-in manifest / Helm values file / distro config over an
   ad-hoc command. If a distro (k0s) offers a declarative cluster spec (`k0sctl.yaml`), that file
   *is* the cluster definition.
2. **Append-only runbook.** Anything genuinely done by hand goes in `RUNBOOK.md` **as it happens**
   (command + host + result + how to undo), then gets promoted into a script under the numbered
   dirs. Nothing about the cluster lives only in someone's shell history.
3. **Pin everything.** `versions.lock.md` pins the distro/k8s version, every Helm chart version,
   and every image by **digest** (not just tag), so a rebuild is byte-reproducible.
4. **Idempotent + ordered.** `bootstrap.sh` re-runs safely; `teardown.sh` reverses in the correct
   order (ARC runner scale sets *before* the controller, k8s before the mesh, re-admit drained
   SPUR nodes last).
5. **No plaintext secrets.** GitHub App key / HF token via sealed-secrets or SOPS; only templates
   and `*.example` files are committed.

## Layout

```
README.md             # this file
RUNBOOK.md            # append-only chronological log of every cluster-mutating action (fill-in template)
versions.lock.md      # pinned distro/k8s + Helm chart versions + image digests
bootstrap.sh          # idempotent, ordered driver: network → cluster → addons → gpu → arc → serving
teardown.sh           # reverse-order teardown
Makefile              # up / gpu / arc / serve / down / capture
00-network/           # WireGuard mesh, firewall ports, sysctls, MTU
10-cluster/           # distro install config (k0sctl.yaml)
15-addons/            # upstream pieces k0s doesn't bundle (storage, ingress, sealed-secrets)
20-gpu/               # AMD device-plugin + node-labeller manifests, node labels, CDI
30-arc/               # ARC controller + runner scale-set Helm values, GitHub App secret template
40-serving/           # sglang serving manifest + fork CI workflow
50-rdma/              # RoCEv2 fabric for distributed inference (multi-node TP/PP, PD-disagg)
```

## How changes get captured during deployment

- Read-only probing → record in `RUNBOOK.md` under a dated heading.
- Each milestone lands its artifacts in the matching numbered dir **and** a
  one-line RUNBOOK entry referencing the file + the exact `kubectl/helm/curl` that applied it.
- After each session: `make capture` snapshots live cluster state (`kubectl get all -A -o yaml`,
  `helm list -A`, applied CRDs, node labels) into `state/<date>/` as a drift check against the tree.
