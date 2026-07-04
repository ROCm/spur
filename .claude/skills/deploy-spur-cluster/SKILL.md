---
name: deploy-spur-cluster
description: Use when the user asks to deploy/install a Spur cluster on one or more bare-metal hosts over SSH. Covers every topology the Ansible playbook does — single-node, multi-node, HA (multi-controller Raft), and HA with separate compute nodes — plus optional PostgreSQL/spurdbd accounting and WireGuard mesh. Installs daemons as systemd services and Slurm-compatible CLI symlinks. Drives everything with plain SSH + bash; no Ansible required. ALWAYS asks the user up-front for mode + topology + controller count + accounting before touching anything.
---

# Deploy Spur Cluster

Spur is an AI-native job scheduler with these daemons:

- **spurctld** — controller / scheduler / Raft consensus (1 instance, or ≥ 3 for HA)
- **spurd** — node agent, runs on every compute host
- **spurdbd** — optional accounting daemon (sacct/fairshare), backed by PostgreSQL. One instance for the whole cluster, on `ACCT_HOST` (default: first controller; may be a dedicated node).

This skill stands a cluster up with only SSH + bash on the targets. It is the standalone equivalent of `deploy/ansible/` and reaches the same end state: daemons run as **systemd services** (survive reboot), Slurm-compatible CLI names are symlinked, and accounting is optional (default on). One flow covers all four topologies — only the host list, Raft topology, and which hosts run an agent differ.

Defaults (override only if the user asks):

| Var | Default |
|---|---|
| `SPUR_HOME` | `/root/spur` |
| `SPUR_INSTALL_DIR` | `/root/.local/bin` |
| `SPUR_VERSION` | `latest` (passed to `install.sh`; or `nightly` / `vX.Y.Z`) |
| `SPUR_BINARY_SRC` | *(empty)* — local dir with pre-built `spur/spurctld/spurd/spurdbd`; used when set, else `install.sh` |
| `SPUR_CONTROLLER_PORT` | `6817` |
| `SPUR_AGENT_PORT` | `6818` |
| `SPUR_RAFT_PORT` | `6821` (hardcoded inside spurctld; cannot be changed via CLI) |
| `SPUR_ACCT_PORT` | `6819` |
| `SPUR_CLUSTER_NAME` | `spur-cluster` |
| `SPUR_LOG_LEVEL` | `info` |
| `SPUR_WIPE_STATE` | `false` (preserve Raft state so re-runs/upgrades are non-destructive; set `true` for a fresh install or intentional Raft reinit) |
| `ACCOUNTING` | `true` (deploy PostgreSQL + spurdbd; set `false` to skip) |
| `ACCT_DB_NAME` / `ACCT_DB_USER` / `ACCT_DB_PASSWORD` | `spur` / `spur` / `spur` |
| `TRANSPORT` | `direct` (LAN) — or `wireguard` for an encrypted mesh |
| SSH user | `root` (unless the user specifies otherwise) |

> These commands assume passwordless `sudo` (or SSH as root). If the SSH user is non-root, prefix privileged commands with `sudo` and confirm they have it. systemd unit installs, apt, and `/etc/systemd/system` writes all require root.
>
> **Non-root SSH + install dir under `/root`:** when `SPUR_INSTALL_DIR` is `/root/.local/bin` (the default) but you SSH as a non-root user, `/root` is mode 700 — the SSH user cannot even *execute* the binaries. In that case **every `spur`/`sbatch`/… CLI invocation must also be `sudo`-prefixed**, not just the file-writing steps. Alternatively set `SPUR_INSTALL_DIR` to a world-readable path (e.g. `/usr/local/bin`). Writing unit files and `spur.conf` needs `sudo tee` or scp-to-`/tmp`-then-`sudo install`, since heredoc redirects run as the SSH user.

## Step 0: gather inputs (MANDATORY — do not skip)

Before any SSH, **ask the user** (use `AskUserQuestion` for anything they didn't state; don't guess):

1. **Deployment mode** — pick exactly one:
   | Mode | Use when |
   |---|---|
   | `single-node` | one host runs controller **and** agent |
   | `multi-node` | 1 controller, N compute agents (controller may also run an agent — hyperconverged) |
   | `ha` | ≥ 3 controllers (Raft), N agents; controllers may be hyperconverged **or** dedicated (separate compute) |

2. **Hosts** — for each role:
   - `CONTROLLERS` — SSH targets running `spurctld`. Counts: single-node 1, multi-node 1, ha odd ≥ 3.
   - `AGENTS` — SSH targets running `spurd`. Any number ≥ 1. A host may appear in **both** lists (hyperconverged) or **only** in `AGENTS` (dedicated compute / "separate compute" HA).

