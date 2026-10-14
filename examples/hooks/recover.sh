#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
# Example auto-recovery hook for Spur.
# Configure in spur.conf:
#   [recovery]
#   program = "/etc/spur/recover.sh"
#
# The script receives SPUR_NODE_NAME, SPUR_DRAIN_REASON,
# SPUR_RECOVERY_PHASE, SPUR_RECOVERY_ATTEMPT, and SPUR_RECOVERY_MAX_ATTEMPTS.
# Exit 0 = phase complete (redispatch if labels remain).
# Exit 42 = permanent failure (stop immediately).
# Exit nonzero = retry (up to max_attempts).

set -euo pipefail

BKC_VERSION="${BKC_VERSION:-6.3.2}"

case "${SPUR_RECOVERY_PHASE:-}" in
  "")
    # Phase 1: reinstall the AMD BKC stack and reboot.
    echo "recovery: reinstalling BKC ${BKC_VERSION} on ${SPUR_NODE_NAME}"
    # /opt/amd/install-bkc.sh --version "$BKC_VERSION"

    # Persist the phase label (WAL-committed before reboot).
    spur node label --self "$SPUR_NODE_NAME" spur.recovery/phase=verify
    # reboot
    ;;

  verify)
    # Phase 2: post-reboot verification.
    echo "recovery: verifying ${SPUR_NODE_NAME}"

    if ! rocm-smi --showhw > /dev/null 2>&1; then
        echo "rocm-smi hardware check failed" >&2
        exit 1
    fi

    # All checks pass — return the node to service.
    spur node label --self "$SPUR_NODE_NAME" "spur.recovery/phase-"
    spur node undrain --self "$SPUR_NODE_NAME"
    ;;

  *)
    echo "unknown phase: ${SPUR_RECOVERY_PHASE}" >&2
    exit 1
    ;;
esac
