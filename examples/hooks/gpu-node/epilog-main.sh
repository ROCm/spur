#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Post-job cleanup on Spur compute nodes (cgroup, Docker, GPU PIDs, scratch).
#
# Slurm-like semantics: epilog cleans ONLY resources owned by THIS job id.
# Safe when multiple users/jobs share a node — never kills by uid alone unless
# this user has no other active jobs on the node.

# shellcheck source=/dev/null
source "${SPUR_HOOKS:-/opt/spur/hooks}/common-env.sh"

LOGFILE="${SPUR_EPILOG_LOG:-/var/log/spur/epilog.log}"
# Used by functions sourced through common-env.sh.
# shellcheck disable=SC2034
SPUR_HOOK_LOG="$LOGFILE"
exec >> >(tee -a "$LOGFILE") 2>&1

echo "======== EPILOG START [${SPUR_JOB_ID}] on $(hostname) @ $(date) ========"

require_job_context || {
    echo "[ERROR] Missing or invalid job context; cleanup cannot be scoped safely."
    exit 1
}

cleanup_owned_path() {
    local d="$1" target rc=0
    [[ -e "$d" ]] || return 0
    while IFS= read -r target; do
        [[ -n "$target" ]] || continue
        echo "[INFO][${SPUR_JOB_ID}] Unmounting job-owned mount $target"
        run_to "$SPUR_HOOK_SETTLE_TIMEOUT" umount "$target" || rc=1
    done < <(findmnt -R "$d" -rno TARGET 2>/dev/null | awk '{ print length, $0 }' | sort -rn | cut -d' ' -f2-)
    findmnt -R "$d" -rno TARGET 2>/dev/null | grep -q . && rc=1
    if (( rc == 0 )); then
        echo "[INFO][${SPUR_JOB_ID}] Reclaiming $d ($(du -xsh "$d" 2>/dev/null | cut -f1))"
        run_to "$SPUR_HOOK_RM_TIMEOUT" rm -rf --one-file-system "$d" || rc=1
    fi
    [[ ! -e "$d" ]] || rc=1
    return "$rc"
}