3. **Accounting** — deploy PostgreSQL + spurdbd for `sacct`/fairshare? Default **yes**. If no, set `ACCOUNTING=false`; job submission still works, only `sacct` is unavailable.

4. **Transport** — `direct` (LAN, default) or `wireguard` (encrypted mesh). WireGuard adds Step 2b; everything else is identical (config advertises WG IPs instead of LAN IPs).

> **For HA, warn the user** if controller count is even or < 3:
> - `N=1` → not HA; suggest `multi-node`.
> - `N=2` → "zero fault tolerance" (quorum 2, tolerates 0 failures) — code-path testing only.
> - even `N ≥ 4` → suggest `N−1` (strictly better).
>
> **Topologies map to inventory shape** exactly like the playbook:
> - single-node → same host in CONTROLLERS and AGENTS
> - multi-node → 1 controller, N agents
> - HA hyperconverged → controllers also in AGENTS
> - HA + separate compute → controllers **not** in AGENTS; distinct agent hosts

Once gathered, define the arrays the rest of the skill uses:
```bash
CONTROLLERS=( user@host1 user@host2 user@host3 )   # ordered — index = Raft node_id - 1; ORDER MUST BE STABLE
AGENTS=(     user@host4 )                            # may overlap CONTROLLERS (hyperconverged) or be disjoint
SSH_USER=root
TRANSPORT=direct                                     # or wireguard
ACCOUNTING=true                                      # or false
ACCT_HOST="${CONTROLLERS[0]}"                         # accounting host: default first controller; may be ANY host — a controller, an agent, or a dedicated node (add it to HOSTS_ALL if dedicated)

SPUR_HOME=/root/spur
SPUR_INSTALL_DIR=/root/.local/bin
SPUR_VERSION=latest
SPUR_BINARY_SRC=                                     # e.g. /tmp/spur-bin to push pre-built binaries
SPUR_CONTROLLER_PORT=6817; SPUR_AGENT_PORT=6818; SPUR_RAFT_PORT=6821; SPUR_ACCT_PORT=6819
SPUR_CLUSTER_NAME=spur-cluster; SPUR_LOG_LEVEL=info; SPUR_WIPE_STATE=false
ACCT_DB_NAME=spur; ACCT_DB_USER=spur; ACCT_DB_PASSWORD=spur

HOSTS_ALL=( $(printf '%s\n' "${CONTROLLERS[@]}" "${AGENTS[@]}" | sort -u) )
ha_enabled=false; [ ${#CONTROLLERS[@]} -gt 1 ] && ha_enabled=true
```

## Step 1: preflight all hosts

Run on every unique host. Abort the whole deploy on any failure.

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  echo "############ $tgt ############"
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$tgt" '
    set +e
    echo "host=$(hostname -s) fqdn=$(hostname -f)"
    echo "kernel=$(uname -r) nproc=$(nproc)"
    echo "--- spur ports (6817/6818/6819/6821) ---"
    ss -tlnpH 2>/dev/null | grep -E ":(6817|6818|6819|6821)\b" || echo "spur ports free"
    echo "--- existing spur pids ---"
    pgrep -ax spurctld; pgrep -ax spurd; pgrep -ax spurdbd; echo "(end pids)"
    echo "--- tools ---"
    for t in curl tar bash ss pgrep pkill systemctl; do command -v $t >/dev/null || echo "MISSING:$t"; done
    echo "--- sudo ---"; sudo -n true 2>/dev/null && echo "sudo:ok" || echo "sudo:NEEDS-PASSWORD"
    echo "--- ip ---"
    ip -4 -o addr show | awk "{print \$2, \$4}" | grep -v "127.0.0.1"
    echo "--- os ---"
    . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME"
  '
