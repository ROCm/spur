#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Removes the per-GPU files slurm-prolog.sh wrote for the job.

EXPORT_DIR="/var/run/exporter"
[ -d "${EXPORT_DIR}" ] && [ -n "${SPUR_JOB_GPUS}" ] || exit 0

# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

for id in $(job_render_ids); do
    rm -f "${EXPORT_DIR}/${id}"
done
