#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# GPU health check + pre-job cleanup before launch (Spur agent prolog).
#
# Multi-tenant safe (shared nodes with concurrent jobs):
#   - Remove containers/GPU PIDs owned by DEAD jobs only
#   - Never touch containers or GPU PIDs belonging to OTHER active jobs
#   - Refuse new work if the node is still dirty (exit 1 -> drain)
#
# Container ownership: spur cgroup + spur_job_id label (+ slurm_job_id legacy).

# shellcheck source=/dev/null
source "${SPUR_HOOKS:-/opt/spur/hooks}/common-env.sh"

LOGFILE="${SPUR_PROLOG_LOG:-/var/log/spur/prolog.log}"
# Used by functions sourced through common-env.sh.
# shellcheck disable=SC2034
SPUR_HOOK_LOG="$LOGFILE"
HOSTNAME=$(hostname)
JOB_ID="${SPUR_JOB_ID:-unknown}"
THRESHOLD="${SPUR_DOCKER_AGE_SEC:-600}"
LAUNCH_GPUS="${SPUR_JOB_GPUS:-}"

log() {
    printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOGFILE"
}

mkdir -p "$(dirname "$LOGFILE")"
require_job_context || {
    log "ERROR: invalid hook resource context"
    exit 1
}
JOB_ID="$SPUR_JOB_ID"
LAUNCH_GPUS="$SPUR_JOB_GPUS"

log "Prolog started for JobID=$JOB_ID on $HOSTNAME (context=${SPUR_SCRIPT_CONTEXT}, gpus=${LAUNCH_GPUS:-none})"

check_capacity_and_cgroups() {
    local online_cpus total_mem_mb controllers required
    online_cpus=$(nproc --all 2>/dev/null || echo 0)
    total_mem_mb=$(awk '/^MemTotal:/{print int($2 / 1024); exit}' "${SPUR_PROC_ROOT}/meminfo" 2>/dev/null || echo 0)
    if [[ -n "${SPUR_CPUS_ON_NODE:-}" ]] && (( SPUR_CPUS_ON_NODE > online_cpus )); then
        log "ERROR: requested CPUs=${SPUR_CPUS_ON_NODE}, but only ${online_cpus} CPUs are online"
        return 1
    fi
    if [[ -n "${SPUR_JOB_MEMORY_MB:-}" ]] && (( SPUR_JOB_MEMORY_MB > total_mem_mb )); then
        log "ERROR: requested memory=${SPUR_JOB_MEMORY_MB}MB exceeds host memory=${total_mem_mb}MB"
        return 1
    fi
    if [[ "$(stat -fc %T "${SPUR_CGROUP_ROOT}" 2>/dev/null || true)" != "cgroup2fs" ]]; then
        log "ERROR: ${SPUR_CGROUP_ROOT} is not on cgroup v2"
        return 1
    fi
    controllers=$(cat "${SPUR_CGROUP_ROOT}/cgroup.controllers" 2>/dev/null || true)
    for required in cpu cpuset memory; do
        grep -qw "$required" <<<"$controllers" || {
            log "ERROR: cgroup v2 controller '$required' is unavailable under ${SPUR_CGROUP_ROOT}"
            return 1
        }
    done
}

check_scratch() {
    local dir="$SPUR_CONTAINER_DIR" existing target avail_mb avail_inodes
    existing="$dir"
    while [[ ! -e "$existing" && "$existing" != "/" ]]; do
        existing=$(dirname "$existing")
    done
    target=$(findmnt -T "$existing" -rno TARGET 2>/dev/null || true)
    if [[ -z "$target" || "$target" == "/" ]]; then
        log "ERROR: SPUR_CONTAINER_DIR=$dir is not backed by a mounted scratch filesystem"
        return 1
    fi
    [[ -d "$dir" && -w "$dir" ]] || {
        log "ERROR: SPUR_CONTAINER_DIR=$dir is missing or not writable"
        return 1
    }
    avail_mb=$(df -Pm "$dir" | awk 'NR==2 {print $4}')
    avail_inodes=$(df -Pi "$dir" | awk 'NR==2 {print $4}')
    if [[ ! "$avail_mb" =~ ^[0-9]+$ ]] || (( avail_mb < SPUR_HOOK_MIN_FREE_MB )); then
        log "ERROR: scratch free space ${avail_mb:-unknown}MB is below ${SPUR_HOOK_MIN_FREE_MB}MB"
        return 1
    fi
    if [[ ! "$avail_inodes" =~ ^[0-9]+$ ]] || (( avail_inodes < SPUR_HOOK_MIN_FREE_INODES )); then
        log "ERROR: scratch free inodes ${avail_inodes:-unknown} is below ${SPUR_HOOK_MIN_FREE_INODES}"
        return 1
    fi
}

