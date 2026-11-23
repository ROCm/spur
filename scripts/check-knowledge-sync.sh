#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Verifies that AGENTS.md knowledge base docs stay in sync with source.
# Checks: crate AGENTS.md presence, knowledge doc presence, module files
# named in crate AGENTS.md exist, and RPC names mentioned in
# spur-proto/AGENTS.md exist in proto/slurm.proto.
# Does NOT force docs to restate source (enum variants, ports, symbol
# or RPC lists); the knowledge base holds only what the code cannot show.
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
# 1. Crate AGENTS.md presence
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
# 2. Cross-cutting knowledge docs presence
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
# 3. Module file listings
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
# 4. RPC names in proto
# --------------------------------------------------------------------------
check_rpc_names() {
    local proto="proto/slurm.proto"
    [ -f "$proto" ] || { fail "RPC: $proto not found"; return; }

    # Extract all RPC names from proto
    local proto_rpcs
    proto_rpcs=$(grep -oP 'rpc\s+\K[A-Za-z0-9]+' "$proto" | sort -u)

    # Check RPC names mentioned in spur-proto/AGENTS.md exist in proto
    local proto_agents="crates/spur-proto/AGENTS.md"
    if [ -f "$proto_agents" ]; then
        local agent_rpcs
        mapfile -t agent_rpcs < <(
            grep -oE '\b(Submit|Get|Cancel|Complete|Suspend|Resume|Update|Requeue|Run|Exec|Drain|Deregister|Register|Create|Delete|List|Revoke|Heartbeat|Ping|Report|Apply|Prepare|Release|Stream|Start|Stop|Reconfigure|Reset|Attach|JobKeepalive|Signal|Validate|Modify|Add|Remove|Interactive)[A-Za-z0-9]+\b' "$proto_agents" \
                | sort -u || true
        )
        for rpc in "${agent_rpcs[@]}"; do
            [ -z "$rpc" ] && continue
            # Skip prose words and message names that match the prefix pattern but aren't RPCs
            case "$rpc" in
                *Request|*Response) continue ;;
                Cancelled|Started|Completed|Running|Suspended|Starts|Creates|Deletes|Returns|Registers|Updates|Reports|Streams|Releases|Prepares|Stops|Resets|Attaches|Drains|Lists|Submits|Gets|Runs|Cancels|Executes|Completes|Resumes|Suspends|Revokes|Applies|Reconfigures|Deregisters|Signals|InteractiveInput) continue ;;
            esac
            if ! grep -qx "$rpc" <<< "$proto_rpcs"; then
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

check_agents_md_presence
check_knowledge_docs_presence
check_module_files
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
