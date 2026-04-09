#!/bin/bash
# Build portable Spur binaries using manylinux_2_28 Docker image (glibc 2.28)
#
# Outputs binaries to dist/bin/ in the repo root.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "Building portable Spur binaries (manylinux_2_28, glibc >= 2.28)..."
DOCKER_BUILDKIT=1 docker build \
    -f "${SCRIPT_DIR}/Dockerfile.manylinux" \
    --target dist \
    --output "type=local,dest=${REPO_ROOT}/dist" \
    "${REPO_ROOT}"

echo ""
echo "Portable binaries:"
ls -lh "${REPO_ROOT}/dist/bin/"
echo ""
echo "Compatible with Ubuntu 20.04+, Debian 10+, RHEL 8+, and any Linux with glibc >= 2.28"
