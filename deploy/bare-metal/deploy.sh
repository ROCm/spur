#!/bin/bash
# Deploy Spur to MI300X cluster
#
# Topology:
#   mi300   (108.61.203.48) — spurctld + spurd + spurrestd (controller node)
#   mi300-2 (149.28.118.43) — spurd (compute node)
#
# Prerequisites:
#   1. Build on mi300: ssh mi300 'cd ~/spur-src && cargo build --release'
#   2. Run this script from shark-a
#
# Binaries are built on mi300 (glibc 2.35) for compatibility with both nodes.

set -euo pipefail

REMOTE_USER="anush"
REMOTE_SRC="/home/${REMOTE_USER}/spur-src"
REMOTE_BIN="${REMOTE_SRC}/target/release"
SPUR_HOME="/home/${REMOTE_USER}/spur"

CONTROLLER_HOST="mi300"
CONTROLLER_IP="108.61.203.48"

CONTROLLER_PORT=6817
AGENT_PORT=6818
REST_PORT=6820

echo "=== Spur MI300X Cluster Deploy ==="
echo "Controller: mi300 (${CONTROLLER_IP})"
echo "Compute:    mi300-2 (149.28.118.43)"
echo ""

# --- Verify binaries exist on mi300 ---
echo ">>> Checking binaries on mi300..."
ssh mi300 "ls -lh ${REMOTE_BIN}/{spurctld,spurd,spurrestd,spur} 2>/dev/null | awk '{print \$9, \$5}'" || {
    echo "ERROR: Binaries not found. Build first:"
    echo "  ssh mi300 'source ~/.cargo/env; cd ~/spur-src && cargo build --release'"
    exit 1
}

# --- Setup directories on both nodes ---
for host in mi300 mi300-2; do
    echo ""
    echo ">>> Setting up ${host}..."
    ssh ${host} "mkdir -p ${SPUR_HOME}/{bin,etc,state,log}"
done

# --- Copy config ---
echo ""
echo ">>> Deploying config..."
DEPLOY_DIR="$(cd "$(dirname "$0")" && pwd)"
scp "${DEPLOY_DIR}/spur.conf" mi300:${SPUR_HOME}/etc/spur.conf

# --- Install binaries on mi300 (controller + agent) ---
echo ""
echo ">>> Installing binaries on mi300..."
ssh mi300 "cp ${REMOTE_BIN}/{spurctld,spurd,spurrestd,spur} ${SPUR_HOME}/bin/"
ssh mi300 "cd ${SPUR_HOME}/bin && for cmd in sbatch srun squeue scancel sinfo sacct scontrol; do ln -sf spur \$cmd 2>/dev/null; done"

# --- Copy binaries to mi300-2 ---
echo ""
echo ">>> Installing binaries on mi300-2..."
ssh mi300 "scp ${REMOTE_BIN}/{spurd,spur} mi300-2:${SPUR_HOME}/bin/" 2>&1 || {
    # If mi300 can't SCP to mi300-2 directly, do two hops
    echo "  Direct SCP failed, using two-hop..."
    scp mi300:${REMOTE_BIN}/spurd /tmp/spurd-mi300
    scp mi300:${REMOTE_BIN}/spur /tmp/spur-mi300
    scp /tmp/spurd-mi300 mi300-2:${SPUR_HOME}/bin/spurd
    scp /tmp/spur-mi300 mi300-2:${SPUR_HOME}/bin/spur
    rm -f /tmp/spurd-mi300 /tmp/spur-mi300
}
ssh mi300-2 "cd ${SPUR_HOME}/bin && for cmd in sbatch srun squeue scancel sinfo sacct scontrol; do ln -sf spur \$cmd 2>/dev/null; done"

# --- Create start/stop scripts ---

# Controller start script (mi300)
ssh mi300 "cat > ${SPUR_HOME}/etc/start-controller.sh" << 'EOF'
#!/bin/bash
set -euo pipefail
SPUR_HOME="${HOME}/spur"
export PATH="${SPUR_HOME}/bin:${PATH}"

# Stop any existing instances
"${SPUR_HOME}/etc/stop-all.sh" 2>/dev/null || true
sleep 1

