#!/usr/bin/env bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Mock variables must expand in the child process, not during script creation.
# shellcheck disable=SC2016
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d)"
trap 'rm -rf "${workdir}"' EXIT
mkdir -p "${workdir}/bin" "${workdir}/scripts"
yq -r '.jobs.sync.steps[-1].run' "${ROOT}/.github/workflows/amd-gpu-ids.yml" > "${workdir}/run.sh"

printf '%s\n' '#!/usr/bin/env bash' \
  'echo gh >> "${CALLS}"' \
  '[[ "$*" == "pr list "* ]] || exit 91' \
  'printf "%s\n" "${PR_NUMBER}"' \
  'exit "${GH_STATUS}"' > "${workdir}/bin/gh"
printf '%s\n' '#!/usr/bin/env bash' \
  'echo git >> "${CALLS}"' \
  'exit 90' > "${workdir}/bin/git"
printf '%s\n' '#!/usr/bin/env bash' \
  'echo sync >> "${CALLS}"' \
  'exit 73' > "${workdir}/scripts/sync-amd-gpu-ids.sh"
chmod +x "${workdir}/bin/gh" "${workdir}/bin/git" "${workdir}/scripts/sync-amd-gpu-ids.sh"

export PATH="${workdir}/bin:${PATH}"
export CALLS="${workdir}/calls" BRANCH="chore/amd-gpu-ids-sync"
export CATALOG="catalog.txt" SUMMARY="${workdir}/summary.md" RUNNER_TEMP="${workdir}"

check() {
  local name="$1" expected_status="$4" expected_calls="$5" status=0
  export PR_NUMBER="$2" GH_STATUS="$3"
  : > "${CALLS}"
  (cd "${workdir}" && bash run.sh) > "${workdir}/output" 2>&1 || status=$?
  if [[ "${status}" != "${expected_status}" || "$(< "${CALLS}")" != "${expected_calls}" ]]; then
    printf 'FAIL: %s (exit %s, calls: %s)\n' "${name}" "${status}" "$(< "${CALLS}")" >&2
    return 1
  fi
  printf 'PASS: %s\n' "${name}"
}

check 'Open sync PR prevents catalog generation and git changes' 123 0 0 gh
check 'No open PR permits catalog generation' '' 0 73 $'gh\nsync'
check 'Failed PR lookup prevents catalog generation and git changes' '' 42 42 gh
