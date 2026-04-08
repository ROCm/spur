# Multi-stage build: one image with all Spur binaries
FROM rust:1.94.1 AS builder

WORKDIR /build
COPY . .

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# Build all workspace binaries in release mode
RUN cargo build --release \
    --bin spur \
    --bin spurctld \
    --bin spurd \
    --bin spurdbd \
    --bin spurrestd \
    --bin spur-k8s-operator

# Runtime image
FROM ubuntu:24.04

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
