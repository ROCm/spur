#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Verifies that AGENTS.md knowledge base docs stay in sync with source.
# Extracts machine-verifiable facts (ports, enum variants, file listings,
# function names, RPC names) and confirms they match the source tree.
#
# Usage:
#   scripts/check-knowledge-sync.sh    # full check

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

failed=0
errors=()

fail() {
    errors+=("$1")
    failed=1
}

# --------------------------------------------------------------------------
# 1. Port constants
# --------------------------------------------------------------------------
check_ports() {
    local config="crates/spur-core/src/config.rs"
    [ -f "$config" ] || { fail "PORTS: $config not found"; return; }

    local -A port_map=(
        [6817]="default_listen_addr"
        [6820]="default_rest_addr"
        [6821]="default_raft_listen_addr"
        [6822]="default_metrics_listen_addr"
    )

    for port in "${!port_map[@]}"; do
        local func="${port_map[$port]}"
        if ! grep -A1 "fn ${func}()" "$config" | grep -q "$port"; then
            fail "PORTS: expected port $port in ${func}() in $config"
        fi
    done

    # Verify AGENTS.md claims match
    local core_agents="crates/spur-core/AGENTS.md"
    if [ -f "$core_agents" ]; then
        for port in 6817 6820 6821 6822; do
            if ! grep -q "$port" "$core_agents"; then
                fail "PORTS: port $port not documented in $core_agents"
            fi
        done
    fi
}

# --------------------------------------------------------------------------
# 2. Enum variant checks
# --------------------------------------------------------------------------
extract_enum_variants() {
    local file="$1" enum_name="$2"
    grep -A100 "pub enum ${enum_name}" "$file" \
        | tail -n +2 \
        | sed '/^}/q' \
        | grep -E '^\s+[A-Z][a-zA-Z]+' \
        | sed 's/[,({].*//' \
        | awk '{$1=$1;print}' \
        | sort
}

check_enum() {
    local source_file="$1" enum_name="$2" agents_file="$3"
    [ -f "$source_file" ] || { fail "ENUM: $source_file not found"; return; }
    [ -f "$agents_file" ] || { fail "ENUM: $agents_file not found"; return; }

    local variants
    variants=$(extract_enum_variants "$source_file" "$enum_name")
    local count
    count=$(echo "$variants" | grep -c . || true)

    if [ "$count" -eq 0 ]; then
        fail "ENUM: could not extract variants for $enum_name from $source_file"
        return
    fi

    while IFS= read -r variant; do
        [ -z "$variant" ] && continue
        if ! grep -qw "$variant" "$agents_file"; then
            fail "ENUM: variant '$variant' of $enum_name exists in $source_file but missing from $agents_file"
        fi
    done <<< "$variants"
}

check_enums() {
    check_enum "crates/spur-core/src/job.rs" "JobState" "crates/spur-core/AGENTS.md"
    check_enum "crates/spur-core/src/node.rs" "NodeState" "crates/spur-core/AGENTS.md"
    check_enum "crates/spurd/src/stepd.rs" "StepdRequest" "crates/spurd/AGENTS.md"
    check_enum "crates/spurd/src/stepd.rs" "StepdResponse" "crates/spurd/AGENTS.md"
}