echo "Starting spurctld..."
nohup spurctld \
    -f "${SPUR_HOME}/etc/spur.conf" \
    --listen "[::]:6817" \
    --state-dir "${SPUR_HOME}/state" \
    --log-level info \
    -D \
    > "${SPUR_HOME}/log/spurctld.log" 2>&1 &
echo "  PID: $!"

sleep 2

echo "Starting spurd (local agent)..."
nohup spurd \
    --controller "http://127.0.0.1:6817" \
    --listen "0.0.0.0:6818" \
    --hostname "mi300" \
    --address "108.61.203.48" \
    --log-level info \
    -D \
    > "${SPUR_HOME}/log/spurd.log" 2>&1 &
echo "  PID: $!"

sleep 1

echo "Starting spurrestd..."
nohup spurrestd \
    --listen "0.0.0.0:6820" \
    --controller "http://127.0.0.1:6817" \
    > "${SPUR_HOME}/log/spurrestd.log" 2>&1 &
echo "  PID: $!"

sleep 1

echo ""
echo "Controller started. Checking status..."
"${SPUR_HOME}/bin/sinfo" 2>/dev/null || echo "(sinfo not ready yet — give it a second)"
echo ""
echo "Logs: ${SPUR_HOME}/log/"
EOF
ssh mi300 "chmod +x ${SPUR_HOME}/etc/start-controller.sh"

# Controller stop script
ssh mi300 "cat > ${SPUR_HOME}/etc/stop-all.sh" << 'EOF'
#!/bin/bash
echo "Stopping Spur services..."
pkill -f 'spurctld.*spur.conf' 2>/dev/null && echo "  spurctld stopped" || echo "  spurctld not running"
pkill -f spurrestd 2>/dev/null && echo "  spurrestd stopped" || echo "  spurrestd not running"
pkill -f 'spurd.*controller' 2>/dev/null && echo "  spurd stopped" || echo "  spurd not running"
EOF
ssh mi300 "chmod +x ${SPUR_HOME}/etc/stop-all.sh"

# Agent start script (mi300-2)
ssh mi300-2 "cat > ${SPUR_HOME}/etc/start-agent.sh" << EOF
#!/bin/bash
set -euo pipefail
SPUR_HOME="\${HOME}/spur"
export PATH="\${SPUR_HOME}/bin:\${PATH}"

# Stop existing
pkill -f 'spurd.*controller' 2>/dev/null && sleep 1 || true

echo "Starting spurd on mi300-2..."
nohup spurd \\
    --controller "http://${CONTROLLER_IP}:${CONTROLLER_PORT}" \\
    --listen "0.0.0.0:${AGENT_PORT}" \\
    --hostname "mi300-2" \\
    --address "149.28.118.43" \\
    --log-level info \\
    -D \\
    > "\${SPUR_HOME}/log/spurd.log" 2>&1 &
echo "  PID: \$!"
echo "Agent started. Log: \${SPUR_HOME}/log/spurd.log"
EOF
ssh mi300-2 "chmod +x ${SPUR_HOME}/etc/start-agent.sh"

# Agent stop script
ssh mi300-2 "cat > ${SPUR_HOME}/etc/stop-agent.sh" << 'EOF'
#!/bin/bash
echo "Stopping spurd..."
pkill -f 'spurd.*controller' 2>/dev/null && echo "  spurd stopped" || echo "  spurd not running"
EOF
ssh mi300-2 "chmod +x ${SPUR_HOME}/etc/stop-agent.sh"

echo ""
echo "=== Deploy complete ==="
echo ""
echo "To start the cluster:"
echo "  1. ssh mi300 '~/spur/etc/start-controller.sh'"
echo "  2. ssh mi300-2 '~/spur/etc/start-agent.sh'"
echo ""
echo "To check cluster status:"
echo "  ssh mi300 '~/spur/bin/sinfo'"
echo ""
echo "To submit a test job:"
echo "  ssh mi300 '~/spur/bin/sbatch --wrap \"hostname && rocm-smi --showid | head -12\"'"
echo ""
echo "To run on both nodes:"
echo "  ssh mi300 '~/spur/bin/srun --nodes=2 hostname'"
echo ""
echo "To stop everything:"
echo "  ssh mi300-2 '~/spur/etc/stop-agent.sh'"
echo "  ssh mi300 '~/spur/etc/stop-all.sh'"
