#!/bin/bash
# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Shared prolog/epilog cleanup helpers (sourced from common-env.sh).
#
# Slurm-like ownership model (multi-tenant safe):
#   - Every resource belongs to a spur JOB ID, never merely a Unix uid.
#   - Epilog cleans ONLY resources owned by the ending job.
#   - Prolog removes ONLY orphans (dead jobs) and never touches active jobs.
#   - Three users on one node: each job's epilog/prolog cycle is independent.
#
# Container ownership (Slurm-like; no app-specific name patterns):
#   1. Docker label spur_job_id (SPUR_DOCKER_LABEL)
#   2. Legacy slurm_job_id label (backward compatible)
#   3. Any container PID in spur/job_<id> cgroup
#   4. Container init started under a process in spur/job_<id> cgroup
# Prolog may retro-label unlabeled GPU containers tied to an active job via GPU overlap.
#
# GPU PID ownership:
#   1. PID in spur/job_<id> cgroup (or descendant)
#   2. PID inside a container owned by job <id>
#   3. Prolog: never kill GPU PIDs protected by ANY active job on the node

dk() {
    run_to "$SPUR_HOOK_DOCKER_TIMEOUT" docker "$@"
}

job_cgroup_dirs() {
    local job_id="$1" d base name
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 1
    base="${SPUR_CGROUP_ROOT}/job_${job_id}"
    for d in "$base" "${base}"_*; do
        [[ -d "$d" ]] || continue
        name="${d##*/}"
        [[ "$name" == "job_${job_id}" || "$name" =~ ^job_${job_id}(_[0-9]+)+$ ]] || continue
        printf '%s\n' "$d"
    done
}

job_cgroup_tree_dirs() {
    local job_id="$1" root
    while IFS= read -r root; do
        find "$root" -xdev -type d -print 2>/dev/null
    done < <(job_cgroup_dirs "$job_id")
}

job_cgroup_pids() {
    local job_id="$1" d pid
    while IFS= read -r d; do
        [[ -r "$d/cgroup.procs" ]] || continue
        while IFS= read -r pid; do
            [[ "$pid" =~ ^[1-9][0-9]*$ ]] && printf '%s\n' "$pid"
        done <"$d/cgroup.procs"
    done < <(job_cgroup_tree_dirs "$job_id")
}

spur_active_job_ids() {
    local d job_id
    for d in "${SPUR_CGROUP_ROOT}"/job_*; do
        [[ -d "$d" ]] || continue
        job_id="${d##*/job_}"
        job_id="${job_id%%_*}"
        [[ "$job_id" =~ ^[0-9]+$ ]] || continue
        job_cgroup_pids "$job_id" | grep -q . || continue
        printf '%s\n' "$job_id"
    done
}

spur_is_active_job() {
    local id="$1" j
    [[ "$id" =~ ^[0-9]+$ ]] || return 1
    for j in "${SPUR_ACTIVE_JOBS[@]}"; do
        [[ "$j" == "$id" ]] && return 0
    done
    return 1
}

pid_spur_job_id() {
    local pid="$1" line
    [[ -n "$pid" && "$pid" -gt 0 && -r "${SPUR_PROC_ROOT}/${pid}/cgroup" ]] || return 0
    while IFS= read -r line; do
        if [[ "$line" =~ /spur/job_([0-9]+)(_[0-9]+)*(/|$) ]]; then
            echo "${BASH_REMATCH[1]}"
            return 0
        fi
    done <"${SPUR_PROC_ROOT}/${pid}/cgroup"
}

container_job_label() {
    local cid="$1" labels key="${SPUR_DOCKER_LABEL:-spur_job_id}"
    labels=$(dk inspect --format '{{json .Config.Labels}}' "$cid" 2>/dev/null || echo "{}")
    if grep -q "\"${key}\"" <<<"$labels"; then
        grep -o "\"${key}\":\"[^\"]*\"" <<<"$labels" | head -1 | cut -d':' -f2 | tr -d '"'
        return 0
    fi
    if grep -q '"slurm_job_id"' <<<"$labels"; then
        grep -o '"slurm_job_id":"[^"]*"' <<<"$labels" | head -1 | cut -d':' -f2 | tr -d '"'
    fi
}

