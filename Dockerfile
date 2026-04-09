# Minimal Spur runtime image — downloads pre-built release binaries.
#
# Build:
#   docker build -t spur .
#   docker build --build-arg VERSION=nightly -t spur:nightly .
#   docker build --build-arg VERSION=v0.1.0 -t spur:v0.1.0 .
#
# Run:
#   docker run --rm spur sinfo
#   docker run -d --name spurctld -p 6817:6817 spur spurctld --listen=[::]:6817

FROM ubuntu:22.04

ARG VERSION=latest
ARG REPO=ROCm/spur

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl util-linux \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    if [ "$VERSION" = "latest" ]; then \
        TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | cut -d'"' -f4); \
        TARBALL="spur-${TAG}-linux-amd64.tar.gz"; \
    elif [ "$VERSION" = "nightly" ]; then \
        TAG=nightly; \
        TARBALL=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/tags/nightly" \
            | grep '"name"' | grep '\.tar\.gz"' | grep -v sha256 | head -1 | cut -d'"' -f4); \
    else \
        TAG="$VERSION"; \
        TARBALL="spur-${TAG}-linux-amd64.tar.gz"; \
    fi; \
    curl -fsSL "https://github.com/${REPO}/releases/download/${TAG}/${TARBALL}" -o /tmp/spur.tar.gz; \
    tar xzf /tmp/spur.tar.gz -C /tmp; \
    cp /tmp/spur-*/bin/spur /tmp/spur-*/bin/spurctld /tmp/spur-*/bin/spurd \
       /tmp/spur-*/bin/spurdbd /tmp/spur-*/bin/spurrestd /usr/local/bin/; \
    rm -rf /tmp/spur*; \
    spur --help >/dev/null 2>&1 || true

# Slurm-compat symlinks
RUN for cmd in sbatch srun squeue scancel sinfo sacct scontrol; do \
        ln -s /usr/local/bin/spur /usr/local/bin/$cmd; \
    done

ENTRYPOINT ["spur"]