check_requested_gpus() {
    local gpu gpu_count rc
    [[ -n "$LAUNCH_GPUS" ]] || {
        log "CPU-only allocation; skipping ROCm checks"
        return 0
    }
    command -v rocm-smi >/dev/null 2>&1 || {
        log "ERROR: rocm-smi not found for GPU allocation"
        return 1
    }
    gpu_count=$(find "${SPUR_SYS_ROOT}/class/drm" -maxdepth 1 -type l -name 'card[0-9]*' 2>/dev/null | wc -l)
    for gpu in $(tr ',' ' ' <<<"$LAUNCH_GPUS"); do
        if (( gpu >= gpu_count )); then
            log "ERROR: requested GPU ${gpu} does not exist (detected ${gpu_count})"
            return 1
        fi
        rc=0
        run_to "$SPUR_HOOK_GPU_TIMEOUT" rocm-smi --device "$gpu" \
            --showtemp --showfan --showvbios >>"$LOGFILE" 2>&1 || rc=$?
        if (( rc != 0 )); then
            log "ERROR: health check failed for requested GPU ${gpu} (rc=${rc})"
            return 1
        fi
    done
}

check_capacity_and_cgroups || exit 1
[[ -z "${SPUR_CONTAINER_DIR:-}" ]] || check_scratch || exit 1
check_requested_gpus || exit 1

SPUR_ACTIVE_JOBS=()
declare -A SPUR_ACTIVE_JOB_GPUS=()
mapfile -t SPUR_ACTIVE_JOBS < <(spur_active_job_ids | sort -u)
for job_id in "${SPUR_ACTIVE_JOBS[@]}"; do
    [[ "$job_id" =~ ^[0-9]+$ ]] || continue
    gpus=$(active_job_gpu_list "$job_id" | head -1)
    if [[ -n "$gpus" ]]; then
        # shellcheck disable=SC2034
        SPUR_ACTIVE_JOB_GPUS["$job_id"]="$gpus"
    fi
    if [[ "$job_id" != "$SPUR_JOB_ID" ]] && gpu_lists_overlap "$LAUNCH_GPUS" "$gpus"; then
        log "ERROR: requested GPUs ${LAUNCH_GPUS} overlap live job ${job_id} GPUs ${gpus}"
        exit 1
    fi
done

if ((${#SPUR_ACTIVE_JOBS[@]} > 0)); then
    log "Active Spur jobs on node: ${SPUR_ACTIVE_JOBS[*]}"
else
    log "No active Spur job cgroups on node"
fi

if command -v docker >/dev/null 2>&1; then
    # Label unlabeled containers that clearly belong to active jobs.
    ps_rc=0
    dk ps -q >/dev/null 2>&1 || ps_rc=$?
    if (( ps_rc != 0 )); then
        if (( ps_rc == 124 )); then
            log "ERROR: 'docker ps' timed out after ${SPUR_HOOK_DOCKER_TIMEOUT}s - dockerd is wedged"
        else
            log "ERROR: 'docker ps' exited ${ps_rc} - dockerd unhealthy"
        fi
        exit 1
    fi

    for cid in $(dk ps -q 2>/dev/null || true); do
        [[ -n "$(container_job_label "$cid")" ]] && continue
        owner_job=$(active_job_owning_container_gpus "$cid" || true)
        [[ -n "$owner_job" ]] && label_container_job "$cid" "$owner_job"
    done

    if [[ "${SPUR_HOOK_CLEAN_ORPHANS}" == "1" ]]; then
        removed=$(cleanup_stale_containers "$LAUNCH_GPUS" "$THRESHOLD")
        log "Removed ${removed} stale container(s)"
    else
        log "SPUR_HOOK_CLEAN_ORPHANS=0, skipping stale container cleanup"
    fi
else
    log "WARNING: docker not found, skipping container cleanup"
fi

if [[ "${SPUR_HOOK_CLEAN_ORPHANS}" == "1" ]]; then
    if [[ -n "$LAUNCH_GPUS" ]]; then
        gpu_killed=$(cleanup_orphan_gpu_pids "$LAUNCH_GPUS")
        log "Killed ${gpu_killed} orphan GPU process tree(s) on requested GPUs ${LAUNCH_GPUS}"
    fi
else
    log "SPUR_HOOK_CLEAN_ORPHANS=0, skipping orphan GPU cleanup"
fi

remaining_orphans=0
[[ -z "$LAUNCH_GPUS" ]] || remaining_orphans=$(count_orphan_gpu_pids "$LAUNCH_GPUS")
if (( remaining_orphans > 0 )) && [[ "${SPUR_HOOK_CLEAN_ORPHANS}" == "1" ]]; then
    # A killed KFD process can remain visible briefly while the driver reaps it.
    sleep 1
    remaining_orphans=$(count_orphan_gpu_pids "$LAUNCH_GPUS")
fi
if (( remaining_orphans > 0 )); then
    log "WARNING: ${remaining_orphans} GPU PIDs still in use without active spur job owner"
    rocm-smi --showpidgpus >>"$LOGFILE" 2>&1 || true
    if [[ "${SPUR_PROLOG_FAIL_ON_DIRTY}" == "1" ]]; then
        log "ERROR: node is dirty; refusing job ${JOB_ID} (prolog exit 1 drains node)"
        exit 1
    fi
fi

log "Prolog completed successfully for JobID=$JOB_ID"
exit 0