# True when container is tied to job_id via label or spur cgroup (Slurm semantics).
container_linked_to_job() {
    local cid="$1" job_id="$2" owner main_pid cg_pid pid
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 1

    owner=$(container_job_label "$cid")
    [[ "$owner" == "$job_id" ]] && return 0

    main_pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
    [[ "$(pid_spur_job_id "$main_pid")" == "$job_id" ]] && return 0

    for pid in $(dk top "$cid" --format '{{.PID}}' 2>/dev/null || true); do
        [[ "$(pid_spur_job_id "$pid")" == "$job_id" ]] && return 0
    done

    while read -r cg_pid; do
        [[ -n "$cg_pid" ]] || continue
        pid_is_descendant_of "$main_pid" "$cg_pid" && return 0
    done < <(job_cgroup_pids "$job_id")
    return 1
}

# Resolve which spur job owns a container, or empty if unknown.
container_owner_job_id() {
    local cid="$1" label main_pid cgroup_job pid job_id
    label=$(container_job_label "$cid")
    [[ -n "$label" ]] && { echo "$label"; return 0; }
    main_pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
    cgroup_job=$(pid_spur_job_id "$main_pid")
    [[ -n "$cgroup_job" ]] && { echo "$cgroup_job"; return 0; }
    for pid in $(dk top "$cid" --format '{{.PID}}' 2>/dev/null || true); do
        job_id=$(pid_spur_job_id "$pid")
        [[ -n "$job_id" ]] && { echo "$job_id"; return 0; }
    done
    return 0
}

container_owned_by_active_job() {
    local cid="$1" owner
    owner=$(container_owner_job_id "$cid")
    [[ -n "$owner" ]] && spur_is_active_job "$owner"
}

label_container_job() {
    local cid="$1" owner_job="$2"
    [[ -n "$owner_job" ]] || return 0
    dk update "--label-add=${SPUR_DOCKER_LABEL}=${owner_job}" "$cid" \
        >>"${SPUR_HOOK_LOG:-/dev/null}" 2>&1 || true
}

active_job_gpu_list() {
    local job_id="$1" pid line val
    while read -r pid; do
        [[ -n "$pid" && "$pid" -gt 0 && -r "${SPUR_PROC_ROOT}/${pid}/environ" ]] || continue
        while IFS= read -r line; do
            case "$line" in
                ROCR_VISIBLE_DEVICES=*|CUDA_VISIBLE_DEVICES=*)
                    val="${line#*=}"
                    [[ -n "$val" ]] && echo "$val" && return 0
                    ;;
            esac
        done < <(tr '\0' '\n' <"${SPUR_PROC_ROOT}/${pid}/environ" 2>/dev/null || true)
    done < <(job_cgroup_pids "$job_id")
}

# True if container config references any GPU index in gpu_list.
# /dev/kfd is deliberately NOT matched — it is shared by every ROCm container.
container_overlaps_gpu_list() {
    local cid="$1" gpu_list="$2" gpu render inspect
    [[ -n "$gpu_list" ]] || return 1
    inspect=$(dk inspect "$cid" 2>/dev/null || true)
    [[ -n "$inspect" ]] || return 1
    for gpu in $(tr ',' ' ' <<<"$gpu_list"); do
        [[ "$gpu" =~ ^[0-9]+$ ]] || continue
        render=$((128 + gpu))
        if grep -qE "(^|[^0-9])card${gpu}([^0-9]|\$)|(^|[^0-9])renderD${render}([^0-9]|\$)" <<<"$inspect"; then
            return 0
        fi
        if grep -qE "(CUDA|ROCR)_VISIBLE_DEVICES=([0-9]+,)*${gpu}(,[0-9]+)*\"" <<<"$inspect"; then
            return 0
        fi
    done
    return 1
}

gpu_lists_overlap() {
    local left="$1" right="$2" gpu
    [[ -n "$left" && -n "$right" ]] || return 1
    for gpu in $(tr ',' ' ' <<<"$left"); do
        [[ ",${right}," == *",${gpu},"* ]] && return 0
    done
    return 1
}

