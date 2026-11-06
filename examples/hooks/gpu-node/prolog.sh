#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Spur agent Prolog entrypoint — readiness is authoritative; exporter is telemetry.
# Configure in spur.conf:  prolog = "/opt/spur/hooks/prolog.sh"

set -euo pipefail
HOOKS="${SPUR_HOOKS:-/opt/spur/hooks}"

"${HOOKS}/prolog-main.sh"
"${HOOKS}/exporter-prolog.sh" || {
    echo "[WARN] exporter prolog failed; job readiness passed" >&2
    exit 0
}