job_owned_paths() {
    local root p d
    for root in "${SPUR_CONTAINER_DIR:-}" "${SPUR_SPOOL}/containers"; do
        [[ -n "$root" && "$root" == /* ]] || continue
        for d in "${root}/job_${SPUR_JOB_ID}" "${root}/step_${SPUR_JOB_ID}_"*; do
            [[ -e "$d" ]] && printf '%s\n' "$d"
        done
    done
    for p in ${SPUR_TMP_JOB_PREFIXES:-}; do
        d="${p}${SPUR_JOB_ID}"
        case "$d" in /tmp/?*|/var/tmp/?*|/local_scratch/?*) ;; *) continue ;; esac
        [[ -e "$d" ]] && printf '%s\n' "$d"
    done
    d="${SPUR_SPOOL}/job${SPUR_JOB_ID}"
    [[ -e "$d" ]] && printf '%s\n' "$d"
}

reclaim_job_paths() {
    local d rc=0
    while IFS= read -r d; do
        cleanup_owned_path "$d" || rc=1
    done < <(job_owned_paths)
    return "$rc"
}

cleanup_rc=0
mapfile -t owned_pids < <(collect_pids_for_job "${SPUR_JOB_ID}" | sort -un || true)

# Capture PSM2 segments while job PIDs are still discoverable.
cleanup_job_scoped_shm "${SPUR_JOB_ID}" || cleanup_rc=1

# 1. Kill every exact cgroup variant for THIS job, including job_<id>_<step>.
#    spurd often rmdir's the tree before this hook; missing dirs are not a
#    skip for leftover PIDs — /proc and Docker/GPU ownership still apply.
if job_cgroup_dirs "${SPUR_JOB_ID}" | grep -q .; then
    kill_spur_job_cgroup_pids "${SPUR_JOB_ID}" || cleanup_rc=1
else
    echo "[INFO][${SPUR_JOB_ID}] Job cgroup already removed by agent; scanning /proc for leftovers"
fi

# 2. Remove containers owned by THIS job (label or captured cgroup linkage).
if command -v docker >/dev/null 2>&1; then
    cleanup_job_containers "${SPUR_JOB_ID}" || cleanup_rc=1
else
    echo "[INFO][${SPUR_JOB_ID}] docker not installed, skipping container cleanup"
fi

# 3. Reap captured job process trees, including PIDs that survived cgroup rmdir.
mapfile -t owned_pids < <(
    {
        ((${#owned_pids[@]})) && printf '%s\n' "${owned_pids[@]}"
        collect_pids_for_job "${SPUR_JOB_ID}" || true
        proc_pids_for_job "${SPUR_JOB_ID}" || true
    } | awk 'NF && $1 ~ /^[1-9][0-9]*$/' | sort -un
)
for pid in "${owned_pids[@]}"; do
    [[ "$pid" =~ ^[1-9][0-9]*$ && -d "${SPUR_PROC_ROOT}/${pid}" ]] || continue
    echo "[WARN][${SPUR_JOB_ID}] Reaping captured survivor pid ${pid}"
    force_kill_process_tree "$pid"
done

if command -v rocm-smi >/dev/null 2>&1; then
    cleanup_job_scoped_gpu_pids "${SPUR_JOB_ID}" || cleanup_rc=1
    verify_job_gpus_released "${SPUR_JOB_ID}" || cleanup_rc=1
fi

# 4. Reclaim exact job/step rootfs, mounts, and configured job scratch.
reclaim_job_paths || cleanup_rc=1

# 5. Per-user sweeps only when this user has no other active jobs on the node.
#    Unconditional uid sweeps delete sibling jobs' PSM2 segments and /tmp files.
echo "[INFO][${SPUR_JOB_ID}] Performing filesystem cleanup"
if [[ "${SPUR_JOB_UID}" -lt 1000 ]]; then
    echo "[INFO][${SPUR_JOB_ID}] System UID ${SPUR_JOB_UID}; skipping UID-wide cleanup"
elif user_has_other_active_jobs "${SPUR_JOB_ID}"; then
    echo "[INFO][${SPUR_JOB_ID}] User ${SPUR_JOB_UID} still has other active jobs; skipping UID-wide cleanup"
else
    find /dev/shm -user "$SPUR_JOB_UID" -type f -name 'psm_shm.*' -delete 2>/dev/null || true
    find /tmp /var/tmp /usr/tmp /local_scratch -user "$SPUR_JOB_UID" -type f -mtime +1 -delete 2>/dev/null || true
fi

# Never drop caches on a shared node while other jobs are running — it hurts
# co-tenant performance and is not job-scoped cleanup.
if [[ "${SPUR_EPILOG_DROP_CACHES:-0}" == "1" ]] \
    && [[ "${SPUR_JOB_UID}" -ge 1000 ]] \
    && ! user_has_other_active_jobs "${SPUR_JOB_ID}"; then
    if [[ -w /proc/sys/vm/drop_caches ]]; then
        echo 3 >/proc/sys/vm/drop_caches
        echo "[INFO][${SPUR_JOB_ID}] Dropped system caches (no other active jobs for uid)."
    fi
fi

verify_job_cgroups_gone "${SPUR_JOB_ID}" || cleanup_rc=1
if command -v docker >/dev/null 2>&1; then
    verify_job_containers_gone "${SPUR_JOB_ID}" || cleanup_rc=1
fi
for pid in "${owned_pids[@]}"; do
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || continue
    if [[ -d "${SPUR_PROC_ROOT}/${pid}" ]]; then
        echo "[WARN][${SPUR_JOB_ID}] Captured job pid ${pid} survived epilog"
        cleanup_rc=1
    fi
done
if job_owned_paths | grep -q .; then
    echo "[WARN][${SPUR_JOB_ID}] Job-owned filesystem paths remain after cleanup"
    cleanup_rc=1
fi

echo "======== EPILOG END [${SPUR_JOB_ID}] on $(hostname) @ $(date) ========"
if (( cleanup_rc != 0 )); then
    echo "[ERROR][${SPUR_JOB_ID}] Cleanup verification failed; requesting node drain"
    exit 1
fi
exit 0