active_job_owning_container_gpus() {
    local cid="$1" job_id gpus
    for job_id in "${SPUR_ACTIVE_JOBS[@]}"; do
        [[ "$job_id" =~ ^[0-9]+$ ]] || continue
        gpus="${SPUR_ACTIVE_JOB_GPUS[$job_id]:-}"
        [[ -n "$gpus" ]] || continue
        if container_overlaps_gpu_list "$cid" "$gpus"; then
            echo "$job_id"
            return 0
        fi
    done
    return 1
}

pid_is_descendant_of() {
    local pid="$1" ancestor="$2" ppid
    [[ -n "$pid" && -n "$ancestor" && "$pid" -gt 0 && "$ancestor" -gt 0 ]] || return 1
    while [[ "$pid" -gt 1 ]]; do
        [[ "$pid" == "$ancestor" ]] && return 0
        ppid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ' || echo "")
        [[ -z "$ppid" || "$ppid" -le 0 ]] && break
        pid="$ppid"
    done
    return 1
}

pid_listed_in_container() {
    local pid="$1" cid="$2" tp
    [[ -n "$pid" && -n "$cid" ]] || return 1
    tp=$(dk top "$cid" --format '{{.PID}}' 2>/dev/null || true)
    grep -qx "$pid" <<<"$tp"
}

# Recursively collect pid and all descendants (full process tree).
collect_process_tree() {
    local pid="$1" child
    [[ -n "$pid" && "$pid" -gt 0 && -d "${SPUR_PROC_ROOT}/$pid" ]] || return 0
    echo "$pid"
    for child in $(ps -o pid= --ppid "$pid" 2>/dev/null || true); do
        collect_process_tree "$child"
    done
}

# True when pid must not be killed by prolog orphan cleanup (another live job).
pid_protected_by_active_job() {
    local pid="$1" owner cid main_pid job_id
    [[ -n "$pid" && "$pid" -gt 0 ]] || return 1
    owner=$(pid_spur_job_id "$pid")
    [[ -n "$owner" ]] && spur_is_active_job "$owner" && return 0
    for cid in $(dk ps -q 2>/dev/null || true); do
        if container_owned_by_active_job "$cid"; then
            main_pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
            pid_is_descendant_of "$pid" "$main_pid" && return 0
            pid_listed_in_container "$pid" "$cid" && return 0
            continue
        fi
        job_id=$(active_job_owning_container_gpus "$cid" || true)
        [[ -z "$job_id" ]] && continue
        main_pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
        pid_is_descendant_of "$pid" "$main_pid" && return 0
        pid_listed_in_container "$pid" "$cid" && return 0
    done
    return 1
}

kill_process_tree() {
    local pid="$1" child
    [[ -n "$pid" && "$pid" -gt 0 && -d "${SPUR_PROC_ROOT}/$pid" ]] || return 0
    for child in $(ps -o pid= --ppid "$pid" 2>/dev/null || true); do
        kill_process_tree "$child"
    done
    kill -TERM "$pid" 2>/dev/null || true
}

force_kill_process_tree() {
    local pid="$1" child
    [[ -n "$pid" && "$pid" -gt 0 && -d "${SPUR_PROC_ROOT}/$pid" ]] || return 0
    kill_process_tree "$pid"
    sleep 1
    for child in $(ps -o pid= --ppid "$pid" 2>/dev/null || true); do
        force_kill_process_tree "$child"
    done
    kill -KILL "$pid" 2>/dev/null || true
}

hook_kill_container() {
    local cid="$1" reason="$2" main_pid rc=0
    main_pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
    [[ -n "$main_pid" && "$main_pid" -gt 0 ]] && force_kill_process_tree "$main_pid"
    echo "[INFO] Killing container $cid ($reason)"
    dk kill "$cid" >>"${SPUR_HOOK_LOG:-/dev/null}" 2>&1 || true
    dk rm -f "$cid" >>"${SPUR_HOOK_LOG:-/dev/null}" 2>&1 || true
    dk inspect "$cid" >/dev/null 2>&1 && rc=1
    return "$rc"
}

containers_for_job() {
    local job_id="$1" cid
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 0

    dk ps -aq --filter "label=${SPUR_DOCKER_LABEL}=${job_id}" 2>/dev/null || true
    dk ps -aq --filter "label=slurm_job_id=${job_id}" 2>/dev/null || true

    for cid in $(dk ps -aq 2>/dev/null || true); do
        container_linked_to_job "$cid" "$job_id" && echo "$cid"
    done
    return 0
}

