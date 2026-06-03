---
name: deploy-spur
description: Use when the user asks to deploy/install Spur on one or more bare-metal hosts over SSH. Handles single-node, multi-node, and HA (multi-controller Raft) deployments with direct SSH + bash. ALWAYS asks the user up-front for mode + topology + controller count before touching anything.
---

# Deploy Spur

Spur is an AI-native job scheduler with three core daemons:

- **spurctld** — controller / scheduler / Raft consensus (1 instance, or ≥ 2 for HA)
- **spurd** — node agent, runs on every compute host
- **spurrestd / spurdbd** — optional REST + accounting (out of scope here)

This skill stands the cluster up with only SSH and bash on the target hosts. The same logic covers single-node → multi-node → HA. Only the host list and Raft topology differ.

Defaults (override only if user asks):

| Var | Default |
|---|---|
| `SPUR_HOME` | `/root/spur` |
| `SPUR_INSTALL_DIR` | `/root/.local/bin` |
| `SPUR_VERSION` | `latest` (passed to `install.sh`; or `nightly` / `vX.Y.Z`) |
| `SPUR_CONTROLLER_PORT` | `6817` |
| `SPUR_AGENT_PORT` | `6818` |
| `SPUR_RAFT_PORT` | `6821` (hardcoded inside spurctld; cannot be changed via CLI) |
| `SPUR_CLUSTER_NAME` | `spur-cluster` |
| `SPUR_LOG_LEVEL` | `info` |
| `SPUR_WIPE_STATE` | `true` (always wipe Raft state on (re)deploy unless user says otherwise) |
| SSH user | `root` (unless user specifies otherwise) |

## Step 0: gather inputs (MANDATORY — do not skip)

Before any SSH, **ask the user** for:

1. **Deployment mode** — pick exactly one:
   | Mode | Use when |
   |---|---|
   | `single-node` | one host, controller and agent on the same machine |
   | `multi-node` | one controller, N compute agents (controller can also run an agent — hyperconverged) |
   | `ha` | ≥ 2 controllers (Raft consensus), any number of agents |

2. **Hosts** — for each role:
   - `CONTROLLERS` — SSH targets that will run `spurctld`. Required count:
     - single-node: 1
     - multi-node: 1
     - ha: **odd number, ≥ 3 recommended.** N=2 is allowed but has zero fault tolerance (use only for code-path testing). Quorum = ⌊N/2⌋+1; tolerates ⌊(N−1)/2⌋ failures.
   - `AGENTS` — SSH targets that will run `spurd`. Any number ≥ 1. Hosts can appear in **both** lists (hyperconverged).

3. **Transport** — `direct` (LAN, default) is the only mode this skill implements. If the user asks for WireGuard or any other overlay, tell them this skill only handles `direct` and stop.

Use `AskUserQuestion` for any of these the user didn't specify in their prompt. Don't guess.

> **For HA mode, explicitly warn the user** if their controller count is even or < 3:
> - `N=1` → not HA, suggest `multi-node` mode instead
> - `N=2` → call out "zero fault tolerance" before proceeding
> - even N ≥ 4 → suggest N−1 (strictly better)

Once gathered, name your shell arrays so the rest of the skill reads cleanly:
```bash
CONTROLLERS=( user@host1 user@host2 ... )   # ordered — index = Raft node_id - 1; ORDER MUST BE STABLE
AGENTS=(     user@host1 user@host2 ... )    # can overlap with CONTROLLERS for hyperconverged
SSH_USER=root                                # or whatever the user gave
TRANSPORT=direct
```
You can also keep two parallel arrays of just-the-hosts (without `user@`) for use in config files.

## Step 1: preflight all hosts

For every unique host (deduped controllers ∪ agents), run this check. Fail the whole deploy if any host fails.

```bash
HOSTS_ALL=( $(printf '%s\n' "${CONTROLLERS[@]}" "${AGENTS[@]}" | sort -u) )

for tgt in "${HOSTS_ALL[@]}"; do
  echo "############ $tgt ############"
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$tgt" '
    set +e
    echo "host=$(hostname -s) fqdn=$(hostname -f)"
    echo "kernel=$(uname -r) nproc=$(nproc)"
    echo "--- spur ports (6817/6818/6821) ---"
    ss -tlnpH 2>/dev/null | grep -E ":(6817|6818|6821)\b" || echo "spur ports free"
    echo "--- existing spur pids ---"
    pgrep -ax spurctld; pgrep -ax spurd; pgrep -ax spurrestd; echo "(end pids)"
    echo "--- tools ---"
    for t in curl tar bash ss pgrep pkill nohup; do command -v $t >/dev/null || echo "MISSING:$t"; done
    echo "--- ip ---"
    ip -4 -o addr show | awk "{print \$2, \$4}" | grep -v "127.0.0.1"
    echo "--- os ---"
    . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME"
  '
done
```

