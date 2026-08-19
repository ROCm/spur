# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Hermetic image for building and linting the Spur documentation.
# Mirrors the Read the Docs environment (Python 3.12 + rocm-docs-core) and adds
# the ROCm doc linters (markdownlint-cli2 + pyspelling/aspell) so one image runs
# build + lint identically on a laptop and in CI.
#
# Build the image (from the repo root):
#   docker build -f docs/Dockerfile -t spur-docs .
# Build the HTML:
#   docker run --rm -v "$PWD:/work" -w /work spur-docs \
#     sphinx-build -W --keep-going -b html docs docs/_build/html
# Lint:
#   docker run --rm -v "$PWD:/work" -w /work spur-docs markdownlint-cli2 "docs/**/*.md"
#   docker run --rm -v "$PWD:/work" -w /work spur-docs pyspelling -c .spellcheck.yaml
FROM python:3.12-slim-bookworm

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

# aspell  -> spelling engine used by pyspelling
# nodejs  -> runtime for markdownlint-cli2
# git     -> rocm-docs-core reads git metadata during the build
RUN apt-get update && apt-get install -y --no-install-recommends \
      aspell aspell-en \
      nodejs npm \
      git \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g markdownlint-cli2@0.14.0

# Install the pinned doc toolchain first so the layer caches across content edits.
COPY docs/sphinx/requirements.txt /tmp/requirements.txt
RUN pip install -r /tmp/requirements.txt

# rocm-docs-core reads git metadata; allow the mounted repo regardless of owner.
RUN git config --global --add safe.directory '*'

WORKDIR /work

# Default: an HTML build with warnings promoted to errors (--keep-going so a
# single build reports every warning). Override the command to run a linter instead.
CMD ["sphinx-build", "-W", "--keep-going", "-b", "html", "docs", "docs/_build/html"]
