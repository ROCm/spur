# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# manylinux_2_28 (glibc 2.28) for broad compatibility.
# cargo-chef caches dependency builds across source changes.

FROM quay.io/pypa/manylinux_2_28_x86_64 AS chef

ARG PROTOC_VERSION=25.1
RUN yum install -y unzip && yum clean all && \
    curl -fsSL "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip" \
        -o /tmp/protoc.zip && \
    unzip -o /tmp/protoc.zip -d /usr/local && \
    rm /tmp/protoc.zip

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

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
    --bin spurdbd \
    --bin spurrestd \
    --bin spur-k8s-operator

FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/spur /usr/local/bin/
COPY --from=builder /build/target/release/spurctld /usr/local/bin/
COPY --from=builder /build/target/release/spurd /usr/local/bin/
COPY --from=builder /build/target/release/spurdbd /usr/local/bin/
COPY --from=builder /build/target/release/spurrestd /usr/local/bin/
COPY --from=builder /build/target/release/spur-k8s-operator /usr/local/bin/

ENTRYPOINT ["spur-k8s-operator"]
CMD ["run"]
