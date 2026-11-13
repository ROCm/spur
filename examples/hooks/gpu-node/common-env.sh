#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Shared Spur/Slurm environment normalization for hook scripts.
# Source this at the top of every hook:  source /opt/spur/hooks/common-env.sh

set -euo pipefail

SPUR_ETC="${SPUR_ETC:-/opt/spur/etc}"
SPUR_HOOKS="${SPUR_HOOKS:-/opt/spur/hooks}"

export SPUR_JOB_ID="${SPUR_JOB_ID:-${SLURM_JOB_ID:-}}"
export SLURM_JOB_ID="${SLURM_JOB_ID:-${SPUR_JOB_ID:-}}"

export SPUR_JOB_UID="${SPUR_JOB_UID:-${SLURM_JOB_UID:-}}"
export SLURM_JOB_UID="${SLURM_JOB_UID:-${SPUR_JOB_UID:-}}"

export SPUR_JOB_USER="${SPUR_JOB_USER:-${SLURM_JOB_USER:-}}"
export SLURM_JOB_USER="${SLURM_JOB_USER:-${SPUR_JOB_USER:-}}"

export SPUR_JOB_PARTITION="${SPUR_JOB_PARTITION:-${SLURM_JOB_PARTITION:-}}"
export SLURM_JOB_PARTITION="${SLURM_JOB_PARTITION:-${SPUR_JOB_PARTITION:-}}"

export SPUR_JOB_GPUS="${SPUR_JOB_GPUS:-${SLURM_JOB_GPUS:-}}"
export SLURM_JOB_GPUS="${SLURM_JOB_GPUS:-${SPUR_JOB_GPUS:-}}"

export SPUR_SCRIPT_CONTEXT="${SPUR_SCRIPT_CONTEXT:-${SLURM_SCRIPT_CONTEXT:-}}"
export SLURM_SCRIPT_CONTEXT="${SLURM_SCRIPT_CONTEXT:-${SPUR_SCRIPT_CONTEXT:-}}"

export CUDA_VISIBLE_DEVICES="${CUDA_VISIBLE_DEVICES:-${ROCR_VISIBLE_DEVICES:-${SPUR_JOB_GPUS:-}}}"
export ROCR_VISIBLE_DEVICES="${ROCR_VISIBLE_DEVICES:-${CUDA_VISIBLE_DEVICES:-}}"

if [[ -z "${SLURM_CLUSTER_NAME:-}" && -z "${SPUR_CLUSTER_NAME:-}" ]]; then
    conf="${SPUR_ETC}/spur.conf"
    if [[ -f "$conf" ]]; then
        _cn=$(
            grep -E '^[[:space:]]*cluster_name[[:space:]]*=' "$conf" \
                | head -1 \
                | sed -E 's/.*=[[:space:]]*"([^"]+)".*/\1/'
        )
        export SLURM_CLUSTER_NAME="${_cn:-spur-cluster}"
    else
        export SLURM_CLUSTER_NAME="spur-cluster"
    fi
    export SPUR_CLUSTER_NAME="${SLURM_CLUSTER_NAME}"
else
    export SLURM_CLUSTER_NAME="${SLURM_CLUSTER_NAME:-${SPUR_CLUSTER_NAME:-spur-cluster}}"
    export SPUR_CLUSTER_NAME="${SPUR_CLUSTER_NAME:-${SLURM_CLUSTER_NAME}}"
fi

export SPUR_CGROUP_ROOT="${SPUR_CGROUP_ROOT:-/sys/fs/cgroup/spur}"
export SPUR_JOB_CGROUP="${SPUR_JOB_CGROUP:-${SPUR_CGROUP_ROOT}/job_${SPUR_JOB_ID}}"
export SPUR_PROC_ROOT="${SPUR_PROC_ROOT:-/proc}"
export SPUR_SYS_ROOT="${SPUR_SYS_ROOT:-/sys}"

export SPUR_DOCKER_LABEL="${SPUR_DOCKER_LABEL:-spur_job_id}"
export SPUR_DOCKER_AGE_SEC="${SPUR_DOCKER_AGE_SEC:-600}"
export SPUR_SPOOL="${SPUR_SPOOL:-/var/spool/spur}"
export SPUR_TMP_JOB_PREFIXES="${SPUR_TMP_JOB_PREFIXES:-/tmp/gha-runner-}"
export SPUR_HOOK_MIN_FREE_MB="${SPUR_HOOK_MIN_FREE_MB:-1024}"
export SPUR_HOOK_MIN_FREE_INODES="${SPUR_HOOK_MIN_FREE_INODES:-1000}"