done
```

Fail-fast rules:
- A spur port held by a process that is NOT `spurctld`/`spurd`/`spurdbd` → abort.
- `MISSING:systemctl` → abort (this skill installs systemd units; systemd is required).
- `MISSING:curl`/`tar` → abort unless `SPUR_BINARY_SRC` is set (installer needs them; the binary-copy path does not).
- SSH fails → abort that host; fix auth first.

(Existing spur daemons are fine — Step 4 stops them.)

## Step 2: install Spur binaries on all hosts (idempotent)

Two sources, same as the playbook. `SPUR_BINARY_SRC` (a local dir holding pre-built `spur`, `spurctld`, `spurd`, `spurdbd`) takes precedence — use it when the upstream repo has no published release (`install.sh` returns 403) or for air-gapped installs. Otherwise curl `install.sh`.

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" "
    set -euo pipefail
    mkdir -p ${SPUR_HOME} ${SPUR_HOME}/state ${SPUR_HOME}/log ${SPUR_HOME}/etc ${SPUR_INSTALL_DIR}
  "
  if [ -n "$SPUR_BINARY_SRC" ]; then
    # Push pre-built binaries from the operator box.
    for b in spur spurctld spurd spurdbd; do
      scp -q "${SPUR_BINARY_SRC}/${b}" "${tgt}:${SPUR_INSTALL_DIR}/${b}"
    done
    ssh "$tgt" "chmod 0755 ${SPUR_INSTALL_DIR}/spur ${SPUR_INSTALL_DIR}/spurctld ${SPUR_INSTALL_DIR}/spurd ${SPUR_INSTALL_DIR}/spurdbd"
  else
    ssh "$tgt" "
      set -euo pipefail
      if [ ! -x ${SPUR_INSTALL_DIR}/spur ]; then
        curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh \
          | INSTALL_DIR=${SPUR_INSTALL_DIR} bash -s -- ${SPUR_VERSION}
      fi
    "
  fi
  # Verify + create Slurm-compatible symlinks (the single `spur` binary dispatches on argv[0]).
  ssh "$tgt" "
    set -euo pipefail
    test -x ${SPUR_INSTALL_DIR}/spur || { echo 'spur binary missing after install' >&2; exit 1; }
    for n in sbatch squeue sinfo scancel sacct scontrol salloc srun; do
      ln -sf ${SPUR_INSTALL_DIR}/spur ${SPUR_INSTALL_DIR}/\$n
    done
    echo 'spur installed + symlinks created'
  "
done
```

> Do NOT rely on `spur --version` — it is not a supported flag and errors. Check for the file with `test -x` instead.

Optional: prepend `${SPUR_INSTALL_DIR}` to `/etc/environment` so non-interactive SSH gets `spur`/`sbatch`/etc. on PATH.

### Step 2b: WireGuard mesh (only when `TRANSPORT=wireguard`)

Skip entirely for `direct`. WireGuard uses the built-in `spur net` CLI and is **single-controller only** — `spur net init` auto-assigns the controller `.1` and there is no multi-controller mesh command, so HA must use `direct`. Steps, using the real CLI (all `spur net` commands log to stderr):

1. `apt install wireguard-tools` on every host.
2. On the controller: `spur net init --cidr 10.44.0.0/16 --port 51820 --interface spur0` (auto-assigns `.1`). Read its pubkey with `wg show spur0 public-key` (there is **no** `spur net pubkey`).
3. On each agent (assign `.2`, `.3`, …): `spur net join --endpoint <ctl-ip>:51820 --server-key <ctl-pubkey> --address 10.44.0.<N> --prefix-len 16 --interface spur0`. `--prefix-len` **must match the CIDR** (defaults to 16). Read the agent pubkey with `wg show spur0 public-key`.
4. On the controller, register each agent: `spur net add-peer --key <agent-pubkey> --allowed-ip 10.44.0.<N>/32 --interface spur0`.

Then set `WG_IP[$host]` per host and use those in place of `IP[...]` for `[controller].hosts`, `peers`, and spurd `--address`/`--controller`. There is no `spur net down` — tear down with `wg-quick down spur0` (or `ip link del spur0`) and remove `/etc/wireguard/spur0.conf`. If the user wants WG but you cannot verify mesh connectivity (all hosts on one `/24` makes it moot), tell them and offer `direct` instead.

## Step 3: derive per-host facts (hostnames, IPs, node_ids)

```bash
host_short() { ssh "$1" 'hostname -s'; }
host_addr()  { local t="${1#*@}"; echo "$t"; }   # SSH target IP/host, minus user@

declare -A SHORT IP NODE_ID
for h in "${HOSTS_ALL[@]}"; do
  SHORT[$h]=$(host_short "$h")
  IP[$h]=$(host_addr "$h")          # for TRANSPORT=wireguard, set IP[$h]=${WG_IP[$h]} instead
done

# 1-based Raft node_id = position in CONTROLLERS. ORDER MATTERS — reordering after a
# deploy breaks openraft membership. To re-order, wipe state on every controller and redeploy.
for i in "${!CONTROLLERS[@]}"; do NODE_ID[${CONTROLLERS[$i]}]=$((i+1)); done
```

`hostname -s` (not `-f`) is intentional — the controller's `[[nodes]]` names, `spurd --hostname`, and `spur show node <name>` must all use the same short form.

## Step 4: stop existing daemons + wipe state

Stop via systemd if a unit exists, and belt-and-suspenders `pkill -x` (exact name — `pkill -f spurd` also kills `spurctld`).

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" '
    for svc in spurd spurctld spurdbd; do
      systemctl stop "$svc" 2>/dev/null || true
    done
    pkill -x spurd 2>/dev/null || true
    pkill -x spurctld 2>/dev/null || true
    pkill -x spurdbd 2>/dev/null || true
    for i in $(seq 1 10); do
      pgrep -x spurctld >/dev/null || pgrep -x spurd >/dev/null || pgrep -x spurdbd >/dev/null || exit 0
      sleep 0.5
    done
    echo "daemons still running after 5s" >&2; exit 1
  '