# True when this PID still belongs to job_id after the agent has removed the
# cgroup directory. Prefer cgroup membership; fall back to the job env the
# process inherited. Never abort on a PID that exits mid-read.
pid_belongs_to_job() {
    local pid="$1" job_id="$2" owner envfile
    [[ "$pid" =~ ^[1-9][0-9]*$ && "$job_id" =~ ^[0-9]+$ ]] || return 1
    [[ -d "${SPUR_PROC_ROOT}/${pid}" ]] || return 1
    owner=$(pid_spur_job_id "$pid" 2>/dev/null || true)
    [[ "$owner" == "$job_id" ]] && return 0
    envfile="${SPUR_PROC_ROOT}/${pid}/environ"
    [[ -e "$envfile" ]] || return 1
    if grep -zqx "SPUR_JOB_ID=${job_id}" "$envfile" >/dev/null 2>&1; then
        return 0
    fi
    if grep -zqx "SLURM_JOB_ID=${job_id}" "$envfile" >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

# Walk /proc after spurd has already rmdir'd /sys/fs/cgroup/spur/job_*.
# Limit to this job's UID so we do not read every process on a busy node.
proc_pids_for_job() {
    local job_id="$1" proc pid puid
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 0
    for proc in "${SPUR_PROC_ROOT}"/[0-9]*; do
        [[ -d "$proc" ]] || continue
        pid="${proc##*/}"
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || continue
        if [[ -n "${SPUR_JOB_UID:-}" ]]; then
            puid=$(awk '/^Uid:/{print $2; exit}' "${proc}/status" 2>/dev/null || true)
            if [[ -n "$puid" && "$puid" != "${SPUR_JOB_UID}" ]]; then
                continue
            fi
        fi
        pid_belongs_to_job "$pid" "$job_id" || continue
        pid_is_system_gpu_service "$pid" && continue
        printf '%s\n' "$pid"
    done
    return 0
}

collect_pids_for_job() {
    local job_id="$1" cid pid root_pid
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 0

    while read -r root_pid; do
        [[ -n "$root_pid" ]] || continue
        collect_process_tree "$root_pid"
    done < <(job_cgroup_pids "$job_id")

    while read -r pid; do
        [[ -n "$pid" ]] || continue
        collect_process_tree "$pid"
    done < <(proc_pids_for_job "$job_id")

    for cid in $(containers_for_job "$job_id" | sort -u); do
        pid=$(dk inspect --format '{{.State.Pid}}' "$cid" 2>/dev/null || echo 0)
        [[ -n "$pid" && "$pid" -gt 0 ]] && collect_process_tree "$pid"
        for pid in $(dk top "$cid" --format '{{.PID}}' 2>/dev/null || true); do
            [[ -n "$pid" ]] && collect_process_tree "$pid"
        done
    done

    for pid in $(gpu_pids_in_use 2>/dev/null || true); do
        pid_belongs_to_job "$pid" "$job_id" && collect_process_tree "$pid"
    done
    return 0
}

cleanup_job_containers() {
    local job_id="$1" cid containers rc=0
    containers=$(containers_for_job "$job_id" | sort -u | sed '/^$/d')
    if [[ -z "$containers" ]]; then
        echo "[INFO][${job_id}] No containers matched for job cleanup"
        return 0
    fi
    for cid in $containers; do
        hook_kill_container "$cid" "job ${job_id} epilog cleanup" || rc=1
    done
    return "$rc"
}

# True if PID is a host GPU service that must survive prolog/epilog.
pid_is_system_gpu_service() {
    local pid="$1" comm=""
    [[ -n "$pid" && "$pid" -gt 0 && -d "${SPUR_PROC_ROOT}/${pid}" ]] || return 1
    comm=$(awk '{ gsub(/[[:space:]]/, ""); print; exit }' "${SPUR_PROC_ROOT}/${pid}/comm" 2>/dev/null || true)
    [[ "$comm" == "gpuagent" ]]
}

gpu_pids_in_use() {
    local pid out
    command -v rocm-smi >/dev/null 2>&1 || return 0
    out=$(run_to "$SPUR_HOOK_GPU_TIMEOUT" rocm-smi --showpidgpus 2>/dev/null || true)
    [[ -n "$out" ]] || return 0
    while read -r pid; do
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || continue
        [[ -d "${SPUR_PROC_ROOT}/${pid}" ]] || continue
        pid_is_system_gpu_service "$pid" && continue
        printf '%s\n' "$pid"
    done < <(
        awk 'match($0, /PID[ \t]+[0-9]+/) {
            s = substr($0, RSTART, RLENGTH)
            sub(/PID[ \t]+/, "", s)
            print s
        }' <<<"$out" | sort -u
    )
    return 0
}

