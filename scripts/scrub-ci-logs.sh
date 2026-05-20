#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Redact sensitive data from CI-collected daemon logs before artifact upload.
#
# Usage:
#   scripts/scrub-ci-logs.sh <log-dir> [--hostnames n1,n2] [--ssh-user USER]

set -euo pipefail

LOG_DIR=""
HOSTNAMES=""
SSH_USER=""

usage() {
    echo "Usage: $0 <log-dir> [--hostnames n1,n2,...] [--ssh-user USER]" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --hostnames)
            HOSTNAMES="${2:-}"
            shift 2
            ;;
        --ssh-user)
            SSH_USER="${2:-}"
            shift 2
            ;;
        -h | --help)
            usage
            ;;
        -*)
            echo "Unknown option: $1" >&2
            usage
            ;;
        *)
            if [ -z "$LOG_DIR" ]; then
                LOG_DIR="$1"
            else
                echo "Unexpected argument: $1" >&2
                usage
            fi
            shift
            ;;
    esac
done

[ -n "$LOG_DIR" ] || usage
[ -d "$LOG_DIR" ] || { echo "Not a directory: $LOG_DIR" >&2; exit 1; }

mapfile -t LOG_FILES < <(find "$LOG_DIR" -type f -name '*.log' 2>/dev/null | sort)
if [ "${#LOG_FILES[@]}" -eq 0 ]; then
    echo "No .log files under $LOG_DIR"
    exit 0
fi

# Escape a string for use in sed regex.
sed_escape_regex() {
    printf '%s' "$1" | sed 's/[][\\^$.*+?{}()|]/\\&/g'
}

scrub_file() {
    local file="$1"
    local tmp
    tmp=$(mktemp)
    cp "$file" "$tmp"

    # SSH private keys (multiline block).
    sed -i -E '/-----BEGIN .* PRIVATE KEY-----/,/-----END .* PRIVATE KEY-----/c\
[REDACTED-KEY]' "$tmp"

    # URLs (http/https) before bare IPs so full URLs are replaced once.
    sed -i -E 's#https?://[^[:space:]"'\'']+#[REDACTED-URL]#g' "$tmp"

    # IPv6 bracket notation, e.g. [::]:6817 or [2001:db8::1]:6818
    sed -i -E 's/\[[0-9a-fA-F:]+\](:[0-9]+)?/[REDACTED-IP]/g' "$tmp"

    # IPv4:port
    sed -i -E 's/\b([0-9]{1,3}\.){3}[0-9]{1,3}:[0-9]+\b/[REDACTED-IP]:[REDACTED-PORT]/g' "$tmp"

    # IPv4 addresses
    sed -i -E 's/\b([0-9]{1,3}\.){3}[0-9]{1,3}\b/[REDACTED-IP]/g' "$tmp"

    # JWT-shaped strings.
    sed -i -E 's/eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/[REDACTED-JWT]/g' "$tmp"

    # Bearer tokens.
    sed -i -E 's/Bearer [A-Za-z0-9._-]+/Bearer [REDACTED]/g' "$tmp"

    # Key-value secrets (password, token, secret, api_key).
    sed -i -E 's/(password|passwd|token|secret|api_key)=[^[:space:]"'\'']+/\1=[REDACTED]/gi' "$tmp"

    # CI remote workdirs (correlate to GitHub run_id).
    sed -i -E 's#/tmp/spur-bm-[0-9]+/[^[:space:]"'\'']*#[REDACTED-PATH]#g' "$tmp"

    # User home prefix only — keep trailing path for debugging.
    sed -i -E 's#/home/[a-z][a-z0-9_-]*/#/home/[REDACTED-USER]/#g' "$tmp"

    # CI-specific install paths.
    sed -i -E 's#/opt/spur-ci[^[:space:]"'\'']*#[REDACTED-PATH]#g' "$tmp"

    # WireGuard peer public keys (topology fingerprint).
    sed -i -E 's/pubkey=[A-Za-z0-9+/=_-]{20,}/pubkey=[REDACTED]/g' "$tmp"

    # Known cluster hostnames from CI (longest first to avoid partial replacements).
    if [ -n "$HOSTNAMES" ]; then
        IFS=',' read -ra NAMES <<< "$HOSTNAMES"
        mapfile -t SORTED_NAMES < <(printf '%s\n' "${NAMES[@]}" | awk '{print length, $0}' | sort -rn | cut -d' ' -f2-)
        local i=0
        for name in "${SORTED_NAMES[@]}"; do
            name="${name#"${name%%[![:space:]]*}"}"
            name="${name%"${name##*[![:space:]]}"}"
            [ -n "$name" ] || continue
            i=$((i + 1))
            local escaped
            escaped=$(sed_escape_regex "$name")
            local repl
            repl="[REDACTED-HOST-${i}]"
            sed -i -E "s/${escaped}/${repl}/g" "$tmp"
        done
    fi

    # SSH username from CI secret.
    if [ -n "$SSH_USER" ]; then
        local user_escaped
        user_escaped=$(sed_escape_regex "$SSH_USER")
        sed -i -E "s/${user_escaped}@[^[:space:]]+/[REDACTED-USER]@[REDACTED-HOST]/g" "$tmp"
        sed -i -E "s/${user_escaped}/[REDACTED-USER]/g" "$tmp"
    fi

    # Container image refs with embedded credentials (docker://user:pass@...).
    sed -i -E 's#docker://[^[:space:]"'\'']+#[REDACTED-URL]#g' "$tmp"

    mv "$tmp" "$file"
}

for f in "${LOG_FILES[@]}"; do
    scrub_file "$f"
    echo "Scrubbed: $f"
done

echo "Scrubbed ${#LOG_FILES[@]} log file(s) under $LOG_DIR"