# Prolog: exit 1 (drain node) if orphan GPU/container workloads remain after cleanup.
export SPUR_PROLOG_FAIL_ON_DIRTY="${SPUR_PROLOG_FAIL_ON_DIRTY:-1}"
# Prolog/epilog: attempt to kill orphan GPU PIDs and stale containers before failing.
export SPUR_HOOK_CLEAN_ORPHANS="${SPUR_HOOK_CLEAN_ORPHANS:-1}"

export SPUR_HOOK_DOCKER_TIMEOUT="${SPUR_HOOK_DOCKER_TIMEOUT:-30}"
export SPUR_HOOK_GPU_TIMEOUT="${SPUR_HOOK_GPU_TIMEOUT:-60}"
export SPUR_HOOK_RM_TIMEOUT="${SPUR_HOOK_RM_TIMEOUT:-300}"
export SPUR_HOOK_SETTLE_TIMEOUT="${SPUR_HOOK_SETTLE_TIMEOUT:-15}"

run_to() {
    local secs="$1"; shift
    timeout --kill-after=10 "$secs" "$@"
}

normalize_gpu_list() {
    local raw="${1:-}" value
    local -A seen=()
    raw="${raw//[[:space:]]/}"
    [[ -z "$raw" ]] && return 0
    IFS=',' read -r -a values <<<"$raw"
    for value in "${values[@]}"; do
        [[ "$value" =~ ^[0-9]+$ ]] || return 1
        [[ -n "${seen[$value]:-}" ]] && continue
        seen["$value"]=1
        printf '%s\n' "$value"
    done
}

# Agent sends 0 when the user omitted --mem / CPU count. That is "unset", not invalid.
normalize_optional_count() {
    local name="$1" value="${2:-}"
    if [[ -z "$value" || "$value" == "0" ]]; then
        printf ''
        return 0
    fi
    [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
        echo "invalid ${name}=${value}" >&2
        return 1
    }
    printf '%s' "$value"
}

require_job_context() {
    local gpu_lines
    [[ "${SPUR_JOB_ID:-}" =~ ^[0-9]+$ ]] || {
        echo "invalid or missing SPUR_JOB_ID" >&2
        return 1
    }
    [[ "${SPUR_JOB_UID:-}" =~ ^[0-9]+$ ]] || {
        echo "invalid or missing SPUR_JOB_UID" >&2
        return 1
    }
    SPUR_CPUS_ON_NODE=$(normalize_optional_count SPUR_CPUS_ON_NODE "${SPUR_CPUS_ON_NODE:-}") || return 1
    SPUR_JOB_MEMORY_MB=$(normalize_optional_count SPUR_JOB_MEMORY_MB "${SPUR_JOB_MEMORY_MB:-}") || return 1
    export SPUR_CPUS_ON_NODE SPUR_JOB_MEMORY_MB
    gpu_lines=$(normalize_gpu_list "${SPUR_JOB_GPUS:-}") || {
        echo "invalid SPUR_JOB_GPUS=${SPUR_JOB_GPUS:-}" >&2
        return 1
    }
    SPUR_JOB_GPUS=$(paste -sd, <<<"$gpu_lines")
    [[ "$SPUR_JOB_GPUS" == $'\n' ]] && SPUR_JOB_GPUS=""
    export SPUR_JOB_GPUS
    export SLURM_JOB_GPUS="$SPUR_JOB_GPUS"
    # Do not clobber an existing visibility list when the job did not receive GPUs.
    if [[ -n "$SPUR_JOB_GPUS" ]]; then
        export CUDA_VISIBLE_DEVICES="$SPUR_JOB_GPUS"
        export ROCR_VISIBLE_DEVICES="$SPUR_JOB_GPUS"
    fi
}

# Prolog populates these before calling cleanup helpers; epilog must see empty
# arrays (not unset) so set -u never treats "${ARRAY[@]:-}" as a single "" key.
# shellcheck disable=SC2034
SPUR_ACTIVE_JOBS=()
# shellcheck disable=SC2034
declare -A SPUR_ACTIVE_JOB_GPUS=()

# Shared cleanup helpers (containers, GPU PIDs, cgroup trees).
# shellcheck source=/dev/null
source "${SPUR_HOOKS}/hook-cleanup.sh"
