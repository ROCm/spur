#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Verifies that AGENTS.md knowledge base docs stay in sync with source.
# Checks: port defaults (config.rs), enum variants (source↔doc), crate
# AGENTS.md presence, module file existence, FFI symbol names, and RPC
# names (architecture.md + spur-proto/AGENTS.md vs proto/slurm.proto).
# Does NOT check: WAL op lists, ports in architecture.md, FFI names in
# the AGENTS.md text (hard-coded list only), or all RPC name patterns.
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
        | grep -E '^\s+[A-Z][a-zA-Z0-9]+' \
        | sed 's/[,({].*//' \
        | awk '{$1=$1;print}' \
        | sort || true
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

extract_doc_enum_variants() {
    local agents_file="$1" enum_name="$2"
    # Extract the section for this enum and pull backtick-quoted or
    # comma-separated PascalCase names that look like variant names.
    # Strategy: find the heading/bullet that introduces the enum, take
    # until the next heading, then grab PascalCase words inside backticks
    # or in comma-separated variant lists.
    local section
    # Try ### heading first (spur-core style), then **`EnumName`**: bullet (spurd style)
    section=$(sed -n "/^### ${enum_name}\$/,/^### /p" "$agents_file" 2>/dev/null || true)
    if [ -z "$section" ]; then
        section=$(sed -n "/\*\*\`${enum_name}\`\*\*/,/^- \*\*\`[A-Z]/p" "$agents_file" 2>/dev/null || true)
    fi
    [ -z "$section" ] && return

    # Extract backtick-quoted PascalCase words and bare PascalCase words
    # in variant-list context (after colon, comma-separated)
    echo "$section" \
        | grep -oE '`[A-Z][a-zA-Z0-9]+`' \
        | tr -d '`' \
        | sort -u || true
}

check_enum_reverse() {
    local source_file="$1" enum_name="$2" agents_file="$3"
    [ -f "$source_file" ] || return
    [ -f "$agents_file" ] || return

    local source_variants
    source_variants=$(extract_enum_variants "$source_file" "$enum_name")
    [ -z "$source_variants" ] && return

    local doc_variants
    doc_variants=$(extract_doc_enum_variants "$agents_file" "$enum_name")
    [ -z "$doc_variants" ] && return

    # Collect all PascalCase names from the source file that are variants
    # of ANY enum, not just the target. This prevents false positives when
    # the doc section mentions variants of related enums.
    local all_source_names
    all_source_names=$(grep -oE '[A-Z][a-zA-Z0-9]+' "$source_file" | sort -u || true)

    while IFS= read -r dv; do
        [ -z "$dv" ] && continue
        case "$dv" in
            Job|Node|"${enum_name}") continue ;;
        esac
        if ! echo "$source_variants" | grep -qx "$dv"; then
            # Only flag if the name doesn't appear anywhere else in the source
            # (it may be a variant of a related enum mentioned in context)
            if ! echo "$all_source_names" | grep -qx "$dv"; then
                fail "ENUM: variant '$dv' documented for $enum_name in $agents_file but not found in source $source_file (stale doc?)"
            fi
        fi
    done <<< "$doc_variants"
}

check_enums() {
    check_enum "crates/spur-core/src/job.rs" "JobState" "crates/spur-core/AGENTS.md"
    check_enum "crates/spur-core/src/node.rs" "NodeState" "crates/spur-core/AGENTS.md"
    check_enum "crates/spurd/src/stepd.rs" "StepdRequest" "crates/spurd/AGENTS.md"
    check_enum "crates/spurd/src/stepd.rs" "StepdResponse" "crates/spurd/AGENTS.md"

    check_enum_reverse "crates/spur-core/src/job.rs" "JobState" "crates/spur-core/AGENTS.md"
    check_enum_reverse "crates/spur-core/src/node.rs" "NodeState" "crates/spur-core/AGENTS.md"
    check_enum_reverse "crates/spurd/src/stepd.rs" "StepdRequest" "crates/spurd/AGENTS.md"
    check_enum_reverse "crates/spurd/src/stepd.rs" "StepdResponse" "crates/spurd/AGENTS.md"
}

