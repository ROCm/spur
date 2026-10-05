#!/usr/bin/env bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Compare crates/spur-devices/src/cdi/amd_gpu_ids.txt against the AMD GPU
# operator NodeFeatureRule, which lists every AMD GPU PCI device ID it supports.
#
#   --check              fail when the rule lists device IDs the catalog lacks (default)
#   --write              add entries with a suggested GPU type for missing device IDs
#   --summary <path>     write a markdown summary (PR body) of the suggested entries
#   --source <url|path>  override the NodeFeatureRule location
#
# Suggested GPU types come from the upstream device comment and are marked TODO.
# A unit test fails until a person reviews them and removes the marker.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CATALOG="${ROOT}/crates/spur-devices/src/cdi/amd_gpu_ids.txt"
SOURCE="${GPU_OPERATOR_NFD_URL:-https://raw.githubusercontent.com/ROCm/gpu-operator/main/helm-charts-k8s/templates/gpu-nfd-default-rule.yaml}"
MODE="check"
SUMMARY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) MODE="check"; shift ;;
    --write) MODE="write"; shift ;;
    --summary) SUMMARY="$2"; shift 2 ;;
    --source) SOURCE="$2"; shift 2 ;;
    -h|--help) sed -n '6,15p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

workdir="$(mktemp -d)"
trap 'rm -rf "${workdir}"' EXIT

nfd="${workdir}/nfd.yaml"
if [[ "${SOURCE}" == http://* || "${SOURCE}" == https://* ]]; then
  echo "Fetching GPU operator NodeFeatureRule: ${SOURCE}"
  curl -fsSL "${SOURCE}" -o "${nfd}"
else
  cp "${SOURCE}" "${nfd}"
fi

# Emit "<id>\t<upstream name>" per device ID, first occurrence wins.
awk '
  /device: \{op: In, value: \[/ {
    if (match($0, /\["[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]"\]/) == 0) next
    id = tolower(substr($0, RSTART + 2, 4))
    if (id in seen) next
    seen[id] = 1
    comment = ""
    at = index($0, "} #")
    if (at > 0) comment = substr($0, at + 3)
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", comment)
    print id "\t" comment
  }
' "${nfd}" | sort > "${workdir}/upstream.tsv"

if [[ ! -s "${workdir}/upstream.tsv" ]]; then
  echo "no device IDs found in ${SOURCE}" >&2
  exit 1
fi

sed 's/#.*//' "${CATALOG}" | awk 'NF { print tolower($1) }' | sort > "${workdir}/have.txt"
cut -f1 "${workdir}/upstream.tsv" > "${workdir}/want.txt"
comm -23 "${workdir}/want.txt" "${workdir}/have.txt" > "${workdir}/missing.txt"

upstream_count="$(wc -l < "${workdir}/want.txt" | tr -d ' ')"
have_count="$(wc -l < "${workdir}/have.txt" | tr -d ' ')"
missing_count="$(wc -l < "${workdir}/missing.txt" | tr -d ' ')"
echo "GPU operator device IDs: ${upstream_count}, catalog entries: ${have_count}, missing: ${missing_count}"

# "Radeon Pro V710 MxGPU" -> v710, "MI300X HF VF" -> mi300x, "RX 9060 XT" -> rx9060-xt.
suggest_type() {
  local name="${1%%/*}"
  name="$(printf '%s' "${name,,}" | sed -E '
    s/\b(vf|hf|mxgpu)\b//g
    s/^[[:space:]]*(radeon pro|radeon|ai pro)[[:space:]]+//
    s/^[[:space:]]+|[[:space:]]+$//g
    s/^rx[[:space:]]+/rx/
    s/[[:space:]]+/-/g')"
  printf '%s' "${name:-unknown}"
}

if [[ "${missing_count}" -eq 0 ]]; then
  [[ -z "${SUMMARY}" ]] || echo "The catalog covers every GPU operator device ID." > "${SUMMARY}"
  echo "The catalog covers every GPU operator device ID."
  exit 0
fi

if [[ -n "${SUMMARY}" ]]; then
  {
    echo "The AMD GPU operator NodeFeatureRule lists ${missing_count} device ID(s) that Spur does not name."
    echo
    echo "Source: ${SOURCE}"
    echo
    echo "| Device ID | Upstream name | Suggested GPU type |"
    echo "|---|---|---|"
  } > "${SUMMARY}"
fi

: > "${workdir}/entries.txt"
while IFS=$'\t' read -r id comment; do
  grep -qx "${id}" "${workdir}/missing.txt" || continue
  gpu_type="$(suggest_type "${comment}")"
  echo "  ${id} (${comment:-unnamed}) -> ${gpu_type}"
  echo "${id} ${gpu_type}  # TODO verify, upstream: ${comment:-unnamed}" >> "${workdir}/entries.txt"
  [[ -z "${SUMMARY}" ]] || echo "| \`${id}\` | ${comment:-unnamed} | \`${gpu_type}\` |" >> "${SUMMARY}"
done < "${workdir}/upstream.tsv"

if [[ -n "${SUMMARY}" ]]; then
  {
    echo
    echo "The suggested GPU types come from the upstream comment. Before merge, check each one and remove its TODO marker, the spur-devices unit tests fail until then."
    echo
    echo "A virtual function or a board variant must use the GPU type of its physical card."
  } >> "${SUMMARY}"
fi

if [[ "${MODE}" != "write" ]]; then
  echo "" >&2
  echo "${missing_count} device ID(s) missing from ${CATALOG}" >&2
  echo "Run: scripts/sync-amd-gpu-ids.sh --write" >&2
  exit 1
fi

# Keep the header comment on top and the entries sorted by device ID.
{
  grep '^#' "${CATALOG}"
  { grep -v '^#' "${CATALOG}"; cat "${workdir}/entries.txt"; } | sort
} > "${workdir}/catalog.txt"
mv "${workdir}/catalog.txt" "${CATALOG}"

echo "Wrote ${missing_count} suggested entr(ies) to ${CATALOG}, review each TODO before merge."
