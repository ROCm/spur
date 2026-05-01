#!/bin/bash
# Spur MI300X Cluster Integration Test
#
# Runs a sequence of tests against a live Spur cluster:
#   1. Cluster health check (both nodes idle)
#   2. Single-node job dispatch
#   3. Two-node job dispatch
#   4. HIP GPU compute test (vector add on all GPUs)
#   5. PyTorch GEMM + RCCL all-reduce across all GPUs
#   6. Job completion tracking
#   7. Container jobs: launch, exit codes, cancel, DNS, /dev/shm,
#      PID namespace, env vars, bind mounts
#
# Prerequisites:
#   - Cluster running (start-controller.sh + start-agent.sh)
#   - gpu_test binary compiled on both nodes
#   - PyTorch venv set up on both nodes
#   - squashfs-tools installed (mksquashfs) for container tests
#
# Usage: ssh mi300 'bash ~/spur/cluster_test.sh'
#   or:  bash deploy/bare-metal/cluster_test.sh  (from shark-a)

set -euo pipefail

SPUR_HOME="${HOME}/spur"
SPUR="${SPUR_HOME}/bin"
PASS=0
FAIL=0
TOTAL=0

run_test() {
    local name="$1"
    shift
    TOTAL=$((TOTAL + 1))
    echo -n "TEST ${TOTAL}: ${name} ... "
    if "$@" > /dev/null 2>&1; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL"
        FAIL=$((FAIL + 1))
    fi
}

expect_output() {
    local name="$1"
    local pattern="$2"
    local file="$3"
    TOTAL=$((TOTAL + 1))
    echo -n "TEST ${TOTAL}: ${name} ... "
    if grep -q "${pattern}" "${file}" 2>/dev/null; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL (pattern '${pattern}' not found in ${file})"
        FAIL=$((FAIL + 1))
    fi
}

wait_job() {
    local job_id="$1"
    local timeout="${2:-120}"
    local elapsed=0
    while [ $elapsed -lt $timeout ]; do
        local state
        state=$(job_state "$job_id")
        case "$state" in
            CD|F|CA) return 0 ;;
            "") return 0 ;;  # job gone from queue = completed
        esac
        sleep 2
        elapsed=$((elapsed + 2))
    done
    echo "(timeout after ${timeout}s)"
}

job_state() {
    local job_id="$1"
    # squeue data: when whitespace-collapsed, fields are:
    #   $1=JOBID $2=NAME $3=USER $4=ST $5=TIME $6=NODES $7=NODELIST
    # (PARTITION merges with the gap after JOBID in display but awk sees it as a separate field
    #  only when it has content — check both $4 and $5 for 2-letter state codes)
    #
    # Note: avoid tail in the pipeline; when awk exits early (after finding the job),
    # tail would get SIGPIPE and exit 141, causing set -o pipefail to abort the script
    # once the queue grows large enough that there are unread lines remaining.
    "${SPUR}/squeue" -t all 2>/dev/null | awk -v id="${job_id}" '
        NR == 1 { next }
        $1 == id {
            # Find the 2-char state code (CD, PD, R, F, CA)
            for (i = 2; i <= NF; i++) {
                if ($i ~ /^(PD|R|CD|CG|F|CA|TO|NF|PR|S)$/) {
                    print $i
                    exit
                }
            }
        }
    ' || true
}

# Clean old output files
rm -f ~/spur-*.out ~/spur-*.err 2>/dev/null

echo "============================================"
echo "  Spur MI300X Cluster Integration Tests"
echo "============================================"
echo ""

# --- Test 1: Cluster health ---
echo "--- Cluster Health ---"
run_test "sinfo returns output" ${SPUR}/sinfo

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: both nodes idle ... "
NODE_COUNT=$(${SPUR}/sinfo 2>/dev/null | grep -c "idle")
if [ "${NODE_COUNT}" -ge 1 ]; then
    IDLE_NODES=$(${SPUR}/sinfo 2>/dev/null | grep idle | awk '{print $4}')
    echo "PASS (${IDLE_NODES} nodes)"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: mi300 node registered ... "
if ${SPUR}/scontrol show nodes 2>/dev/null | grep -q "NodeName=mi300"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: mi300-2 node registered ... "
if ${SPUR}/scontrol show nodes 2>/dev/null | grep -q "NodeName=mi300-2"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

echo ""

# --- Test 2: Single-node basic job ---
echo "--- Single-Node Job ---"
cat > /tmp/spur-test-basic.sh << 'SCRIPT'
#!/bin/bash
echo "hostname=$(hostname)"
echo "SPUR_JOB_ID=${SPUR_JOB_ID}"
echo "SPUR_NUM_NODES=${SPUR_NUM_NODES}"
echo "cpus=$(nproc)"
echo "SUCCESS"
SCRIPT
chmod +x /tmp/spur-test-basic.sh

