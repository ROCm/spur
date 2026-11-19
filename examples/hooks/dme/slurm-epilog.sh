#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Removes the per-GPU files slurm-prolog.sh wrote for the job.

EXPORT_DIR="/var/run/exporter"
[ -d "${EXPORT_DIR}" ] && [ -n "${SPUR_JOB_GPUS}" ] || exit 0

# Spur runs hooks through a file descriptor, so a hook cannot find files next to itself.
# shellcheck source=/dev/null
source "${SPUR_DME_HOOKS:-/usr/share/exporter}/common.sh" || exit 0

for id in $(job_render_ids); do
    rm -f "${EXPORT_DIR}/${id}"
done
