#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Checks that source files contain the SPDX license header.
# Used by both the pre-commit hook and CI workflow.
#
# Usage:
#   scripts/check-license-headers.sh          # check all tracked source files
#   scripts/check-license-headers.sh file...  # check specific files only

set -euo pipefail

SPDX_TAG="SPDX-License-Identifier: Apache-2.0"

check_file() {
    local file="$1"
    # Read enough lines to find the header (shebangs push it down a line or two)
    if ! head -5 "$file" | grep -qF "$SPDX_TAG"; then
        echo "  $file"
        return 1
    fi
    return 0
}

# If arguments were passed, check only those files; otherwise check all tracked files.
if [ $# -gt 0 ]; then
    files=("$@")
else
    mapfile -t files < <(git ls-files -- '*.rs' '*.proto' '*.py' '*.sh' 'Dockerfile' '*/Dockerfile' '*/Dockerfile.*')
fi

failed=0
missing=()

for f in "${files[@]}"; do
    [ -f "$f" ] || continue
    if ! check_file "$f"; then
        missing+=("$f")
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    echo ""
    echo "ERROR: ${#missing[@]} file(s) missing SPDX license header."
    echo "Each source file (.rs, .proto, .py, .sh, Dockerfile) must contain:"
    echo "  SPDX-License-Identifier: Apache-2.0"
    echo ""
    echo "For .rs and .proto files, add these lines at the top:"
    echo "  // Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved."
    echo "  // SPDX-License-Identifier: Apache-2.0"
    echo ""
    echo "For .py and .sh files (after the shebang, if any):"
    echo "  # Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved."
    echo "  # SPDX-License-Identifier: Apache-2.0"
    exit 1
fi

echo "All source files have SPDX license headers."
