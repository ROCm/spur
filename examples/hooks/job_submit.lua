-- Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

-- Example Lua job_submit hook (Slurm job_submit/lua parity). The controller
-- runs slurm_job_submit(job_desc, submit_uid) in a sandbox at submission.
-- Return slurm.SUCCESS to accept, or a non-zero code to reject; the message
-- from slurm.log_user is shown to the submitting user. Mutate job_desc in place
-- to modify the job -- only whitelisted fields take effect: qos, partition,
-- account, constraint, comment, reservation, priority, time_limit (minutes),
-- begin_time (RFC3339 string), gres (array), hold. Other fields (identity,
-- script, resource counts) are ignored. The sandbox has no os/io/require, so a
-- policy script cannot shell out, read files, or load modules.
-- Setting a field to nil is a no-op (it cannot clear a value); assign a new
-- value to change it. The hook runs at submission only -- `scontrol update`
-- does not re-run it.

local MAX_MINUTES = 1440

function slurm_job_submit(job_desc, submit_uid)
    -- Prevent bad submissions: a partition is mandatory here.
    if job_desc.partition == nil or job_desc.partition == "" then
        slurm.log_user("a partition is required (submit with -p/--partition)")
        return slurm.ERROR
    end

    -- Add QoS automatically for a given account.
    if job_desc.account == "research" then
        job_desc.qos = "high"
    end

    -- Enforce a walltime cap (time_limit is in minutes, Slurm convention).
    if job_desc.time_limit == nil or job_desc.time_limit > MAX_MINUTES then
        job_desc.time_limit = MAX_MINUTES
    end

    return slurm.SUCCESS
end