gpu_pids_for_list() {
    local gpu_list="${1:-}" out pid used_gpu gpu
    [[ -n "$gpu_list" ]] || return 0
    command -v rocm-smi >/dev/null 2>&1 || return 0
    out=$(run_to "$SPUR_HOOK_GPU_TIMEOUT" rocm-smi --showpidgpus 2>/dev/null || true)
    [[ -n "$out" ]] || return 0
    while read -r pid used_gpu; do
        [[ "$pid" =~ ^[1-9][0-9]*$ && "$used_gpu" =~ ^[0-9]+$ ]] || continue
        [[ -d "${SPUR_PROC_ROOT}/${pid}" ]] || continue
        pid_is_system_gpu_service "$pid" && continue
        for gpu in $(tr ',' ' ' <<<"$gpu_list"); do
            [[ "$gpu" =~ ^[0-9]+$ ]] || continue
            if [[ "$used_gpu" == "$gpu" ]]; then
                printf '%s\n' "$pid"
                break
            fi
        done
    done < <(
        awk '
            function emit(pid, text,    n, a, i) {
                n = split(text, a, /[^0-9]+/)
                for (i = 1; i <= n; i++)
                    if (a[i] ~ /^[0-9]+$/) print pid, a[i]
            }
            /^PID[ \t:=]+[0-9]+/ {
                line = $0
                pid = line
                sub(/^PID[ \t:=]+/, "", pid)
                sub(/[^0-9].*$/, "", pid)
                pending = ""

                if (line ~ /using[ \t]+0[ \t]+DRM device/) next
                if (line ~ /DRM device/) {
                    pending = pid
                    next
                }
                if (line ~ /using[ \t]+GPU/) {
                    sub(/^.*using[ \t]+GPU(s)?[ \t:]*/, "", line)
                    emit(pid, line)
                }
                next
            }
            pending != "" {
                emit(pending, $0)
                pending = ""
            }
        ' <<<"$out"
    )
}

# Epilog: kill GPU PIDs owned by THIS job only (never all PIDs for a uid).
cleanup_job_scoped_gpu_pids() {
    local job_id="$1" pid seen=""
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 0
    for pid in $(collect_pids_for_job "$job_id" | sort -u || true); do
        [[ -n "$pid" && "$pid" -gt 0 ]] || continue
        grep -qx "$pid" <<<"$seen" && continue
        seen+="$pid"$'\n'
        if gpu_pids_in_use | grep -qx "$pid"; then
            echo "[INFO][${job_id}] Killing GPU pid $pid (job-scoped)"
            force_kill_process_tree "$pid"
        fi
    done
}

# Prolog: kill GPU PIDs with no active job owner (multi-tenant safe).
cleanup_orphan_gpu_pids() {
    local gpu_list="${1:-}" pid owner count=0 pids
    if [[ -n "$gpu_list" ]]; then
        pids=$(gpu_pids_for_list "$gpu_list" | sort -u || true)
    else
        pids=$(gpu_pids_in_use | sort -u || true)
    fi
    for pid in $pids; do
        [[ -n "$pid" && "$pid" -gt 0 ]] || continue
        pid_protected_by_active_job "$pid" && continue
        owner=$(ps -o user= -p "$pid" 2>/dev/null | tr -d ' ' || echo "")
        echo "[INFO] Killing orphan GPU pid $pid (user=${owner:-unknown}, no active job owner)"
        force_kill_process_tree "$pid"
        count=$((count + 1))
    done
    echo "$count"
}