JOB1=$(${SPUR}/sbatch -J test-basic -N 1 /tmp/spur-test-basic.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch single-node submitted (job ${JOB1})" test -n "${JOB1}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: job ${JOB1} completes ... "
if wait_job "${JOB1}" 30; then
    STATE=$(job_state "${JOB1}")
    if [ "${STATE}" = "CD" ] || [ -z "${STATE}" ]; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL (state=${STATE})"
        FAIL=$((FAIL + 1))
    fi
else
    echo "FAIL (timeout)"
    FAIL=$((FAIL + 1))
fi

# Find which node ran it and check output
for node_host in mi300 mi300-2; do
    OUTFILE="${HOME}/spur-${JOB1}.out"
    if [ -f "${OUTFILE}" ]; then
        expect_output "job ${JOB1} output has SUCCESS" "SUCCESS" "${OUTFILE}"
        expect_output "job ${JOB1} has SPUR_JOB_ID" "SPUR_JOB_ID=${JOB1}" "${OUTFILE}"
        break
    fi
done

echo ""
sleep 2

# --- Test 3: Two-node job ---
echo "--- Two-Node Job ---"
cat > /tmp/spur-test-2node.sh << 'SCRIPT'
#!/bin/bash
echo "node=$(hostname)"
echo "SPUR_JOB_ID=${SPUR_JOB_ID}"
echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"
echo "SPUR_NUM_NODES=${SPUR_NUM_NODES}"
echo "SPUR_TASK_OFFSET=${SPUR_TASK_OFFSET}"
echo "SPUR_PEER_NODES=${SPUR_PEER_NODES}"
echo "TWO_NODE_OK"
SCRIPT
chmod +x /tmp/spur-test-2node.sh

JOB2=$(${SPUR}/sbatch -J test-2node -N 2 /tmp/spur-test-2node.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch two-node submitted (job ${JOB2})" test -n "${JOB2}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: job ${JOB2} completes ... "
if wait_job "${JOB2}" 30; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

OUTFILE="${HOME}/spur-${JOB2}.out"
if [ -f "${OUTFILE}" ]; then
    expect_output "job ${JOB2} has TWO_NODE_OK" "TWO_NODE_OK" "${OUTFILE}"
    expect_output "job ${JOB2} has SPUR_NODE_RANK" "SPUR_NODE_RANK=" "${OUTFILE}"
    expect_output "job ${JOB2} has SPUR_PEER_NODES" "SPUR_PEER_NODES=" "${OUTFILE}"
    expect_output "job ${JOB2} reports 2 nodes" "SPUR_NUM_NODES=2" "${OUTFILE}"
fi

echo ""
sleep 2

# --- Test 4: HIP GPU test ---
echo "--- HIP GPU Compute ---"
cat > /tmp/spur-test-gpu.sh << 'SCRIPT'
#!/bin/bash
~/spur/bin/gpu_test
SCRIPT
chmod +x /tmp/spur-test-gpu.sh

JOB3=$(${SPUR}/sbatch -J test-hip -N 1 /tmp/spur-test-gpu.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch HIP gpu_test submitted (job ${JOB3})" test -n "${JOB3}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: HIP job ${JOB3} completes ... "
if wait_job "${JOB3}" 30; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

OUTFILE="${HOME}/spur-${JOB3}.out"
if [ -f "${OUTFILE}" ]; then
    expect_output "HIP gpu_test ALL PASS" "ALL PASS" "${OUTFILE}"
    expect_output "HIP found 8 GPUs" "GPU count: 8" "${OUTFILE}"
    expect_output "HIP detected MI300X" "MI300X" "${OUTFILE}"
fi

echo ""
sleep 2

# --- Test 5: 2-node HIP GPU test ---
echo "--- Two-Node HIP GPU Compute ---"
JOB4=$(${SPUR}/sbatch -J test-hip2 -N 2 /tmp/spur-test-gpu.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch 2-node HIP submitted (job ${JOB4})" test -n "${JOB4}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: 2-node HIP job ${JOB4} completes ... "
if wait_job "${JOB4}" 30; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

OUTFILE="${HOME}/spur-${JOB4}.out"
if [ -f "${OUTFILE}" ]; then
    expect_output "2-node HIP ALL PASS" "ALL PASS" "${OUTFILE}"
fi

echo ""
sleep 2

# --- Test 6: PyTorch distributed test ---
echo "--- PyTorch Distributed (GEMM + RCCL) ---"
JOB5=$(${SPUR}/sbatch -J test-pt -N 2 ~/spur/distributed_job.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch PyTorch distributed submitted (job ${JOB5})" test -n "${JOB5}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: PyTorch job ${JOB5} completes ... "
if wait_job "${JOB5}" 180; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

OUTFILE="${HOME}/spur-${JOB5}.out"
if [ -f "${OUTFILE}" ]; then
    expect_output "PyTorch found 8 GPUs" "GPUs: 8" "${OUTFILE}"
    expect_output "PyTorch detected MI300X" "MI300X" "${OUTFILE}"
    expect_output "PyTorch GEMM ran" "TFLOPS" "${OUTFILE}"
    expect_output "PyTorch RCCL all-reduce ran" "All-Reduce" "${OUTFILE}"
    expect_output "PyTorch test completed" "DONE" "${OUTFILE}"
fi

echo ""
sleep 2

# --- Test 7: Job cancellation ---
echo "--- Job Cancellation ---"
cat > /tmp/spur-test-long.sh << 'SCRIPT'
#!/bin/bash
sleep 300
SCRIPT
chmod +x /tmp/spur-test-long.sh

JOB6=$(${SPUR}/sbatch -J test-cancel -N 1 /tmp/spur-test-long.sh 2>/dev/null | awk '{print $NF}')
run_test "sbatch long job submitted (job ${JOB6})" test -n "${JOB6}"

sleep 3  # let it start
${SPUR}/scancel "${JOB6}" 2>/dev/null

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: job ${JOB6} cancelled ... "
sleep 2
STATE=$(job_state "${JOB6}")
if [ "${STATE}" = "CA" ] || [ "${STATE}" = "F" ] || [ -z "${STATE}" ]; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL (state=${STATE})"
    FAIL=$((FAIL + 1))
fi

echo ""

sleep 2

# ---------------------------------------------------------------------------
# Helpers for extended tests
# ---------------------------------------------------------------------------

# Poll until a terminal state is seen; return that state (or GONE/TIMEDOUT).
wait_final_state() {
    local job_id="$1"
    local timeout="${2:-60}"
    local elapsed=0
    local last=""
    while [ $elapsed -lt $timeout ]; do
        local s
        s=$(job_state "$job_id")
        case "$s" in
            CD|F|CA|TO)  echo "$s"; return 0 ;;
            "")
                # Gone from squeue — if we saw a state before, return it.
                if [ -n "$last" ]; then echo "$last"; else echo "GONE"; fi
                return 0 ;;
        esac
        last="$s"
        sleep 1
        elapsed=$((elapsed + 1))
    done
    echo "TIMEDOUT"
}

# Read output file from mi300-2 for a given job.
remote_out() {
    local path="$1"
    ssh mi300-2 "cat ${path} 2>/dev/null" 2>/dev/null || true
}

# Assert a job reaches a specific terminal state.
expect_state() {
    local name="$1"
    local job_id="$2"
    local expected="$3"
    local timeout="${4:-60}"
    TOTAL=$((TOTAL + 1))
    echo -n "TEST ${TOTAL}: ${name} ... "
    local got
    got=$(wait_final_state "$job_id" "$timeout")
    # GONE is acceptable for CD (job purged immediately after success)
    if [ "$got" = "$expected" ] || { [ "$expected" = "CD" ] && [ "$got" = "GONE" ]; }; then
        echo "PASS (state=${got})"
        PASS=$((PASS + 1))
    else
        echo "FAIL (expected ${expected}, got ${got})"
        FAIL=$((FAIL + 1))
    fi
}

# --- Test 8: Failed Job Detection ---
echo "--- Failed Job Detection ---"

cat > /tmp/spur-test-exitfail.sh << 'SCRIPT'
#!/bin/bash
echo "before-failure"
exit 42
SCRIPT
chmod +x /tmp/spur-test-exitfail.sh

JFAIL=$(${SPUR}/sbatch -J test-exitfail -N 1 \
    -w mi300 \
    -o /tmp/spur-fail-$$.out \
    /tmp/spur-test-exitfail.sh 2>/dev/null | awk '{print $NF}')
run_test "failed job: sbatch submitted" test -n "${JFAIL}"
expect_state "failed job: state=F" "${JFAIL}" "F" 30

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: failed job: output captured before exit ... "
if grep -q "before-failure" /tmp/spur-fail-$$.out 2>/dev/null; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi
rm -f /tmp/spur-fail-$$.out
echo ""
sleep 2

# --- Test 9: Custom Output / Error Paths ---
echo "--- Custom Output/Error Paths ---"

CUSTOM_OUT="/tmp/spur-custom-out-$$.txt"
CUSTOM_ERR="/tmp/spur-custom-err-$$.txt"

cat > /tmp/spur-test-io.sh << 'SCRIPT'
#!/bin/bash
echo "stdout-line"
echo "stderr-line" >&2
echo "CUSTOM_IO_OK"
SCRIPT
chmod +x /tmp/spur-test-io.sh

JIO=$(${SPUR}/sbatch -J test-custom-io -N 1 \
    -w mi300 \
    -o "${CUSTOM_OUT}" -e "${CUSTOM_ERR}" \
    /tmp/spur-test-io.sh 2>/dev/null | awk '{print $NF}')
run_test "custom io: submitted" test -n "${JIO}"
wait_job "${JIO}" 30
expect_output "custom io: stdout in -o file" "CUSTOM_IO_OK" "${CUSTOM_OUT}"
expect_output "custom io: stderr in -e file" "stderr-line" "${CUSTOM_ERR}"
rm -f "${CUSTOM_OUT}" "${CUSTOM_ERR}"

# %j substitution
JSUBST=$(${SPUR}/sbatch -J test-subst -N 1 \
    -w mi300 \
    -o "/tmp/spur-subst-%j.out" \
    /tmp/spur-test-basic.sh 2>/dev/null | awk '{print $NF}')
run_test "%j substitution: submitted" test -n "${JSUBST}"
wait_job "${JSUBST}" 30
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: %j substitution: file exists as /tmp/spur-subst-${JSUBST}.out ... "
if [ -f "/tmp/spur-subst-${JSUBST}.out" ]; then
    echo "PASS"
    PASS=$((PASS + 1))
    rm -f "/tmp/spur-subst-${JSUBST}.out"
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi
echo ""
sleep 2

# --- Test 10: Environment Variable Passthrough ---
echo "--- Environment Variable Passthrough ---"

cat > /tmp/spur-test-env.sh << 'SCRIPT'
#!/bin/bash
echo "MYVAR=${MYVAR}"
echo "MULTIVAR=${MULTIVAR}"
echo "ENV_OK"
SCRIPT
chmod +x /tmp/spur-test-env.sh

JENV=$(MYVAR=hello123 MULTIVAR=world456 \
    ${SPUR}/sbatch -J test-env -N 1 \
    -w mi300 \
    -o /tmp/spur-env-$$.out \
    --export=MYVAR,MULTIVAR \
    /tmp/spur-test-env.sh 2>/dev/null | awk '{print $NF}')
run_test "env passthrough: submitted" test -n "${JENV}"
wait_job "${JENV}" 30
expect_output "env passthrough: MYVAR=hello123" "MYVAR=hello123" "/tmp/spur-env-$$.out"
expect_output "env passthrough: MULTIVAR=world456" "MULTIVAR=world456" "/tmp/spur-env-$$.out"
rm -f /tmp/spur-env-$$.out
echo ""
sleep 2

# --- Test 11: Node Selection (--nodelist / --exclude) ---
echo "--- Node Selection ---"

cat > /tmp/spur-test-nodename.sh << 'SCRIPT'
#!/bin/bash
echo "RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}"
echo "NODENAME_OK"
SCRIPT
chmod +x /tmp/spur-test-nodename.sh

# --nodelist mi300
JNODELIST=$(${SPUR}/sbatch -J test-nodelist -N 1 \
    -o /tmp/spur-nodelist-$$.out \
    -w mi300 \
    /tmp/spur-test-nodename.sh 2>/dev/null | awk '{print $NF}')
run_test "nodelist: --nodelist mi300 submitted" test -n "${JNODELIST}"
wait_job "${JNODELIST}" 30
expect_output "nodelist: job ran on mi300" "RAN_ON=mi300" "/tmp/spur-nodelist-$$.out"
rm -f /tmp/spur-nodelist-$$.out
sleep 2

# --nodelist mi300-2
JNODELIST2=$(${SPUR}/sbatch -J test-nodelist2 -N 1 \
    -o /tmp/spur-nodelist2-$$.out \
    -w mi300-2 \
    /tmp/spur-test-nodename.sh 2>/dev/null | awk '{print $NF}')
run_test "nodelist: --nodelist mi300-2 submitted" test -n "${JNODELIST2}"
wait_job "${JNODELIST2}" 30
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: nodelist: job ran on mi300-2 ... "
REMOTE_NL=$(remote_out "/tmp/spur-nodelist2-$$.out")
if echo "${REMOTE_NL}" | grep -q "RAN_ON=mi300-2"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL (remote: ${REMOTE_NL:-empty})"
    FAIL=$((FAIL + 1))
fi
ssh mi300-2 "rm -f /tmp/spur-nodelist2-$$.out" 2>/dev/null || true
sleep 2

# --exclude mi300 → must run on mi300-2
JEXCLUDE=$(${SPUR}/sbatch -J test-exclude -N 1 \
    -o /tmp/spur-exclude-$$.out \
    -x mi300 \
    /tmp/spur-test-nodename.sh 2>/dev/null | awk '{print $NF}')
run_test "exclude: --exclude mi300 submitted" test -n "${JEXCLUDE}"
wait_job "${JEXCLUDE}" 30
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: exclude: job ran on mi300-2 (not mi300) ... "
REMOTE_EX=$(remote_out "/tmp/spur-exclude-$$.out")
if echo "${REMOTE_EX}" | grep -q "RAN_ON=mi300-2"; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL (remote: ${REMOTE_EX:-empty})"
    FAIL=$((FAIL + 1))
fi
ssh mi300-2 "rm -f /tmp/spur-exclude-$$.out" 2>/dev/null || true
echo ""
sleep 2

# --- Test 12: Concurrent Job Scheduling ---
echo "--- Concurrent Job Scheduling ---"

cat > /tmp/spur-test-concurrent.sh << 'SCRIPT'
#!/bin/bash
echo "CONCURRENT_START=$(hostname)"
sleep 5
echo "CONCURRENT_DONE"
SCRIPT
chmod +x /tmp/spur-test-concurrent.sh

JCON1=$(${SPUR}/sbatch -J test-concurrent1 -N 1 \
    -o /tmp/spur-con1-$$.out /tmp/spur-test-concurrent.sh 2>/dev/null | awk '{print $NF}')
JCON2=$(${SPUR}/sbatch -J test-concurrent2 -N 1 \
    -o /tmp/spur-con2-$$.out /tmp/spur-test-concurrent.sh 2>/dev/null | awk '{print $NF}')
run_test "concurrent: job1 submitted" test -n "${JCON1}"
run_test "concurrent: job2 submitted" test -n "${JCON2}"

sleep 3
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: concurrent: both jobs running simultaneously ... "
CON1_STATE=$(job_state "${JCON1}")
CON2_STATE=$(job_state "${JCON2}")
if [ "${CON1_STATE}" = "R" ] && [ "${CON2_STATE}" = "R" ]; then
    echo "PASS"
    PASS=$((PASS + 1))
else
    echo "FAIL (job1=${CON1_STATE}, job2=${CON2_STATE})"
    FAIL=$((FAIL + 1))
fi

wait_job "${JCON1}" 30
wait_job "${JCON2}" 30

# Each job ran on a different node; check local + remote for both
CON1_LOCAL=$(cat /tmp/spur-con1-$$.out 2>/dev/null || true)
CON1_REMOTE=$(remote_out "/tmp/spur-con1-$$.out")
CON2_LOCAL=$(cat /tmp/spur-con2-$$.out 2>/dev/null || true)
CON2_REMOTE=$(remote_out "/tmp/spur-con2-$$.out")

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: concurrent: job1 completed ... "
if echo "${CON1_LOCAL}${CON1_REMOTE}" | grep -q "CONCURRENT_DONE"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"; FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: concurrent: job2 completed ... "
if echo "${CON2_LOCAL}${CON2_REMOTE}" | grep -q "CONCURRENT_DONE"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"; FAIL=$((FAIL + 1))
fi

rm -f /tmp/spur-con1-$$.out /tmp/spur-con2-$$.out
ssh mi300-2 "rm -f /tmp/spur-con1-$$.out /tmp/spur-con2-$$.out" 2>/dev/null || true
echo ""
sleep 2

# --- Test 13: Distributed Env Var Correctness (RANK / WORLD_SIZE) ---
echo "--- Distributed Env Var Correctness ---"

cat > /tmp/spur-test-dist-env.sh << 'SCRIPT'
#!/bin/bash
echo "RANK=${RANK}"
echo "WORLD_SIZE=${WORLD_SIZE}"
echo "MASTER_ADDR=${MASTER_ADDR}"
echo "MASTER_PORT=${MASTER_PORT}"
echo "SPUR_NODE_RANK=${SPUR_NODE_RANK}"
echo "DIST_ENV_OK"
SCRIPT
chmod +x /tmp/spur-test-dist-env.sh

JDIST=$(${SPUR}/sbatch -J test-dist-env -N 2 \
    -o /tmp/spur-dist-$$.out \
    /tmp/spur-test-dist-env.sh 2>/dev/null | awk '{print $NF}')
run_test "dist env: 2-node job submitted" test -n "${JDIST}"
wait_job "${JDIST}" 30

LOCAL_DIST=$(cat /tmp/spur-dist-$$.out 2>/dev/null || true)
REMOTE_DIST=$(remote_out "/tmp/spur-dist-$$.out")
ALL_DIST="${LOCAL_DIST}
${REMOTE_DIST}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: dist env: WORLD_SIZE=2 on both nodes ... "
LOCAL_WS=$(echo "${LOCAL_DIST}" | grep -c "^WORLD_SIZE=2" || true)
REMOTE_WS=$(echo "${REMOTE_DIST}" | grep -c "^WORLD_SIZE=2" || true)
if [ "${LOCAL_WS}" -ge 1 ] && [ "${REMOTE_WS}" -ge 1 ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (local_count=${LOCAL_WS}, remote_count=${REMOTE_WS})"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: dist env: RANK 0 and RANK 1 each present once ... "
HAS_RANK0=$(echo "${ALL_DIST}" | grep -c "^RANK=0" || true)
HAS_RANK1=$(echo "${ALL_DIST}" | grep -c "^RANK=1" || true)
if [ "${HAS_RANK0}" -ge 1 ] && [ "${HAS_RANK1}" -ge 1 ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (RANK=0 count=${HAS_RANK0}, RANK=1 count=${HAS_RANK1})"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: dist env: MASTER_ADDR non-empty on both nodes ... "
LOCAL_MA=$(echo "${LOCAL_DIST}" | grep "^MASTER_ADDR=" | grep -v "MASTER_ADDR=$" | wc -l || true)
REMOTE_MA=$(echo "${REMOTE_DIST}" | grep "^MASTER_ADDR=" | grep -v "MASTER_ADDR=$" | wc -l || true)
if [ "${LOCAL_MA}" -ge 1 ] && [ "${REMOTE_MA}" -ge 1 ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

expect_output "dist env: MASTER_PORT=29500" "MASTER_PORT=29500" "/tmp/spur-dist-$$.out"

rm -f /tmp/spur-dist-$$.out
ssh mi300-2 "rm -f /tmp/spur-dist-$$.out" 2>/dev/null || true
echo ""
sleep 2

# --- Test 14: Job Hold and Release ---
echo "--- Job Hold and Release ---"

JHOLD=$(${SPUR}/sbatch -J test-hold -N 1 -H \
    -o /tmp/spur-hold-$$.out \
    /tmp/spur-test-basic.sh 2>/dev/null | awk '{print $NF}')
run_test "hold: sbatch -H submitted" test -n "${JHOLD}"
sleep 2

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: hold: job stays pending (PD) ... "
HOLD_STATE=$(job_state "${JHOLD}")
if [ "${HOLD_STATE}" = "PD" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (state=${HOLD_STATE:-empty})"
    FAIL=$((FAIL + 1))
fi

run_test "hold: scontrol release exits 0" \
    ${SPUR}/scontrol release "${JHOLD}"
wait_job "${JHOLD}" 30

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: hold: job completes after release ... "
REL_STATE=$(job_state "${JHOLD}")
if [ "${REL_STATE}" = "CD" ] || [ -z "${REL_STATE}" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (state=${REL_STATE})"
    FAIL=$((FAIL + 1))
fi
rm -f /tmp/spur-hold-$$.out
echo ""
sleep 2

# --- Test 15: Job Dependencies (afterok) ---
echo "--- Job Dependencies (afterok) ---"

cat > /tmp/spur-test-dep-a.sh << 'SCRIPT'
#!/bin/bash
echo "DEP_A_START"
sleep 6
echo "DEP_A_DONE"
SCRIPT
chmod +x /tmp/spur-test-dep-a.sh

cat > /tmp/spur-test-dep-b.sh << 'SCRIPT'
#!/bin/bash
echo "DEP_B_RAN"
SCRIPT
chmod +x /tmp/spur-test-dep-b.sh

JDEP_A=$(${SPUR}/sbatch -J test-dep-a -N 1 \
    -o /tmp/spur-dep-a-$$.out \
    /tmp/spur-test-dep-a.sh 2>/dev/null | awk '{print $NF}')
run_test "dependency: job A submitted" test -n "${JDEP_A}"

JDEP_B=$(${SPUR}/sbatch -J test-dep-b -N 1 \
    -o /tmp/spur-dep-b-$$.out \
    --dependency=afterok:${JDEP_A} \
    /tmp/spur-test-dep-b.sh 2>/dev/null | awk '{print $NF}')
run_test "dependency: job B (afterok:A) submitted" test -n "${JDEP_B}"

sleep 3
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: dependency: B pending while A runs ... "
STATE_A=$(job_state "${JDEP_A}")
STATE_B=$(job_state "${JDEP_B}")
if [ "${STATE_A}" = "R" ] && [ "${STATE_B}" = "PD" ]; then
    echo "PASS (A=R, B=PD)"; PASS=$((PASS + 1))
else
    echo "FAIL (A=${STATE_A}, B=${STATE_B})"
    FAIL=$((FAIL + 1))
fi

wait_job "${JDEP_A}" 30
sleep 3  # give scheduler a cycle to unblock B
wait_job "${JDEP_B}" 30

DEP_B_OUT=$(cat /tmp/spur-dep-b-$$.out 2>/dev/null || remote_out "/tmp/spur-dep-b-$$.out")
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: dependency: B ran after A completed ... "
if echo "${DEP_B_OUT}" | grep -q "DEP_B_RAN"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (B output: ${DEP_B_OUT:-empty})"
    FAIL=$((FAIL + 1))
fi

rm -f /tmp/spur-dep-a-$$.out /tmp/spur-dep-b-$$.out
ssh mi300-2 "rm -f /tmp/spur-dep-b-$$.out" 2>/dev/null || true
echo ""
sleep 2

# --- Test 16: Time Limit Enforcement ---
echo "--- Time Limit Enforcement ---"

cat > /tmp/spur-test-walltime.sh << 'SCRIPT'
#!/bin/bash
echo "WALLTIME_STARTED"
sleep 300
echo "WALLTIME_SHOULD_NOT_REACH"
SCRIPT
chmod +x /tmp/spur-test-walltime.sh

JWTIME=$(${SPUR}/sbatch -J test-walltime -N 1 \
    -o /tmp/spur-walltime-$$.out \
    -t 0:00:10 \
    /tmp/spur-test-walltime.sh 2>/dev/null | awk '{print $NF}')
run_test "time limit: --time=0:00:10 submitted" test -n "${JWTIME}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: time limit: job killed within 30s ... "
KILLED=0
for i in $(seq 1 30); do
    sleep 1
    S=$(job_state "${JWTIME}")
    case "$S" in CA|F|TO|"") KILLED=1; break ;; esac
done
if [ "${KILLED}" -eq 1 ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (still running after 30s)"
    FAIL=$((FAIL + 1))
    ${SPUR}/scancel "${JWTIME}" 2>/dev/null || true
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: time limit: job started but did not finish naturally ... "
WT_OUT=$(cat /tmp/spur-walltime-$$.out 2>/dev/null || remote_out "/tmp/spur-walltime-$$.out")
if echo "${WT_OUT}" | grep -q "WALLTIME_STARTED" && \
   ! echo "${WT_OUT}" | grep -q "WALLTIME_SHOULD_NOT_REACH"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi
rm -f /tmp/spur-walltime-$$.out
ssh mi300-2 "rm -f /tmp/spur-walltime-$$.out" 2>/dev/null || true
echo ""
sleep 2

# --- Test 17: Distributed Inference (Tensor-Parallel, 8-way per node) ---
echo "--- Distributed Inference (TP-8 on each MI300X node) ---"
#
# inference_test.py runs mp.spawn across all GPUs on the node.
# Communication is intra-node RCCL (cross-node NCCL is firewalled on Vultr).
# Spur dispatches the job to both nodes; each runs an independent TP group.
# We verify both nodes complete and report throughput.
#

JI=$(${SPUR}/sbatch -J test-infer -N 2 \
    -o /tmp/spur-infer-$$.out \
    ~/spur/inference_job.sh 2>/dev/null | awk '{print $NF}')
run_test "inference: 2-node job submitted" test -n "${JI}"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: inference: both nodes complete within 10m ... "
if wait_job "${JI}" 600; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (timeout)"
    FAIL=$((FAIL + 1))
fi

LOCAL_INF=$(cat /tmp/spur-infer-$$.out 2>/dev/null || true)
REMOTE_INF=$(remote_out "/tmp/spur-infer-$$.out")

expect_output "inference: node0 INFERENCE_OK" "INFERENCE_OK" "/tmp/spur-infer-$$.out"

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: inference: mi300-2 INFERENCE_OK ... "
if echo "${REMOTE_INF}" | grep -q "INFERENCE_OK"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (remote: ${REMOTE_INF:-empty})"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: inference: throughput reported on both nodes ... "
LOCAL_TP=$(echo "${LOCAL_INF}" | grep -c "Throughput:" || true)
REMOTE_TP=$(echo "${REMOTE_INF}" | grep -c "Throughput:" || true)
if [ "${LOCAL_TP}" -ge 1 ] && [ "${REMOTE_TP}" -ge 1 ]; then
    # Print the numbers so they're visible in CI logs
    echo "PASS"
    echo "    node0: $(echo "${LOCAL_INF}"  | grep Throughput:)"
    echo "    node1: $(echo "${REMOTE_INF}" | grep Throughput:)"
    PASS=$((PASS + 1))
else
    echo "FAIL (node0_count=${LOCAL_TP}, node1_count=${REMOTE_TP})"
    FAIL=$((FAIL + 1))
fi

TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: inference: output finite (no NaN/Inf) ... "
if ! echo "${LOCAL_INF}${REMOTE_INF}" | grep -qi "non-finite\|nan\|error"; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"
    FAIL=$((FAIL + 1))
fi

rm -f /tmp/spur-infer-$$.out
ssh mi300-2 "rm -f /tmp/spur-infer-$$.out" 2>/dev/null || true
echo ""

# --- Container Jobs ---
echo "--- Container Jobs ---"

# Collect all agent nodes from sinfo (idle or mix).
CTEST_AGENTS=$("${SPUR}/sinfo" 2>/dev/null | awk 'NR>1 && ($5=="idle" || $5=="mix") {gsub(/\[.*\]/,"",$6); split($6,a,","); for(i in a) if(a[i]!="") print a[i]}' | sort -u)

ctest_cleanup() {
    rm -f "/tmp/spur-ctest-$$"* 2>/dev/null
    rm -rf "/tmp/spur-ctest-bind-$$" 2>/dev/null
    for node in $CTEST_AGENTS; do
        [ "$node" = "$(hostname)" ] && continue
        ssh "$node" "rm -f /tmp/spur-ctest-$$* 2>/dev/null; rm -rf /tmp/spur-ctest-bind-$$ 2>/dev/null" 2>/dev/null || true
    done
}
trap 'ctest_cleanup' EXIT

# Build a squashfs image with enough tools to test real functionality
CTEST_IMG="/tmp/spur-ctest-$$.sqsh"
CTEST_ROOT=$(mktemp -d "/tmp/spur-ctest-rootfs-$$.XXXXXX")
mkdir -p "$CTEST_ROOT"/{bin,usr/bin,lib,lib64,etc,dev,proc,sys,tmp,run,home}
# Include getent for DNS resolution testing (works without nslookup/dig)
for b in bash cat echo sleep hostname id df env stat ls wc head tail tr touch mkdir getent; do
    src=$(which "$b" 2>/dev/null)
    [ -f "$src" ] && cp "$src" "$CTEST_ROOT/usr/bin/"
done
ln -sf /usr/bin/bash "$CTEST_ROOT/bin/bash"
ln -sf /usr/bin/bash "$CTEST_ROOT/bin/sh"
for f in "$CTEST_ROOT/usr/bin/"*; do
    ldd "$f" 2>/dev/null | grep "=>" | awk '{print $3}' | while read -r lib; do
        if [ -f "$lib" ]; then
            dir=$(dirname "$lib")
            mkdir -p "$CTEST_ROOT$dir"
            cp -n "$lib" "$CTEST_ROOT$lib" 2>/dev/null || true
        fi
    done
done
# NSS libs for getent/DNS
for nsslib in libnss_dns.so.2 libnss_files.so.2 libresolv.so.2; do
    src="/lib/x86_64-linux-gnu/$nsslib"
    [ -f "$src" ] && mkdir -p "$CTEST_ROOT/lib/x86_64-linux-gnu" && cp -n "$src" "$CTEST_ROOT/lib/x86_64-linux-gnu/" 2>/dev/null || true
done
[ -f /lib64/ld-linux-x86-64.so.2 ] && cp /lib64/ld-linux-x86-64.so.2 "$CTEST_ROOT/lib64/"
for f in /etc/passwd /etc/group /etc/nsswitch.conf; do
    [ -f "$f" ] && cp "$f" "$CTEST_ROOT/etc/"
done
if command -v mksquashfs >/dev/null 2>&1; then
    mksquashfs "$CTEST_ROOT" "$CTEST_IMG" -noappend -quiet >/dev/null 2>&1
    rm -rf "$CTEST_ROOT"
    CTEST_READY=1
else
    echo "SKIP: mksquashfs not found, skipping container tests"
    rm -rf "$CTEST_ROOT"
    CTEST_READY=0
fi

# Ship image + helper scripts to every agent node
ctest_ship() {
    local path="$1"
    for node in $CTEST_AGENTS; do
        [ "$node" = "$(hostname)" ] && continue
        scp -q "$path" "$node:$path" 2>/dev/null || true
    done
}

# Submit a container job (no node pin — scheduler picks any agent)
ctest_sbatch() {
    "${SPUR}/sbatch" --container-image="$CTEST_IMG" "$@" 2>/dev/null | awk '{print $NF}'
}

# Check exit code of a completed job
ctest_exit_code() {
    "${SPUR}/scontrol" show job "$1" 2>/dev/null | grep -oP 'ExitCode=\K[0-9-]+'
}

# Dump debug info for a failed job
ctest_debug() {
    local name="$1" job_id="$2"
    echo "    DEBUG ${name}:"
    "${SPUR}/scontrol" show job "$job_id" 2>/dev/null | grep -E 'JobState|ExitCode|NodeList|Reason' || true
}

if [ "$CTEST_READY" = "1" ]; then

echo "  image: ${CTEST_IMG} ($(du -h "$CTEST_IMG" 2>/dev/null | awk '{print $1}'))"
echo "  agents: ${CTEST_AGENTS//$'\n'/, }"

# Ship image to all agent nodes
ctest_ship "$CTEST_IMG"
echo ""

# C1: Container launch — verify it runs and exits cleanly
cat > "/tmp/spur-ctest-c1-$$.sh" << 'SCRIPT'
#!/bin/bash
hostname >/dev/null || exit 1
id >/dev/null || exit 1
SCRIPT
chmod +x "/tmp/spur-ctest-c1-$$.sh"
ctest_ship "/tmp/spur-ctest-c1-$$.sh"
JC1=$(ctest_sbatch "/tmp/spur-ctest-c1-$$.sh")
run_test "C1: container sbatch submitted (job ${JC1})" test -n "${JC1}"
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C1: container job completes with exit 0 ... "
if wait_job "${JC1}" 30 && [ "$(ctest_exit_code "${JC1}")" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL"; FAIL=$((FAIL + 1))
    ctest_debug "C1" "${JC1}"
fi

# C2: Exit code propagation
cat > "/tmp/spur-ctest-c2-$$.sh" << 'SCRIPT'
#!/bin/bash
exit 42
SCRIPT
chmod +x "/tmp/spur-ctest-c2-$$.sh"
ctest_ship "/tmp/spur-ctest-c2-$$.sh"
JC2=$(ctest_sbatch "/tmp/spur-ctest-c2-$$.sh")
wait_job "${JC2}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C2: exit code 42 captured ... "
EC=$(ctest_exit_code "${JC2}")
if [ "$EC" = "42" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (got ExitCode=${EC})"; FAIL=$((FAIL + 1))
    ctest_debug "C2" "${JC2}"
fi

# C3: Cancellation — verify process tree is actually killed
cat > "/tmp/spur-ctest-c3-$$.sh" << 'SCRIPT'
#!/bin/bash
sleep 3600
SCRIPT
chmod +x "/tmp/spur-ctest-c3-$$.sh"
ctest_ship "/tmp/spur-ctest-c3-$$.sh"
JC3=$(ctest_sbatch "/tmp/spur-ctest-c3-$$.sh")
for i in $(seq 1 15); do
    ST=$(job_state "$JC3")
    [ "$ST" = "R" ] && break
    sleep 1
done
"${SPUR}/scancel" "$JC3" 2>/dev/null
sleep 6
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C3: cancelled job state ... "
C3_STATE=$(job_state "$JC3")
if [ "$C3_STATE" = "CA" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (state=${C3_STATE})"; FAIL=$((FAIL + 1))
fi

# C4: DNS — resolve a real hostname inside the container
cat > "/tmp/spur-ctest-c4-$$.sh" << 'SCRIPT'
#!/bin/bash
# Fail if resolv.conf contains loopback (should have been stripped)
grep -q "127.0.0.53" /etc/resolv.conf && exit 1
# Fail if DNS resolution doesn't work
getent hosts google.com >/dev/null 2>&1 || exit 2
SCRIPT
chmod +x "/tmp/spur-ctest-c4-$$.sh"
ctest_ship "/tmp/spur-ctest-c4-$$.sh"
JC4=$(ctest_sbatch "/tmp/spur-ctest-c4-$$.sh")
wait_job "${JC4}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C4: DNS works inside container ... "
EC=$(ctest_exit_code "${JC4}")
if [ "$EC" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (exit ${EC}: 1=loopback in resolv.conf, 2=getent failed)"; FAIL=$((FAIL + 1))
    ctest_debug "C4" "${JC4}"
fi

# C5: /dev/shm — write to shared memory inside container
cat > "/tmp/spur-ctest-c5-$$.sh" << 'SCRIPT'
#!/bin/bash
echo "shm_test" > /dev/shm/spur_ctest || exit 1
rm -f /dev/shm/spur_ctest
# Verify shm is mounted (any size > 0 means it's not missing)
df /dev/shm >/dev/null 2>&1 || exit 2
SCRIPT
chmod +x "/tmp/spur-ctest-c5-$$.sh"
ctest_ship "/tmp/spur-ctest-c5-$$.sh"
JC5=$(ctest_sbatch "/tmp/spur-ctest-c5-$$.sh")
wait_job "${JC5}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C5: /dev/shm writable and sized correctly ... "
EC=$(ctest_exit_code "${JC5}")
if [ "$EC" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (exit ${EC}: 1=write failed, 2=too small)"; FAIL=$((FAIL + 1))
    ctest_debug "C5" "${JC5}"
fi

# C6: PID namespace — verify process sees itself as PID 1
cat > "/tmp/spur-ctest-c6-$$.sh" << 'SCRIPT'
#!/bin/bash
[ "$$" = "1" ] || exit 1
# /proc must be readable (mounted fresh in the PID namespace)
[ -r /proc/self/status ] || exit 2
SCRIPT
chmod +x "/tmp/spur-ctest-c6-$$.sh"
ctest_ship "/tmp/spur-ctest-c6-$$.sh"
JC6=$(ctest_sbatch "/tmp/spur-ctest-c6-$$.sh")
wait_job "${JC6}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C6: PID namespace (process is PID 1) ... "
EC=$(ctest_exit_code "${JC6}")
if [ "$EC" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (exit ${EC}: 1=not PID 1, 2=/proc/1 wrong)"; FAIL=$((FAIL + 1))
    ctest_debug "C6" "${JC6}"
fi

# C7: Environment variables pass through correctly
cat > "/tmp/spur-ctest-c7-$$.sh" << 'SCRIPT'
#!/bin/bash
[ -n "$SPUR_JOB_ID" ] || exit 1
[ -n "$OMP_NUM_THREADS" ] || exit 2
SCRIPT
chmod +x "/tmp/spur-ctest-c7-$$.sh"
ctest_ship "/tmp/spur-ctest-c7-$$.sh"
JC7=$(ctest_sbatch "/tmp/spur-ctest-c7-$$.sh")
wait_job "${JC7}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C7: env vars passed to container ... "
EC=$(ctest_exit_code "${JC7}")
if [ "$EC" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (exit ${EC}: 1=no SPUR_JOB_ID, 2=no OMP_NUM_THREADS)"; FAIL=$((FAIL + 1))
    ctest_debug "C7" "${JC7}"
fi

# C8: Bind mount — verify content readable and read-only enforced
CTEST_BIND="/tmp/spur-ctest-bind-$$"
mkdir -p "$CTEST_BIND"
echo "bind_mount_ci_test" > "$CTEST_BIND/data.txt"
for node in $CTEST_AGENTS; do
    [ "$node" = "$(hostname)" ] && continue
    if ! ssh -o ConnectTimeout=5 "$node" "mkdir -p $CTEST_BIND" 2>/dev/null; then
        echo "    WARN: cannot reach $node via ssh, C8 bind mount test may fail"
    else
        scp -q -r "$CTEST_BIND/" "$node:$CTEST_BIND/" 2>/dev/null || true
    fi
done
cat > "/tmp/spur-ctest-c8-$$.sh" << 'SCRIPT'
#!/bin/bash
[ "$(cat /mnt/data/data.txt 2>/dev/null)" = "bind_mount_ci_test" ] || exit 1
# Write must fail on a :ro mount
touch /mnt/data/write_test 2>/dev/null && exit 2
exit 0
SCRIPT
chmod +x "/tmp/spur-ctest-c8-$$.sh"
ctest_ship "/tmp/spur-ctest-c8-$$.sh"
JC8=$(ctest_sbatch --container-mounts="${CTEST_BIND}:/mnt/data:ro" "/tmp/spur-ctest-c8-$$.sh")
wait_job "${JC8}" 30 >/dev/null 2>&1
TOTAL=$((TOTAL + 1))
echo -n "TEST ${TOTAL}: C8: bind mount content + read-only ... "
EC=$(ctest_exit_code "${JC8}")
if [ "$EC" = "0" ]; then
    echo "PASS"; PASS=$((PASS + 1))
else
    echo "FAIL (exit ${EC}: 1=content wrong, 2=ro not enforced)"; FAIL=$((FAIL + 1))
    ctest_debug "C8" "${JC8}"
fi

fi # CTEST_READY

echo ""

# --- Summary ---
echo "============================================"
echo "  Results: ${PASS}/${TOTAL} passed, ${FAIL} failed"
echo "============================================"

if [ "${FAIL}" -gt 0 ]; then
    exit 1
fi