# --------------------------------------------------------------------------
# 3. Crate AGENTS.md presence
# --------------------------------------------------------------------------
check_agents_md_presence() {
    local crate_dir
    for crate_dir in crates/*/; do
        [ -d "${crate_dir}src" ] || continue
        if [ ! -f "${crate_dir}AGENTS.md" ]; then
            fail "AGENTS.md: crate $crate_dir has src/ but no AGENTS.md"
        fi
    done
}

# --------------------------------------------------------------------------
# 4. Cross-cutting knowledge docs presence
# --------------------------------------------------------------------------
check_knowledge_docs_presence() {
    local required_docs=(
        docs/knowledge/accounting/accounting.md
        docs/knowledge/common/architecture.md
        docs/knowledge/common/slurm-compatibility.md
        docs/knowledge/consensus/raft.md
        docs/knowledge/deployment/operations.md
        docs/knowledge/devices/gpu-discovery.md
        docs/knowledge/execution/containers-k0s.md
        docs/knowledge/execution/job-execution.md
        docs/knowledge/networking/wireguard-mesh.md
        docs/knowledge/scheduler/scheduling.md
        docs/knowledge/security/auth-model.md
        docs/knowledge/testing/testing.md
    )

    for doc in "${required_docs[@]}"; do
        if [ ! -f "$doc" ]; then
            fail "KNOWLEDGE: required doc $doc is missing"
        fi
    done
}

# --------------------------------------------------------------------------
# 5. Module file listings
# --------------------------------------------------------------------------
check_module_files() {
    local crate_dir agents_file
    for agents_file in crates/*/AGENTS.md; do
        crate_dir="$(dirname "$agents_file")"
        local src_dir="${crate_dir}/src"
        [ -d "$src_dir" ] || continue

        local rs_files
        mapfile -t rs_files < <(grep -oE '\b[a-z0-9_]+\.rs\b' "$agents_file" | sort -u)
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
# 6. FFI exported symbols
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
# 7. RPC names in proto
# --------------------------------------------------------------------------
check_rpc_names() {
    local proto="proto/slurm.proto"
    [ -f "$proto" ] || { fail "RPC: $proto not found"; return; }

    # Extract all RPC names from proto
    local proto_rpcs
    proto_rpcs=$(grep -oP 'rpc\s+\K[A-Za-z0-9]+' "$proto" | sort -u)

    # Check architecture.md RPC lists match proto.
    # Extract only from the bold-prefixed RPC listing lines (e.g. "**Job management:** SubmitJob, GetJobs, ...").
    local arch="docs/knowledge/common/architecture.md"
    if [ -f "$arch" ]; then
        if ! grep -q '## 5\. gRPC services' "$arch"; then
            fail "RPC: '## 5. gRPC services' heading not found in $arch (renamed?)"
            return
        fi
        local arch_rpcs
        arch_rpcs=$(sed -n '/## 5\. gRPC services/,/## 6\./p' "$arch" \
            | sed -e ':a' -e '/^\*\*/{' -e 'N' -e '/\n[^*#\n]/{ s/\n/ /; ba; }' -e '}' \
            | grep '^\*\*' \
            | tr ',' '\n' \
            | grep -oE '[A-Z][a-zA-Z0-9]+' \
            | sort -u || true)

        while IFS= read -r rpc; do
            [ -z "$rpc" ] && continue
            case "$rpc" in
                SlurmController|SlurmAccounting|SlurmAgent|PostgreSQL) continue ;;
                Served|Backed|Connects|Called|Networking|Diagnostics|Admission|Signals) continue ;;
                Job|Node|Partition|Cluster|Fairshare|Audit|QOS|WireGuard|Reservation|Entity) continue ;;
            esac
            if ! echo "$proto_rpcs" | grep -qx "$rpc"; then
                fail "RPC: $rpc listed in $arch gRPC services section but not found in $proto"
            fi
        done <<< "$arch_rpcs"
    fi

    # Check spur-proto/AGENTS.md RPCs exist in proto
    local proto_agents="crates/spur-proto/AGENTS.md"
    if [ -f "$proto_agents" ]; then
        # Extract RPC names from the AGENTS.md service listings
        local agent_rpcs
        mapfile -t agent_rpcs < <(
            grep -oE '\b(Submit|Get|Cancel|Complete|Suspend|Resume|Update|Requeue|Run|Exec|Drain|Deregister|Register|Create|Delete|List|Revoke|Heartbeat|Ping|Report|Apply|Prepare|Release|Stream|Start|Stop|Reconfigure|Reset|Attach|JobKeepalive|Signal|Validate|Modify|Add|Remove|Interactive)[A-Za-z0-9]+\b' "$proto_agents" \
                | sort -u || true
        )
        for rpc in "${agent_rpcs[@]}"; do
            [ -z "$rpc" ] && continue
            # Skip prose words that match the prefix pattern but aren't RPCs
            case "$rpc" in
                Cancelled|Started|Completed|Running|Suspended|Starts|Creates|Deletes|Returns|Registers|Updates|Reports|Streams|Releases|Prepares|Stops|Resets|Attaches|Drains|Lists|Submits|Gets|Runs|Cancels|Executes|Completes|Resumes|Suspends|Revokes|Applies|Reconfigures|Deregisters|Signals|InteractiveInput) continue ;;
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
check_agents_md_presence
check_knowledge_docs_presence
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
    echo "Fix the AGENTS.md / knowledge docs to match the current source."
    exit 1
fi

echo "All knowledge base checks passed."
