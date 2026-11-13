#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

#
# Removes per-GPU exporter metadata written at prolog time.

# shellcheck source=/dev/null
source "${SPUR_HOOKS:-/opt/spur/hooks}/common-env.sh"

EXPORT_DIR="${EXPORT_DIR:-/var/run/exporter/}"

[[ -d ${EXPORT_DIR} ]] || exit 0

while read -r id; do
    rm -f "${EXPORT_DIR}/${id}"
done < <(gpu_render_ids "${SPUR_JOB_GPUS}")

exit 0
