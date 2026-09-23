# versions.lock — pin everything (fill in at deploy time)

Reproducibility depends on pinning. Record **exact versions** and, for images, the **digest**
(`repo@sha256:…`), not just a tag. Update this file in the same commit as any change that bumps a
version.

| Layer | Component | Pinned version / digest | Source of truth | Notes |
|---|---|---|---|---|
| Distro | k8s distribution | **k0s** `v1.32.4+k0s.0` (bump to the pinned release) | `10-cluster/k0sctl.yaml` | locked 2026-07-09 |
| Distro | Kubernetes version | 1.32.4 (from k0s `v1.32.4+k0s.0`; >=1.32 for AMD DRA) | k0s release | |
| Network | CNI | **Calico `bird`** (BGP native, no overlay) over spur0 | `10-cluster/k0sctl.yaml` | VXLAN = fallback; needs spur-net AllowedIPs |
| GPU | ROCm k8s-device-plugin | _TBD_ image `rocm/k8s-device-plugin@sha256:…` | `20-gpu/` | + node-labeller |
| GPU | (host) amdgpu / ROCm | in-box on Ubuntu 24.04 / k6.8 | node image | record `rocm-smi --version` |
| ARC | gha-runner-scale-set-controller | _TBD_ chart version | `30-arc/` | OCI chart |
| ARC | gha-runner-scale-set | _TBD_ chart version | `30-arc/` | must match controller |
| ARC | runner image | `ghcr.io/powderluv/sglang-runner@sha256:…` | `30-arc/Dockerfile` | ROCm + docker CLI + kubectl |
| ARC | docker:dind | `docker@sha256:…` | `30-arc/` | sidecar |
| Serving | sglang image | `rocm/sgl-dev@sha256:…` (gfx950/mi35x) | `40-serving/` | pin the mi35x tag by digest |
| Serving | model | _TBD (ungated default preferred)_ | `40-serving/` | + HF license note |
| Addons | ingress / LB / storage | _TBD (only if distro doesn't bundle)_ | `20/40` | k0s: ingress-nginx + MetalLB + local-path |

Also record once, at deploy time: `uname -r`, `containerd --version` (CDI needs >=2.0 for the DRA
path), `rocm-smi --version`, and the exact distro installer URL/commit.
