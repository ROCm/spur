#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Labels each GPU of the job for the AMD Device Metrics Exporter: one file per
# GPU, named by its render ID and naming only that GPU, because the exporter
# clears only the GPU a file is named after when the file is deleted.

EXPORT_DIR="/var/run/exporter"
[ -d "${EXPORT_DIR}" ] && [ -n "${SPUR_JOB_GPUS}" ] || exit 0

# Spur runs hooks through a file descriptor, so a hook cannot find files next to itself.
# shellcheck source=/dev/null
source "${SPUR_DME_HOOKS:-/usr/share/exporter}/common.sh" || exit 0

for id in $(job_render_ids); do
    printf '{"SLURM_JOB_ID":"%s","SLURM_JOB_USER":"%s","SLURM_JOB_PARTITION":"%s","SLURM_CLUSTER_NAME":"%s","CUDA_VISIBLE_DEVICES":"%s"}' \
        "${SLURM_JOB_ID}" "${SPUR_JOB_USER}" "${SLURM_JOB_PARTITION}" "${SPUR_CLUSTER_NAME:-${SLURM_CLUSTER_NAME:-}}" "${id}" \
        > "${EXPORT_DIR}/${id}"
done