done

# Wipe Raft state on controllers BEFORE start (so spurctld does not rewrite the log we delete).
if [ "$SPUR_WIPE_STATE" = true ]; then
  for tgt in "${CONTROLLERS[@]}"; do
    ssh "$tgt" "rm -rf ${SPUR_HOME}/state && mkdir -p ${SPUR_HOME}/state"
  done
fi
```

Default is **no wipe** so re-runs and upgrades preserve the job queue and node registrations. Wipe only for a fresh install or an intentional Raft reinit. Because Spur 0.3.0 has no online Raft membership change, **changing the controller set (add/remove/reorder) requires a wipe** — if you're keeping state but the controller list differs from the running cluster, warn the user and require `SPUR_WIPE_STATE=true`. Compute agents are not Raft members and can be added/removed freely without a wipe. When demoting a host from controller to agent-only, `systemctl disable --now spurctld` on it first, or the stale daemon keeps the old membership and can block quorum.

## Step 5: deploy accounting (only when `ACCOUNTING=true`) — on `ACCT_HOST`

Accounting is a single service for the whole cluster; it lives on `ACCT_HOST` (default `CONTROLLERS[0]`, but may be any host — a controller, an agent, or a dedicated node). Deploy it **before** the controllers so the `[accounting]` block spurctld reads has a live spurdbd. Idempotent: existence-checked role/DB creation. If `ACCT_HOST` is a dedicated node, make sure Step 2 installed the `spurdbd` binary there too (add it to `HOSTS_ALL`).

```bash
if [ "$ACCOUNTING" = true ]; then
  ssh "$ACCT_HOST" "
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    # Install PostgreSQL (Debian/Ubuntu). For RHEL, swap in dnf + postgresql-server + initdb.
    if ! command -v psql >/dev/null 2>&1; then
      apt-get update -qq
      apt-get install -y -qq postgresql postgresql-contrib
    fi
    systemctl enable --now postgresql
    # Create role + DB idempotently via the postgres superuser.
    sudo -u postgres psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname='${ACCT_DB_USER}'\" | grep -q 1 \
      || sudo -u postgres psql -c \"CREATE ROLE ${ACCT_DB_USER} LOGIN PASSWORD '${ACCT_DB_PASSWORD}'\"
    sudo -u postgres psql -tAc \"SELECT 1 FROM pg_database WHERE datname='${ACCT_DB_NAME}'\" | grep -q 1 \
      || sudo -u postgres psql -c \"CREATE DATABASE ${ACCT_DB_NAME} OWNER ${ACCT_DB_USER}\"
    test -x ${SPUR_INSTALL_DIR}/spurdbd || { echo 'spurdbd binary missing — install it or set ACCOUNTING=false' >&2; exit 1; }
  "

  # Install + start the spurdbd systemd unit. --migrate creates the accounting tables.
  ssh "$ACCT_HOST" "cat > /etc/systemd/system/spurdbd.service <<UNIT
[Unit]
Description=Spur Accounting Daemon (spurdbd)
After=network-online.target postgresql.service
Wants=network-online.target
Requires=postgresql.service

[Service]
Type=simple
ExecStart=${SPUR_INSTALL_DIR}/spurdbd --database-url postgresql://${ACCT_DB_USER}:${ACCT_DB_PASSWORD}@localhost/${ACCT_DB_NAME} --listen [::]:${SPUR_ACCT_PORT} --migrate --log-level ${SPUR_LOG_LEVEL}
Restart=on-failure
RestartSec=3
User=root
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable spurdbd
  systemctl restart spurdbd
  for i in \$(seq 1 30); do ss -tlnH | grep -q ':${SPUR_ACCT_PORT}\b' && { echo 'spurdbd up'; break; }; sleep 1; [ \$i -eq 30 ] && { echo 'spurdbd did not bind ${SPUR_ACCT_PORT}' >&2; exit 1; }; done
  "
fi
```

## Step 6: render spur.conf + install spurctld systemd unit on every controller

All controllers get the **same** `spur.conf` except `node_id` (HA only). The `peers` list order must be identical on every host (by `node_id`).

```bash
# CSVs for the controller list and the raft peers list.
ctl_hosts_csv=""; peers_csv=""
for h in "${CONTROLLERS[@]}"; do
  ctl_hosts_csv+="\"${IP[$h]}\", "
  peers_csv+="\"${IP[$h]}:${SPUR_RAFT_PORT}\", "
