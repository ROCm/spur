#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Example job_submit hook (Slurm job_submit.lua analog). The controller runs this
# at submission with the resolved job spec as JSON on stdin. Contract:
#   - exit 0, no stdout            -> accept unchanged
#   - exit non-zero                -> reject; stderr is shown to the submitting user
#   - exit 0, JSON object on stdout -> modify; only whitelisted fields are applied
# Whitelist: qos, partition, account, constraint, comment, reservation, priority,
# time_limit_minutes (integer minutes), begin_time (RFC3339), gres (array), hold.
# Non-whitelisted keys (identity, script, resource counts) are ignored and logged.
# Requires jq. Also available in the environment: SPUR_JOB_USER, SPUR_JOB_UID,
# SPUR_JOB_GID, SPUR_JOB_PARTITION.

set -euo pipefail

spec=$(cat)

partition=$(jq -r '.partition // ""' <<<"$spec")
account=$(jq -r '.account // ""' <<<"$spec")

# Prevent bad submissions: a partition is mandatory here.
if [[ -z "$partition" ]]; then
    echo "a partition is required (submit with -p/--partition)" >&2
    exit 1
fi

changes='{}'

# Add QoS automatically for a given account.
if [[ "$account" == "research" ]]; then
    changes=$(jq -c '. + {qos: "high"}' <<<"$changes")
fi

# Enforce a walltime cap when the request exceeds it or is unset. On stdin the
# limit is `time_limit` as [seconds, nanos]; the modify key is time_limit_minutes.
max_minutes=1440
req_secs=$(jq -r '.time_limit[0] // empty' <<<"$spec")
if [[ -z "$req_secs" || $((req_secs / 60)) -gt "$max_minutes" ]]; then
    changes=$(jq -c --argjson m "$max_minutes" '. + {time_limit_minutes: $m}' <<<"$changes")
fi

# Emit changes only when non-empty; otherwise stay silent to accept unchanged.
if [[ "$changes" != "{}" ]]; then
    echo "$changes"
fi
exit 0