# Optional GPU-list argument intentionally supports both node-wide diagnostics
# and allocation-scoped prolog checks.
# shellcheck disable=SC2120
count_orphan_gpu_pids() {
    local gpu_list="${1:-}" pid n=0 pids
    if [[ -n "$gpu_list" ]]; then
        pids=$(gpu_pids_for_list "$gpu_list" | sort -u || true)
    else
        pids=$(gpu_pids_in_use | sort -u || true)
    fi
    for pid in $pids; do
        [[ -n "$pid" ]] || continue
        pid_protected_by_active_job "$pid" && continue
        n=$((n + 1))
    done
    echo "$n"
}

# Prolog: remove containers owned by dead jobs; never touch active job containers.
cleanup_stale_containers() {
    local launch_gpus="${1:-}" threshold="${2:-600}" now cid
    local created_raw created_epoch age owner active_owner
    local removed=0

    now=$(date +%s)
    for cid in $(dk ps -q 2>/dev/null || true); do
        owner=$(container_owner_job_id "$cid")
        if [[ -n "$owner" ]] && spur_is_active_job "$owner"; then
            continue
        fi
        active_owner=$(active_job_owning_container_gpus "$cid" || true)
        if [[ -n "$active_owner" ]]; then
            label_container_job "$cid" "$active_owner"
            continue
        fi

        created_raw=$(dk inspect --format '{{.Created}}' "$cid" 2>/dev/null || echo "")
        created_epoch=$(date -d "$created_raw" +%s 2>/dev/null || echo 0)
        age=$((now - created_epoch))

        if [[ -n "$owner" ]] && ! spur_is_active_job "$owner"; then
            hook_kill_container "$cid" "stale container owned by dead job=${owner}"
            removed=$((removed + 1))
            continue
        fi

        if (( age < threshold )); then
            continue
        fi

        if [[ -n "$launch_gpus" ]] && container_overlaps_gpu_list "$cid" "$launch_gpus"; then
            hook_kill_container "$cid" "stale unlabeled on launch GPUs ${launch_gpus} age=${age}s"
            removed=$((removed + 1))
        fi
    done
    echo "$removed"
}

remove_job_cgroup_dir() {
    local cg="$1"
    rmdir "$cg" 2>/dev/null && return 0
    # Real cgroupfs rmdir's even with control files. Unit-test fixtures are
    # ordinary directories, so drop empty mock control files then rmdir.
    if [[ -f "${cg}/cgroup.procs" ]] && ! grep -qE '^[0-9]+$' "${cg}/cgroup.procs"; then
        rm -f "${cg}/cgroup.procs" "${cg}/cgroup.kill" "${cg}/cgroup.freeze" 2>/dev/null || true
        rmdir "$cg" 2>/dev/null && return 0
    fi
    return 1
}

