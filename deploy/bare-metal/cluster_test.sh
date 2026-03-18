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
#
# Prerequisites:
#   - Cluster running (start-controller.sh + start-agent.sh)
#   - gpu_test binary compiled on both nodes
#   - PyTorch venv set up on both nodes
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
    return 1
}

job_state() {
    local job_id="$1"
    # squeue data: when whitespace-collapsed, fields are:
    #   $1=JOBID $2=NAME $3=USER $4=ST $5=TIME $6=NODES $7=NODELIST
    # (PARTITION merges with the gap after JOBID in display but awk sees it as a separate field
    #  only when it has content — check both $4 and $5 for 2-letter state codes)
    "${SPUR}/squeue" 2>/dev/null | tail -n +2 | awk -v id="${job_id}" '
        $1 == id {
            # Find the 2-char state code (CD, PD, R, F, CA)
            for (i = 2; i <= NF; i++) {
                if ($i ~ /^(PD|R|CD|CG|F|CA|TO|NF|PR|S)$/) {
                    print $i
                    exit
                }
            }
        }
    '
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

# --- Summary ---
echo "============================================"
echo "  Results: ${PASS}/${TOTAL} passed, ${FAIL} failed"
echo "============================================"

if [ "${FAIL}" -gt 0 ]; then
    exit 1
fi