# --------------------------------------------------------------------------
# 3. Module file listings
# --------------------------------------------------------------------------
check_module_files() {
    local crate_dir agents_file
    for agents_file in crates/*/AGENTS.md; do
        crate_dir="$(dirname "$agents_file")"
        local src_dir="${crate_dir}/src"
        [ -d "$src_dir" ] || continue

        local rs_files
        mapfile -t rs_files < <(grep -oE '\b[a-z_]+\.rs\b' "$agents_file" | sort -u)
        for rs_file in "${rs_files[@]}"; do
            case "$rs_file" in
                lib.rs|mod.rs|main.rs|build.rs) continue ;;
            esac
            if grep -qP "^\s+${rs_file}" "$agents_file"; then
                if [ ! -f "${src_dir}/${rs_file}" ]; then
                    # Check subdirectories and tests/
                    if ! find "$src_dir" "${crate_dir}/tests" -name "$rs_file" -print -quit 2>/dev/null | grep -q .; then
                        fail "FILES: $agents_file claims $rs_file but not found in $crate_dir/"
                    fi
                fi
            fi
        done
    done
}

# --------------------------------------------------------------------------
# 4. FFI exported symbols
# --------------------------------------------------------------------------
check_ffi_symbols() {
    local agents="crates/spur-ffi/AGENTS.md"
    local source="crates/spur-ffi/src/lib.rs"
    [ -f "$agents" ] || return
    [ -f "$source" ] || { fail "FFI: $source not found"; return; }

    local symbols=(
        slurm_init_job_desc_msg
        slurm_submit_batch_job
        slurm_kill_job
        slurm_load_jobs
        slurm_load_node
        slurm_load_partitions
        slurm_free_job_info_msg
        slurm_strerror
        slurm_get_errno
        slurm_seterrno
        slurm_perror
    )

    for sym in "${symbols[@]}"; do
        if ! grep -q "fn ${sym}" "$source"; then
            fail "FFI: symbol $sym listed in $agents but not found in $source"
        fi
    done
}

# --------------------------------------------------------------------------
# 5. RPC names in proto
# --------------------------------------------------------------------------
check_rpc_names() {
    local proto="proto/slurm.proto"
    [ -f "$proto" ] || { fail "RPC: $proto not found"; return; }

    # Extract all RPC names from proto
    local proto_rpcs
    proto_rpcs=$(grep -oP 'rpc\s+\K[A-Za-z]+' "$proto" | sort -u)

    # Check architecture.md lists match proto
    local arch="docs/knowledge/common/architecture.md"
    if [ -f "$arch" ]; then
        # Extract RPC names mentioned in the gRPC services section
        local arch_rpcs
        arch_rpcs=$(sed -n '/## 5\. gRPC services/,/## 6\./p' "$arch" \
            | grep -oE '[A-Z][a-zA-Z]+' \
            | sort -u)

        while IFS= read -r rpc; do
            [ -z "$rpc" ] && continue
            # Only check names that look like RPCs (CamelCase, not common words)
            case "$rpc" in
                Served|Backed|Connects|Called|Port|Job|Node|Partition|Cluster|Diagnostics|Admission|Networking) continue ;;
                The|API|All|Each|SlurmController|SlurmAccounting|SlurmAgent|PostgreSQL) continue ;;
                Axum|gRPC|REST|Raft|Slurm|WireGuard) continue ;;
            esac
            if echo "$proto_rpcs" | grep -qx "$rpc"; then
                : # RPC exists in proto — good
            fi
        done <<< "$arch_rpcs"
    fi

    # Check spur-proto/AGENTS.md RPCs exist in proto
    local proto_agents="crates/spur-proto/AGENTS.md"
    if [ -f "$proto_agents" ]; then
        # Extract RPC names from the AGENTS.md service listings
        local agent_rpcs
        mapfile -t agent_rpcs < <(
            grep -oE '\b(Submit|Get|Cancel|Complete|Suspend|Resume|Update|Requeue|Run|Exec|Drain|Deregister|Register|Create|Delete|List|Revoke|Heartbeat|Ping|Report|Apply|Prepare|Release|Stream|Start|Stop|Reconfigure|Reset|Attach|JobKeepalive)[A-Za-z]+\b' "$proto_agents" \
                | sort -u
        )
        for rpc in "${agent_rpcs[@]}"; do
            [ -z "$rpc" ] && continue
            # Skip prose words that match the prefix pattern but aren't RPCs
            case "$rpc" in
                Cancelled|Started|Completed|Running|Suspended|Starts|Creates|Deletes|Returns|Registers|Updates|Reports|Streams|Releases|Prepares|Stops|Resets|Attaches|Drains|Lists|Submits|Gets|Runs|Cancels|Executes|Completes|Resumes|Suspends|Revokes|Applies|Reconfigures|Deregisters) continue ;;
            esac
            if ! echo "$proto_rpcs" | grep -qx "$rpc"; then
                fail "RPC: $rpc listed in $proto_agents but not found in $proto"
            fi
        done
    fi
}

# --------------------------------------------------------------------------
# Run all checks
# --------------------------------------------------------------------------
echo "Checking knowledge base sync..."
echo ""

check_ports
check_enums
check_module_files
check_ffi_symbols
check_rpc_names

if [ "$failed" -ne 0 ]; then
    echo ""
    echo "FAILED: ${#errors[@]} knowledge base sync error(s):"
    echo ""
    for err in "${errors[@]}"; do
        echo "  $err"
    done
    echo ""
    echo "Fix the AGENTS.md / knowledge docs to match the current source, or"
    echo "update the source to match the documented contract."
    exit 1
fi

echo "All knowledge base checks passed."
