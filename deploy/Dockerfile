# Multi-stage build: one image with all Spur binaries
# Builder uses manylinux_2_28 (glibc 2.28) for broad compatibility
FROM quay.io/pypa/manylinux_2_28_x86_64 AS builder

# Install protoc (AlmaLinux 8 ships protoc 3.5 which is too old for prost)
ARG PROTOC_VERSION=25.1
RUN yum install -y unzip && yum clean all && \
    curl -fsSL "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-x86_64.zip" \
        -o /tmp/protoc.zip && \
    unzip -o /tmp/protoc.zip -d /usr/local && \
    rm /tmp/protoc.zip

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build
COPY . .

# Build all workspace binaries in release mode
RUN cargo build --release \
    --bin spur \
    --bin spurctld \
    --bin spurd \
    --bin spurdbd \
    --bin spurrestd \
    --bin spur-k8s-operator

# Runtime image
FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    util-linux \
    && rm -rf /var/lib/apt/lists/*

# Copy all binaries
COPY --from=builder /build/target/release/spur /usr/local/bin/
COPY --from=builder /build/target/release/spurctld /usr/local/bin/
COPY --from=builder /build/target/release/spurd /usr/local/bin/
COPY --from=builder /build/target/release/spurdbd /usr/local/bin/
COPY --from=builder /build/target/release/spurrestd /usr/local/bin/
COPY --from=builder /build/target/release/spur-k8s-operator /usr/local/bin/

# Default to running the operator
ENTRYPOINT ["spur-k8s-operator"]
CMD ["run"]