kill_spur_job_cgroup_pids() {
    local job_id="$1" cg pid deadline rc=0
    local -a roots=()
    mapfile -t roots < <(job_cgroup_dirs "$job_id")
    ((${#roots[@]} > 0)) || return 0

    for cg in "${roots[@]}"; do
        if [[ -e "${cg}/cgroup.kill" ]]; then
            echo "[INFO][${job_id}] Killing cgroup tree via ${cg}/cgroup.kill"
            echo 1 >"${cg}/cgroup.kill" 2>/dev/null || rc=1
        else
            if [[ -e "${cg}/cgroup.freeze" ]]; then
                echo 1 >"${cg}/cgroup.freeze" 2>/dev/null || true
            fi
            while read -r pid; do
                force_kill_process_tree "$pid"
            done < <(job_cgroup_pids "$job_id")
            if [[ -e "${cg}/cgroup.freeze" ]]; then
                echo 0 >"${cg}/cgroup.freeze" 2>/dev/null || true
            fi
        fi
    done

    deadline=$((SECONDS + SPUR_HOOK_SETTLE_TIMEOUT))
    while (( SECONDS < deadline )) && job_cgroup_pids "$job_id" | grep -q .; do
        sleep 1
    done
    while read -r pid; do
        [[ -d "${SPUR_PROC_ROOT}/$pid" ]] || continue
        echo "[WARN][${job_id}] Force-killing survivor pid $pid in cgroup"
        force_kill_process_tree "$pid"
    done < <(job_cgroup_pids "$job_id" | sort -u)

    while read -r cg; do
        remove_job_cgroup_dir "$cg" || rc=1
    done < <(job_cgroup_tree_dirs "$job_id" | awk '{ print length, $0 }' | sort -rn | cut -d' ' -f2-)
    job_cgroup_pids "$job_id" | grep -q . && rc=1
    return "$rc"
}

# Delete /dev/shm/psm_shm.* opened by THIS job's processes only.
cleanup_job_scoped_shm() {
    local job_id="$1" pid fd target seen=""
    [[ "$job_id" =~ ^[0-9]+$ ]] || return 0
    for pid in $(collect_pids_for_job "$job_id" | sort -u || true); do
        [[ -r "${SPUR_PROC_ROOT}/${pid}/fd" ]] || continue
        for fd in "${SPUR_PROC_ROOT}/${pid}"/fd/*; do
            [[ -e "$fd" ]] || continue
            target=$(readlink "$fd" 2>/dev/null) || continue
            case "$target" in
                /dev/shm/psm_shm.*)
                    grep -qx "$target" <<<"$seen" && continue
                    seen+="$target"$'\n'
                    echo "[INFO][${job_id}] Removing job-scoped PSM2 segment $target"
                    rm -f "$target" 2>/dev/null || true
                    ;;
            esac
        done
    done
}

# True if SPUR_JOB_UID owns a process in some OTHER job's cgroup on this node.
user_has_other_active_jobs() {
    local this_job="${1:-${SPUR_JOB_ID:-}}"
    local job_id pid puid
    while IFS= read -r job_id; do
        [[ "$job_id" == "$this_job" ]] && continue
        while IFS= read -r pid; do
            [[ -n "$pid" ]] || continue
            puid=$(awk '/^Uid:/{print $2; exit}' "${SPUR_PROC_ROOT}/${pid}/status" 2>/dev/null || echo "")
            [[ "$puid" == "${SPUR_JOB_UID}" ]] && return 0
        done < <(job_cgroup_pids "$job_id")
    done < <(spur_active_job_ids | sort -u)
    return 1
}

verify_job_cgroups_gone() {
    local job_id="$1" cg
    if job_cgroup_pids "$job_id" | grep -q .; then
        echo "[WARN][${job_id}] job cgroup processes remain"
        return 1
    fi
    while read -r cg; do
        remove_job_cgroup_dir "$cg" || true
    done < <(job_cgroup_tree_dirs "$job_id" | awk '{ print length, $0 }' | sort -rn | cut -d' ' -f2-)
    if job_cgroup_dirs "$job_id" | grep -q .; then
        echo "[WARN][${job_id}] empty job cgroup directories remain"
        return 1
    fi
    if proc_pids_for_job "$job_id" | grep -q .; then
        echo "[WARN][${job_id}] job processes remain in /proc after cgroup teardown"
        return 1
    fi
    return 0
}

verify_job_containers_gone() {
    local job_id="$1"
    if containers_for_job "$job_id" | grep -q .; then
        echo "[WARN][${job_id}] job-owned containers remain"
        return 1
    fi
    return 0
}

verify_job_gpus_released() {
    local job_id="$1" pid cid job_containers
    job_containers=$(containers_for_job "$job_id" | sort -u | sed '/^$/d')
    for pid in $(gpu_pids_in_use 2>/dev/null || true); do
        [[ -n "$pid" ]] || continue
        if pid_belongs_to_job "$pid" "$job_id"; then
            echo "[WARN][${job_id}] GPU pid $pid still tied to job after epilog"
            return 1
        fi
        for cid in $job_containers; do
            pid_listed_in_container "$pid" "$cid" && {
                echo "[WARN][${job_id}] GPU pid $pid still inside job container $cid after epilog"
                return 1
            }
        done
    done
    return 0
}

verify_gpus_idle() {
    local orphans
    orphans=$(count_orphan_gpu_pids "")
    if (( orphans > 0 )); then
        echo "[WARN] ${orphans} GPU PIDs still active with no active spur job owner"
        return 1
    fi
    echo "[INFO] GPU check passed: no unowned GPU processes on node"
    return 0
}