done
ctl_hosts_csv=${ctl_hosts_csv%, }; peers_csv=${peers_csv%, }

# Per-agent [[nodes]] blocks (90% of RAM) + partition node list.
nodes_blocks=""; part_csv=""
for h in "${AGENTS[@]}"; do
  cpus=$(ssh "$h" 'nproc')
  mem_kb=$(ssh "$h" "awk '/MemTotal/{print \$2}' /proc/meminfo")
  mem_mb=$(( mem_kb / 1024 * 9 / 10 ))
  nodes_blocks+=$'\n'"[[nodes]]"$'\n'"names = \"${SHORT[$h]}\""$'\n'"cpus = ${cpus}"$'\n'"memory_mb = ${mem_mb}"$'\n'
  part_csv+="${SHORT[$h]},"
done
part_csv=${part_csv%,}

# Accounting block (only when enabled) points at ACCT_HOST's advertised IP.
# ${IP[$ACCT_HOST]} requires ACCT_HOST to be in HOSTS_ALL (Step 3); falls back to
# stripping user@ from ACCT_HOST if it wasn't resolved into the IP map.
acct_block=""
if [ "$ACCOUNTING" = true ]; then
  acct_ip="${IP[$ACCT_HOST]:-${ACCT_HOST#*@}}"
  acct_block=$'\n'"[accounting]"$'\n'"host = \"${acct_ip}:${SPUR_ACCT_PORT}\""$'\n'"database_url = \"postgresql://${ACCT_DB_USER}:${ACCT_DB_PASSWORD}@localhost/${ACCT_DB_NAME}\""$'\n'"fairshare_refresh_secs = 30"$'\n'
fi

wg_line="wg_enabled = false"
[ "$TRANSPORT" = wireguard ] && wg_line="wg_enabled = true"$'\n'"wg_interface = \"spur0\""

for ctl in "${CONTROLLERS[@]}"; do
  raft_block=""
  if $ha_enabled; then
    raft_block="node_id = ${NODE_ID[$ctl]}"$'\n'"peers = [${peers_csv}]"
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
${acct_block}
[network]
${wg_line}
agent_port = ${SPUR_AGENT_PORT}
${nodes_blocks}
[[partitions]]
name = "default"
default = true
nodes = "${part_csv}"
max_time = "INFINITE"
EOF
  scp -q "$tmp" "$ctl:${SPUR_HOME}/etc/spur.conf"
  rm -f "$tmp"

  # Install + (re)start the spurctld systemd unit.
  ssh "$ctl" "cat > /etc/systemd/system/spurctld.service <<UNIT
[Unit]
Description=Spur Controller Daemon (spurctld)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=HOME=${SPUR_HOME}
WorkingDirectory=${SPUR_HOME}
ExecStart=${SPUR_INSTALL_DIR}/spurctld -f ${SPUR_HOME}/etc/spur.conf --state-dir ${SPUR_HOME}/state --log-level ${SPUR_LOG_LEVEL}
Restart=on-failure
RestartSec=3
User=root
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable spurctld
  systemctl restart spurctld
  "
done

# Wait for every controller's gRPC port to bind.
for ctl in "${CONTROLLERS[@]}"; do
  ssh "$ctl" "for i in \$(seq 1 30); do ss -tlnH | grep -q ':${SPUR_CONTROLLER_PORT}\b' && exit 0; sleep 1; done; echo 'spurctld did not bind ${SPUR_CONTROLLER_PORT}' >&2; exit 1"
done
```

**HA only:** spurctld binds 6817 immediately but returns `no leader elected yet` until quorum forms. Wait on the first controller:

```bash
if $ha_enabled; then
  ssh "${CONTROLLERS[0]}" "
    for i in \$(seq 1 60); do
      # NOTE: prefix with sudo if the install dir is under /root and you SSH non-root.
      out=\$(${SPUR_INSTALL_DIR}/spur nodes 2>&1); rc=\$?
      # Ready only when the command SUCCEEDS and the output has a real node table.
      # Gate on rc==0 too — a Permission denied / crash must NOT be read as 'leader up'.
      if [ \$rc -eq 0 ] && ! echo \"\$out\" | grep -qE 'no leader|not the Raft leader|cannot reach leader|transport error|Connection refused|Permission denied'; then
        echo OK; exit 0
      fi
      sleep 1
    done
    echo \"timeout waiting for leader: \$out\" >&2; exit 1
  "
