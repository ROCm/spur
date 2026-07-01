# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build portable Spur binaries on AlmaLinux 8 (glibc 2.28).
# cargo-chef caches dependency builds across source changes.
#
# Container image (CI, K8s):
#   docker build --target runtime -t spur:<tag> .
#
# Extract portable binaries to disk (nightly, release):
#   DOCKER_BUILDKIT=1 docker build --target dist --output type=local,dest=./dist .

FROM almalinux:8 AS chef

ARG PROTOC_VERSION=25.1
RUN dnf install -y unzip curl gcc make binutils && dnf clean all && \
    curl -fsSL "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip" \
        -o /tmp/protoc.zip && \
    unzip -o /tmp/protoc.zip -d /usr/local && \
    rm /tmp/protoc.zip

COPY rust-toolchain.toml .
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup show

RUN cargo install cargo-chef --locked --version 0.1.77

WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY . .
RUN cargo build --release --locked \
    --bin spur \
    --bin spurctld \
    --bin spurd \
    --bin spur-k8s-operator

RUN echo "=== Required GLIBC versions ===" && \
    for bin in spur spurctld spurd spur-k8s-operator; do \
        MAX=$(objdump -T target/release/${bin} 2>/dev/null \
            | grep -oP 'GLIBC_\d+\.\d+' | sort -uV | tail -1); \
        echo "  ${bin}: requires ${MAX:-none}"; \
    done

FROM ubuntu:24.04 AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/spur /usr/local/bin/
COPY --from=builder /build/target/release/spurctld /usr/local/bin/
COPY --from=builder /build/target/release/spurd /usr/local/bin/
COPY --from=builder /build/target/release/spur-k8s-operator /usr/local/bin/

RUN groupadd --gid 1001 spur && useradd --uid 1001 --gid spur --no-create-home --shell /usr/sbin/nologin spur
USER spur

# Multi-binary image: Kubernetes manifests must set container command per workload
# (e.g. spurctld, spur-k8s-operator, spurd). Workloads that need root (e.g. spurd
# for seccomp/Landlock) should override with securityContext.runAsUser: 0.

FROM scratch AS dist
COPY --from=builder /build/target/release/spur /bin/
COPY --from=builder /build/target/release/spurctld /bin/
COPY --from=builder /build/target/release/spurd /bin/
COPY --from=builder /build/target/release/spur-k8s-operator /bin/
