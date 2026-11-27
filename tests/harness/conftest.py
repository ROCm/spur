# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Make the native-host harness importable without the e2e conftest's cluster fixtures."""

import sys
from pathlib import Path

_E2E_DIR = Path(__file__).resolve().parents[1] / "native_host" / "e2e"
if str(_E2E_DIR) not in sys.path:
    sys.path.insert(0, str(_E2E_DIR))