Fail-fast rules:
- A port in `6817/6818/6821` is in use by a process that is NOT `spurctld`/`spurd`/`spurrestd` → abort.
- `MISSING:curl` or `MISSING:tar` → abort (installer needs both).
- SSH itself fails → abort that host; the user needs to fix auth before continuing.

(Existing spur daemons are fine — we'll stop them in Step 4. Just note their PIDs in your status message.)

## Step 2: install Spur binary on all hosts (idempotent)

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" "
    set -euo pipefail
    mkdir -p ${SPUR_HOME} ${SPUR_HOME}/state ${SPUR_HOME}/log ${SPUR_HOME}/etc ${SPUR_INSTALL_DIR}
    # Skip install if binary already present and runnable.
    if ! ${SPUR_INSTALL_DIR}/spur --version >/dev/null 2>&1; then
      curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh \
        | INSTALL_DIR=${SPUR_INSTALL_DIR} bash -s -- ${SPUR_VERSION}
    fi
    ${SPUR_INSTALL_DIR}/spur --version
  "
done
```

Optional: prepend `${SPUR_INSTALL_DIR}` to `/etc/environment` so non-interactive SSH gets `spur` on PATH.

## Step 3: derive per-host facts (hostnames, IPs, node_ids)

You need three things per host before writing config:

```bash
# Short hostname (used as the spur "node name" — must match between spur.conf and `spurd --hostname`).
host_short() { ssh "$1" 'hostname -s'; }

# Advertised address. Use the SSH target hostname/IP by default — same address other hosts reach this one on.
# Strip user@ if present.
host_addr() { local t="${1#*@}"; echo "$t"; }
# (If user supplied a separate routable IP per host, use that instead.)

declare -A SHORT IP NODE_ID
for h in "${HOSTS_ALL[@]}"; do
  SHORT[$h]=$(host_short "$h")
  IP[$h]=$(host_addr "$h")
done

# 1-based Raft node_id, position in CONTROLLERS. ORDER MATTERS — reordering after a deploy
# breaks openraft membership. To re-order, wipe ~/spur/state/raft on every controller and redeploy.
for i in "${!CONTROLLERS[@]}"; do
  NODE_ID[${CONTROLLERS[$i]}]=$((i+1))
done
```

`hostname -s` (not `-f`) is intentional — controller's `[[nodes]]` list, `spurd --hostname`, and `spur show node <name>` must all use the same form, and the short form is what's idiomatic in Spur examples.

## Step 4: stop any existing Spur daemons on every host

**Critical:** use `pkill -x` (exact name), NEVER `pkill -f spurd` — the substring match also kills `spurctld`.

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" '
    pkill -x spurd 2>/dev/null || true
    pkill -x spurctld 2>/dev/null || true
    for i in $(seq 1 10); do
      pgrep -x spurctld >/dev/null || pgrep -x spurd >/dev/null || exit 0
      sleep 0.5
    done
    echo "daemons still running after 5s" >&2
    exit 1
  '
done
```

If `SPUR_WIPE_STATE=true` (default), also wipe Raft state on every **controller** before starting:

```bash
for tgt in "${CONTROLLERS[@]}"; do
  ssh "$tgt" "rm -rf ${SPUR_HOME}/state && mkdir -p ${SPUR_HOME}/state"
done
```

Skip the wipe only if the user explicitly asked to preserve state. Reordering controllers without wiping will break Raft.

## Step 5: render and push spur.conf to every controller

Build it locally, then SCP. All controllers get the **same** `spur.conf` except for `node_id` (HA only).

`peers` list **must be in the same order on every host** (it's by `node_id`, which is 1-based position in CONTROLLERS).

```bash
ha_enabled=false
[ ${#CONTROLLERS[@]} -gt 1 ] && ha_enabled=true

# Controllers list (used by [controller].hosts)
ctl_hosts_csv=$(printf '"%s", ' "${CONTROLLERS[@]/#*@/}" | sed 's/, $//')
# Strip user@ from each; for the peers list we need host:port too.
peers_csv=$(for h in "${CONTROLLERS[@]}"; do printf '"%s:%s", ' "${h#*@}" "${SPUR_RAFT_PORT}"; done | sed 's/, $//')

# Per-host [[nodes]] blocks
nodes_blocks=""
for h in "${AGENTS[@]}"; do
  cpus=$(ssh "$h" 'nproc')
  mem_kb=$(ssh "$h" "awk '/MemTotal/{print \$2}' /proc/meminfo")
  mem_mb=$(( mem_kb / 1024 * 9 / 10 ))   # 90% of RAM
  nodes_blocks+="
[[nodes]]
names = \"${SHORT[$h]}\"
cpus = ${cpus}
memory_mb = ${mem_mb}
"
done
partition_nodes_csv=$(IFS=,; nodes=(); for h in "${AGENTS[@]}"; do nodes+=("${SHORT[$h]}"); done; echo "${nodes[*]}")
```

Template the file (per controller, because `node_id` differs):

```bash
for ctl in "${CONTROLLERS[@]}"; do
  nid="${NODE_ID[$ctl]}"
  raft_block=""
  if $ha_enabled; then
    raft_block="node_id = ${nid}
peers = [${peers_csv}]"
  fi

  tmp=$(mktemp)
  cat > "$tmp" <<EOF
cluster_name = "${SPUR_CLUSTER_NAME}"

[controller]
listen_addr = "[::]:${SPUR_CONTROLLER_PORT}"
hosts = [${ctl_hosts_csv}]
state_dir = "${SPUR_HOME}/state"
raft_listen_addr = "[::]:${SPUR_RAFT_PORT}"
${raft_block}

[scheduler]
plugin = "backfill"
interval_secs = 1

[network]
wg_enabled = false
agent_port = ${SPUR_AGENT_PORT}
${nodes_blocks}
[[partitions]]
name = "default"
default = true
nodes = "${partition_nodes_csv}"
max_time = "INFINITE"
EOF

  scp -q "$tmp" "$ctl:${SPUR_HOME}/etc/spur.conf"
  rm -f "$tmp"
done
```

## Step 6: start spurctld on every controller

Daemonize correctly — three things matter:

- `-D` means **FOREGROUND** in Spur. **Never** pass `-D` when backgrounding.
- `nohup ... < /dev/null &` plus `disown` — both mandatory, or the SSH wrapper can kill the daemon on disconnect.
- Don't use brace expansion in `mkdir -p` over SSH under `set -e` — already avoided above.

```bash
for ctl in "${CONTROLLERS[@]}"; do
  ssh "$ctl" "
    nohup ${SPUR_INSTALL_DIR}/spurctld \
        -f ${SPUR_HOME}/etc/spur.conf \
        --state-dir ${SPUR_HOME}/state \
        --log-level ${SPUR_LOG_LEVEL} \
        > ${SPUR_HOME}/log/spurctld.log 2>&1 < /dev/null &
    disown
    echo started pid=\$!
  "
done

# Wait for every controller's gRPC port to bind.
for ctl in "${CONTROLLERS[@]}"; do
  ssh "$ctl" "
    for i in \$(seq 1 30); do
      ss -tlnH | grep -q ':${SPUR_CONTROLLER_PORT}\b' && exit 0
      sleep 1
    done
    echo 'spurctld did not bind ${SPUR_CONTROLLER_PORT}' >&2; exit 1
  "
done
```

**HA only:** spurctld binds 6817 immediately but returns `Status::unavailable("no leader elected yet")` until Raft quorum forms. Run this loop on the **first** controller:

```bash
if $ha_enabled; then
  ssh "${CONTROLLERS[0]}" "
    for i in \$(seq 1 60); do
      out=\$(${SPUR_INSTALL_DIR}/spur nodes 2>&1)
      echo \"\$out\" | grep -qE 'no leader|not the Raft leader|cannot reach leader|transport error|Connection refused' || { echo OK; exit 0; }
      sleep 1
    done
    echo 'timeout waiting for leader' >&2; exit 1
  "
fi
```

## Step 7: start spurd on every agent

`spurd --controller` in Spur 0.3.0 takes a single URL — agents are pointed at `CONTROLLERS[0]`. Non-leader controllers forward writes via Raft internally, so this works in HA too. Real failover when `CONTROLLERS[0]` dies needs a VIP / DNS in front of port 6817 (not done by this skill).

Also critical:
- Pass `--hostname` and `--address` **explicitly**. Auto-detect picks `127.0.0.1`, which breaks inter-node dispatch.
- `cd ${SPUR_HOME}` before launching — spurd's CWD is where job stdout (`spur-<N>.out`) lands. Make it predictable.

```bash
CTL0_ADDR="${CONTROLLERS[0]#*@}"
CTL_URL="http://${CTL0_ADDR}:${SPUR_CONTROLLER_PORT}"

for ag in "${AGENTS[@]}"; do
  ssh "$ag" "
    cd ${SPUR_HOME}
    nohup ${SPUR_INSTALL_DIR}/spurd \
        --controller ${CTL_URL} \
        --hostname ${SHORT[$ag]} \
        --address ${IP[$ag]} \
        --listen 0.0.0.0:${SPUR_AGENT_PORT} \
        --log-level ${SPUR_LOG_LEVEL} \
        > ${SPUR_HOME}/log/spurd.log 2>&1 < /dev/null &
    disown
    echo started pid=\$!
  "
done

# Wait for each agent's port to bind.
for ag in "${AGENTS[@]}"; do
  ssh "$ag" "
    for i in \$(seq 1 30); do
      ss -tlnH | grep -q ':${SPUR_AGENT_PORT}\b' && exit 0
      sleep 1
    done
    echo 'spurd did not bind ${SPUR_AGENT_PORT}' >&2; exit 1
  "
done
```

## Step 8: wait for every agent to register with the controller

`spur nodes` collapses by partition, so use `spur show node <name>` per-agent. Loop client-side:

```bash
for ag in "${AGENTS[@]}"; do
  for i in $(seq 1 30); do
    if ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur show node ${SHORT[$ag]} >/dev/null 2>&1"; then
      echo "${SHORT[$ag]} registered"; break
    fi
    [ $i -eq 30 ] && { echo "${SHORT[$ag]} never registered" >&2; exit 1; }
    sleep 1
  done
done
```

## Step 9: smoke test

### Single-node test (every deploy)

```bash
ssh "${CONTROLLERS[0]}" "cat > /tmp/spur-test-single.sh <<'EOF'
#!/bin/bash
#SBATCH --job-name=spur-test-single
echo \"ran on \$(hostname) at \$(date)\"
EOF
chmod +x /tmp/spur-test-single.sh
cd ${SPUR_HOME}
jid=\$(${SPUR_INSTALL_DIR}/spur submit /tmp/spur-test-single.sh | grep -oE '[0-9]+')
echo \"JOBID=\$jid\"
for i in \$(seq 1 30); do
  st=\$(${SPUR_INSTALL_DIR}/spur show job \$jid 2>/dev/null | grep -oE 'JobState=[A-Z]+' | head -1 | cut -d= -f2)
  case \"\$st\" in COMPLETED) echo OK; break ;; FAILED|CANCELLED|TIMEOUT|NODE_FAIL) echo \"BAD: \$st\" >&2; exit 1 ;; esac
  sleep 1
done
echo \"final state: \$st\"
"
```

Output lives at `${SPUR_HOME}/spur-<JOBID>.out` on whichever agent picked the job up. Loop agents and `cat` to find it (no shared-FS assumption).

### Multi-node test (when |AGENTS| ≥ 2)

```bash
N=${#AGENTS[@]}
# Use <<'EOF' so $SPUR_TASK_OFFSET etc. are NOT expanded by the submitting shell
# (they must reach the agent verbatim and be expanded at job-run time).
# Inject ${N} afterwards via sed.
ssh "${CONTROLLERS[0]}" "cat > /tmp/spur-test-multi.sh <<'EOF'
#!/bin/bash
#SBATCH --job-name=spur-test-multi
#SBATCH -N __N__
#SBATCH --ntasks-per-node=1
echo \"node \$SPUR_TASK_OFFSET of \$SPUR_NUM_NODES on \$(hostname); peers=\$SPUR_PEER_NODES\"
EOF
sed -i 's/__N__/${N}/' /tmp/spur-test-multi.sh
chmod +x /tmp/spur-test-multi.sh
cd ${SPUR_HOME}
jid=\$(${SPUR_INSTALL_DIR}/spur submit /tmp/spur-test-multi.sh | grep -oE '[0-9]+')
echo \"JOBID=\$jid\"
for i in \$(seq 1 60); do
  st=\$(${SPUR_INSTALL_DIR}/spur show job \$jid 2>/dev/null | grep -oE 'JobState=[A-Z]+' | head -1 | cut -d= -f2)
  case \"\$st\" in COMPLETED) echo OK; break ;; FAILED|CANCELLED|TIMEOUT|NODE_FAIL) echo \"BAD: \$st\" >&2; exit 1 ;; esac
  sleep 1
done
"
# Fetch outputs from EVERY agent — multi-node writes locally on each node.
for ag in "${AGENTS[@]}"; do
  echo "=== ${SHORT[$ag]} ==="
  ssh "$ag" "ls ${SPUR_HOME}/spur-*.out 2>/dev/null | xargs -r tail -n +1"
done
```

## Step 10: verify

```bash
ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur nodes"
ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur queue"
for ag in "${AGENTS[@]}"; do
  ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur show node ${SHORT[$ag]}" | head -5
done

# HA: confirm exactly one controller logged leadership
if $ha_enabled; then
  for ctl in "${CONTROLLERS[@]}"; do
    echo "--- $ctl ---"
    ssh "$ctl" "grep 'become leader' ${SPUR_HOME}/log/spurctld.log || echo '(follower)'"
  done
fi
```

## Step 11: teardown (only when asked)

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" "pkill -x spurd 2>/dev/null; pkill -x spurctld 2>/dev/null; true"
done
# Wipe state AND any stray job-stdout files. spurd writes spur-<N>.out into its
# CWD at startup, which can drift across redeploys (${SPUR_HOME}, ${SPUR_INSTALL_DIR},
# /root, /tmp). Sweep all of them so old job output doesn't masquerade as new.
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" "
    rm -rf ${SPUR_HOME}
    rm -f /root/spur-*.out ${SPUR_INSTALL_DIR}/spur-*.out /tmp/spur-*.out
  "
done
```

## Gotchas (all hard-won — don't relearn them)

### Daemon / process management
- **`-D` means FOREGROUND, not daemonize.** Use `nohup ... < /dev/null & disown`; never pass `-D` when backgrounding.
- **`pkill -f spurd` also kills `spurctld`** (substring match). Always `pkill -x` (exact name).
- **SSH backgrounding hangs without `< /dev/null` and `disown`.** Both mandatory.
- **`mkdir -p {a,b,c}` brace expansion can fail under `set -e` over SSH** — use explicit paths.

### Spur quirks
- **Output file `spur-<N>.out` lands in spurd's CWD at startup**, not the submitter's CWD (despite what `spur show job` claims in `WorkDir=`). Always `cd ${SPUR_HOME}` before launching spurd so its location is predictable.
- **`spur nodes` collapses by partition** — counts nodes in one row. To verify per-host registration, loop `spur show node <name>`.
- **`spur show node <name>` does prefix match**, not exact match. If two hostnames share a prefix (e.g. `gpu-a` and `gpu-ab`), querying the shorter one returns BOTH. For an unambiguous lookup pipe through `awk`: `spur show node <name> | awk -v n=<name> '/^NodeName=/{p=($0=="NodeName="n)} p'`.
- **`spur show job` uses `JobState=COMPLETED` (uppercase)**, not `State: Completed`. Parse `JobState=[A-Z]+`.
- **Raft port 6821 is hardcoded** in spurctld (not a CLI flag). Preflight must include it.
- **Harmless log spam: `invalid transition from Completed to Completed`** on followers after multi-node jobs. Leader already reported terminal state; followers' duplicate reports get rejected. Job actually succeeded.

### Multi-node / HA specifics
- **Agent `--hostname` must match the `[[nodes]]` name in `spur.conf`** and `spur show node <name>`. Use `hostname -s` consistently.
- **Pass `--hostname` and `--address` explicitly to spurd** in multi-node. Auto-detect picks `127.0.0.1` which makes inter-node dispatch fail.
- **No shared FS assumption.** In a multi-node job, each node writes its own `spur-<JOBID>.out` locally. Fetch from every agent, not just the controller.
- **HA needs a leader-elected wait**, not just port-listening. `spurctld` binds `:6817` immediately but returns `Status::unavailable("no leader elected yet")` until the Raft quorum forms. Loop on that error message in `spur nodes` until it clears.
- **openraft `info`-level logs spam a multi-line `RaftState{...}` dump on every start**, which clobbers `grep "become leader"` in noisy logs. If you're parsing the log for leader detection, set `RUST_LOG=openraft=warn,info` when launching spurctld, or grep with `-m1` and a stricter pattern.
- **HA `peers` list order must be stable across redeploys.** `node_id` is the 1-based position; reordering breaks openraft membership. To re-order, wipe `~/spur/state/raft/` on every controller and redeploy.
- **Client-side failover is NOT implemented in Spur 0.3.0.** `spurd --controller` takes a single URL. If that controller dies, agents are stranded even if Raft still has a leader. Production HA needs an L4 VIP / DNS in front of `:6817`.

## Report back

End the run with:
- Mode used (single-node / multi-node / ha) + counts (X controllers, Y agents)
- Per-host: install version, daemon PIDs, log paths (`${SPUR_HOME}/log/spurctld.log`, `spurd.log`)
- `spur nodes` output
- HA only: which `node_id` became leader (`grep "become leader" ${SPUR_HOME}/log/spurctld.log`)
- Test job IDs + stdout
- Any deviation from this skill — flag it so the skill can be patched
