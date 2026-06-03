# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Deploy asset path resolution for E2E tests."""

import os
from pathlib import Path

_E2E_ROOT = Path(__file__).resolve().parent
_LOCAL_REPO_ROOT = _E2E_ROOT.parent.parent


def repo_root() -> Path:
    """Git checkout root for local runs (parent of ``tests/``)."""
    return _LOCAL_REPO_ROOT


def _deploy_root() -> Path:
    if env := os.environ.get("SPUR_DEPLOY_DIR"):
        return Path(env)
    return _LOCAL_REPO_ROOT / "deploy"


def k8s_deploy_dir() -> Path:
    return _deploy_root() / "k8s"


def native_host_deploy_dir() -> Path:
    return _deploy_root() / "native-host"