fi
```

Set the client env on each **controller** so `squeue`/`sinfo`/`scontrol` (use `SPUR_CONTROLLER_ADDR`) and `sacct`/`sacctmgr`/`sreport`/`sshare` (use `SPUR_ACCOUNTING_ADDR`) work with no per-command flags — otherwise they default to `localhost` and fail on any controller that isn't the accounting host. Controllers only (agents don't run the CLI for users here):

```bash
ACCT_IP="${IP[$ACCT_HOST]:-${ACCT_HOST#*@}}"
for ctl in "${CONTROLLERS[@]}"; do
  ctl_ip="${IP[$ctl]}"
  ssh "$ctl" "
    sudo sed -i '/^SPUR_CONTROLLER_ADDR=/d;/^SPUR_ACCOUNTING_ADDR=/d' /etc/environment
    echo 'SPUR_CONTROLLER_ADDR=http://${ctl_ip}:${SPUR_CONTROLLER_PORT}' | sudo tee -a /etc/environment >/dev/null
    $( [ \"$ACCOUNTING\" = true ] && echo "echo 'SPUR_ACCOUNTING_ADDR=http://${ACCT_IP}:${SPUR_ACCT_PORT}' | sudo tee -a /etc/environment >/dev/null" )
  "
done
```

(`/etc/environment` is read at login, so a fresh shell / `bash -lc` picks it up; for `sudo` also running the CLI, use `sudo -E` to pass the vars through.)

## Step 7: install spurd systemd unit on every agent

`spurd --controller` takes a single URL — point every agent at `CONTROLLERS[0]` (followers forward writes via Raft). `WorkingDirectory=${SPUR_HOME}` in the unit sets the *fallback* stdout dir for `spur-<N>.out` (a job's own `WorkDir`/submit-CWD takes precedence). `--hostname`/`--address` are explicit (auto-detect picks `127.0.0.1`, breaking inter-node dispatch).

```bash
CTL0="${IP[${CONTROLLERS[0]}]}"
for ag in "${AGENTS[@]}"; do
  ssh "$ag" "cat > /etc/systemd/system/spurd.service <<UNIT
[Unit]
Description=Spur Node Agent (spurd)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=HOME=${SPUR_HOME}
WorkingDirectory=${SPUR_HOME}
ExecStart=${SPUR_INSTALL_DIR}/spurd --controller http://${CTL0}:${SPUR_CONTROLLER_PORT} --hostname ${SHORT[$ag]} --address ${IP[$ag]} --listen 0.0.0.0:${SPUR_AGENT_PORT} --log-level ${SPUR_LOG_LEVEL}
Restart=on-failure
RestartSec=3
User=root
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable spurd
  systemctl restart spurd
  for i in \$(seq 1 30); do ss -tlnH | grep -q ':${SPUR_AGENT_PORT}\b' && exit 0; sleep 1; done
  echo 'spurd did not bind ${SPUR_AGENT_PORT}' >&2; exit 1
  "
done
```

## Step 8: wait for every agent to register

`spur nodes` collapses by partition, so check per-agent with `spur show node`:

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

### Single-node job (every deploy)

```bash
ssh "${CONTROLLERS[0]}" "cat > /tmp/spur-test-single.sh <<'EOF'
#!/bin/bash
#SBATCH --job-name=spur-test-single
echo \"ran on \$(hostname) at \$(date)\"
EOF
chmod +x /tmp/spur-test-single.sh
cd /tmp   # predictable, world-accessible WorkDir; job stdout -> /tmp/spur-<JOBID>.out. Do NOT cd into a 0700 dir like /root/spur when SSHing non-root.
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

Output lands in `spur-<JOBID>.out` on whichever agent ran the job, in the job's `WorkDir` — i.e. the CWD at submit time (`/tmp` above). It is NOT necessarily `${SPUR_HOME}`. To locate it robustly, loop agents and search the likely dirs: `sudo find /tmp ${SPUR_HOME} /home /root -maxdepth 2 -name 'spur-<JOBID>.out'` (no shared-FS assumption; use `sudo` for non-root SSH).

### Multi-node job (when |AGENTS| ≥ 2)

`-N` must not exceed the agent count. Use `<<'EOF'` so `$SPUR_*` vars reach the agent verbatim and expand at run time; inject `N` via sed.

```bash
N=${#AGENTS[@]}
ssh "${CONTROLLERS[0]}" "cat > /tmp/spur-test-multi.sh <<'EOF'
#!/bin/bash
#SBATCH --job-name=spur-test-multi
#SBATCH -N __N__
#SBATCH --ntasks-per-node=1
echo \"node \$SPUR_TASK_OFFSET of \$SPUR_NUM_NODES on \$(hostname); peers=\$SPUR_PEER_NODES\"
EOF
sed -i 's/__N__/${N}/' /tmp/spur-test-multi.sh
chmod +x /tmp/spur-test-multi.sh
cd /tmp   # predictable WorkDir (see single-node note); output -> /tmp/spur-<JOBID>.out per node
jid=\$(${SPUR_INSTALL_DIR}/spur submit /tmp/spur-test-multi.sh | grep -oE '[0-9]+')
echo \"JOBID=\$jid\"
for i in \$(seq 1 60); do
  st=\$(${SPUR_INSTALL_DIR}/spur show job \$jid 2>/dev/null | grep -oE 'JobState=[A-Z]+' | head -1 | cut -d= -f2)
  case \"\$st\" in COMPLETED) echo OK; break ;; FAILED|CANCELLED|TIMEOUT|NODE_FAIL) echo \"BAD: \$st\" >&2; exit 1 ;; esac
  sleep 1
done
"
# Multi-node writes locally on each node — fetch from every agent.
for ag in "${AGENTS[@]}"; do
  echo "=== ${SHORT[$ag]} ==="
  ssh "$ag" "ls ${SPUR_HOME}/spur-*.out 2>/dev/null | xargs -r tail -n +1"
done
```

### Accounting check (when `ACCOUNTING=true`)

```bash
ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/sacct | head"   # sacct works from any controller (gRPC to spurdbd)
ssh "$ACCT_HOST" "sudo -u postgres psql -d ${ACCT_DB_NAME} -tAc 'SELECT count(*) FROM jobs;'"   # postgres lives on ACCT_HOST
```
Expect a row per completed job. (With `ACCOUNTING=false`, `sacct` is expected to fail — that's fine; jobs still run.)

## Step 10: verify

```bash
ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur nodes"
ssh "${CONTROLLERS[0]}" "${SPUR_INSTALL_DIR}/spur queue"

# Every daemon should be systemd-active.
for ctl in "${CONTROLLERS[@]}"; do ssh "$ctl" "systemctl is-active spurctld"; done
for ag  in "${AGENTS[@]}";      do ssh "$ag"  "systemctl is-active spurd"; done
[ "$ACCOUNTING" = true ] && ssh "$ACCT_HOST" "systemctl is-active spurdbd postgresql"

# HA: identify the CURRENT leader. Every node that was ever leader has a
# 'become leader' line, so grep -m1 (first match) is wrong after any
# re-election. The authoritative current leader is in each node's persisted
# vote — read node_id from vote.json (identical on all healthy peers).
if $ha_enabled; then
  ssh "${CONTROLLERS[0]}" "sudo cat ${SPUR_HOME}/state/raft/vote.json 2>/dev/null" \
    | grep -oE '\"node_id\":[0-9]+' | tail -1 | sed 's/.*://' \
    | xargs -I{} echo "current Raft leader: node_id={}"
fi

# Separate-compute HA sanity: controllers that are NOT agents must have spurd inactive.
for ctl in "${CONTROLLERS[@]}"; do
  is_agent=false; for a in "${AGENTS[@]}"; do [ "$a" = "$ctl" ] && is_agent=true; done
  $is_agent || ssh "$ctl" "systemctl is-active spurd 2>/dev/null | grep -q inactive && echo '$ctl: no agent (correct)' || echo '$ctl: unexpected spurd'"
done
```

## Step 11: teardown (only when asked)

```bash
for tgt in "${HOSTS_ALL[@]}"; do
  ssh "$tgt" "
    for svc in spurd spurctld spurdbd; do systemctl disable --now \$svc 2>/dev/null || true; done
    pkill -x spurd 2>/dev/null; pkill -x spurctld 2>/dev/null; pkill -x spurdbd 2>/dev/null
    rm -f /etc/systemd/system/spurd.service /etc/systemd/system/spurctld.service /etc/systemd/system/spurdbd.service
    systemctl daemon-reload 2>/dev/null || true
    rm -rf ${SPUR_HOME}
    rm -f /root/spur-*.out ${SPUR_INSTALL_DIR}/spur-*.out /tmp/spur-*.out
  "
done
```

To also remove accounting data (destructive): `ssh "$ACCT_HOST" "sudo -u postgres dropdb ${ACCT_DB_NAME}; sudo -u postgres dropuser ${ACCT_DB_USER}"`. Only do this if the user explicitly asks — it deletes all job history. Leave PostgreSQL itself installed unless asked to purge it.

## Gotchas (all hard-won — don't relearn them)

### Install / systemd
- **This skill uses systemd**, not `nohup`. Units are `/etc/systemd/system/spur{ctld,d,dbd}.service`, `enabled` (survive reboot), `Restart=on-failure`. Always `systemctl daemon-reload` after writing a unit.
- **`spur --version` is NOT supported** — it errors. Check the binary with `test -x`, not by running `--version`.
- **`install.sh` returns 403 when the repo has no published release.** Set `SPUR_BINARY_SRC` to a local dir of pre-built binaries and the skill scp's them instead.
- **Slurm-compat symlinks** (`sbatch`/`squeue`/`sinfo`/`scancel`/`sacct`/`scontrol`/`salloc`/`srun` → `spur`) are created on every install path. The `spur` multi-call binary dispatches on argv[0].
- **`pkill -f spurd` also kills `spurctld`** (substring match). Always `pkill -x` (exact name).

### Accounting
- **Deploy accounting BEFORE the controller.** spurctld reads the `[accounting]` block at start and connects to spurdbd; if spurdbd isn't up yet, wire it first (Step 5 precedes Step 6).
- **spurdbd needs `--migrate`** to create the accounting tables on first run.
- **Accounting is one instance for the whole cluster**, on `CONTROLLERS[0]`. Don't run it per-node.
- **Default DB creds are `spur/spur/spur`** — fine for a lab, change `ACCT_DB_PASSWORD` for anything real. Flag this to the user.
- **`SPUR_WIPE_STATE` defaults to `false`** — re-runs and upgrades preserve the Raft job queue and node registrations. `SPUR_WIPE_STATE=true` resets the Raft job-id counter (job ids restart at 1, upserting onto the same accounting rows); use it only for a fresh install or intentional reinit.
- **Rebuild all four binaries together for an upgrade.** The daemons share a Raft WAL schema and a spurdbd DB schema. Pushing a `spurctld` built from a different tree than its peers can crash it on start (`unknown variant …` / `LogIndex(N) violates`) when it reads a log entry it can't parse.
- **Changing the controller set needs a wipe** (Spur 0.3.0 has no online membership change). Adding/removing/reordering a controller with state preserved leaves openraft with a mismatched on-disk membership. Agents are not Raft members — add/remove them freely.
- **A stale dpkg lock** (`Could not get lock /var/lib/dpkg/lock-frontend`) means another apt/unattended-upgrade is running. Wait for it, or clear a genuinely hung `apt-get` before retrying — don't `--force`.

### Spur quirks
- **Output file `spur-<N>.out` goes to the job's `WorkDir` = the CWD at submit time.** Submitting from `/tmp` writes `/tmp/spur-<N>.out`; submitting from the SSH user's home writes it there. Do NOT `cd` into a 0700 dir (e.g. `/root/spur`) when SSHing as a non-root user — the `cd` fails and WorkDir silently becomes the user's home. Pin the submit CWD to `/tmp` for predictability, and search `/tmp /home /root ${SPUR_HOME}` when hunting for output.
- **`spur nodes` collapses by partition.** To verify per-host registration, loop `spur show node <name>`.
- **`spur show node <name>` is a prefix match**, not exact. Colliding prefixes (`gpu-a` vs `gpu-ab`) return both; disambiguate with `awk -v n=<name> '/^NodeName=/{p=($0=="NodeName="n)} p'`.
- **`spur show job` uses `JobState=COMPLETED` (uppercase).** Parse `JobState=[A-Z]+`.
- **Raft port 6821 is hardcoded** in spurctld (not a CLI flag). Preflight must include it.
- **Harmless log spam `invalid transition from Completed to Completed`** on followers after multi-node jobs — the job actually succeeded.

### Multi-node / HA specifics
- **Agent `--hostname` must match the `[[nodes]]` name in `spur.conf`** and `spur show node <name>`. Use `hostname -s` consistently.
- **Pass `--hostname` and `--address` explicitly to spurd.** Auto-detect picks `127.0.0.1`, breaking inter-node dispatch.
- **No shared-FS assumption.** Each node writes its own `spur-<JOBID>.out` locally; fetch from every agent.
- **HA needs a leader-elected wait**, not just port-listening. Loop on `no leader elected yet` until it clears.
- **HA `peers` list order must be stable across redeploys.** `node_id` is the 1-based position; reordering breaks openraft. To re-order, wipe state on every controller and redeploy.
- **`-N` in a multi-node job must not exceed the agent count**, or it stays PENDING.
- **Separate-compute HA:** controllers NOT in `AGENTS` must have no `spurd`. Verify with `systemctl is-active spurd` → `inactive`.
- **Client-side failover is NOT implemented in Spur 0.3.0.** `spurd --controller` is a single URL; if `CONTROLLERS[0]` dies, agents are stranded even with a live Raft leader. Production HA needs an L4 VIP / DNS in front of `:6817`.

## Report back

End the run with:
- Mode + counts (X controllers, Y agents), transport, accounting on/off
- Per-host: install source (binary-src vs installer), `systemctl is-active` for each daemon, log source (`journalctl -u spurctld`/`spurd`/`spurdbd`)
- `spur nodes` output
- HA only: which `node_id` became leader
- Accounting only: `sacct` output + DB job count
- Test job IDs + stdout (single, and multi if run)
- Any deviation from this skill — flag it so the skill can be patched
