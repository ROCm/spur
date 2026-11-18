#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Sourced by slurm-prolog.sh and slurm-epilog.sh.

# Prints the render ID of each GPU in SPUR_JOB_GPUS, one per line, and warns on
# stderr for any GPU it cannot map. Spur builds those ids from each GPU's KFD
# topology entry (encode_stable_id in crates/spur-devices); Spur 0.13-0.15 and
# 0.16+ number GPU partitions differently, so both forms are matched.
job_render_ids() {
    local -A render=()
    local -a gpus=()
    local props key value minor domain loc gpu
    for props in /sys/class/kfd/kfd/topology/nodes/*/properties; do
        [ -r "${props}" ] || continue
        minor=0 domain=0 loc=0
        while read -r key value; do
            case "${key}" in
                drm_render_minor) minor=${value} ;;
                domain) domain=${value} ;;
                location_id) loc=${value} ;;
            esac
        done < "${props}"
        (( minor > 0 )) || continue
        render[$(( (domain << 24) | (loc << 8) ))]=${minor}
        render[$(( (domain << 24) | ((loc & ~7) << 8) | (loc & 7) ))]=${minor}
    done
    IFS=',' read -ra gpus <<< "${SPUR_JOB_GPUS}"
    for gpu in "${gpus[@]}"; do
        [ -n "${gpu}" ] || continue
        if [ -n "${render[${gpu}]:-}" ]; then
            echo "${render[${gpu}]}"
        else
            echo "${0##*/}: job ${SLURM_JOB_ID}: no render ID for GPU ${gpu} in /sys/class/kfd" >&2
        fi
    done
}
