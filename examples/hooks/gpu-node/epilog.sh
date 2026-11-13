#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Spur agent Epilog entrypoint — runs main epilog then exporter cleanup.
# Configure in spur.conf:  epilog = "/opt/spur/hooks/epilog.sh"

set -uo pipefail
HOOKS="${SPUR_HOOKS:-/opt/spur/hooks}"

main_rc=0
"${HOOKS}/epilog-main.sh" || main_rc=$?
"${HOOKS}/exporter-epilog.sh" || echo "[WARN] exporter epilog cleanup failed" >&2
exit "$main_rc"
