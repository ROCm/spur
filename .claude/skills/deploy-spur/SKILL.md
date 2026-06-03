---
name: deploy-spur
description: Use when the user asks to deploy/install Spur on one or more bare-metal hosts over SSH. Handles single-node, multi-node, and HA (multi-controller Raft) deployments through one Ansible playbook. ALWAYS asks the user up-front for mode + topology + controller count before touching anything.
---

# Deploy Spur — Unified

Spur is an AI-native job scheduler with three core daemons:

- **spurctld** — controller / scheduler / Raft consensus (1 instance, or ≥ 2 for HA)
- **spurd** — node agent, runs on every compute host
- **spurrestd / spurdbd** — optional REST + accounting (out of scope here)

This skill stands the cluster up via the project's Ansible playbook at `deploy/ansible/`. The same `deploy.yml` covers single-node → multi-node → HA — only the inventory differs.

## Step 0: gather inputs from the user (MANDATORY — do not skip)

Before any SSH or playbook run, **ask the user**:

1. **Deployment mode** — pick exactly one:
   | Mode | Use when |
   |---|---|
   | `single-node` | one host, controller and agent on the same machine |
   | `multi-node` | one controller, N compute agents |
   | `ha` | ≥ 2 controllers (Raft consensus), any number of agents |

2. **Hosts** — for each role:
   - `CONTROLLERS` — SSH targets that will run `spurctld`. Required count:
     - single-node: 1
     - multi-node: 1
     - ha: **odd number, ≥ 3 recommended.** N=2 is allowed but has zero fault tolerance (use only for code-path testing). Quorum = ⌊N/2⌋+1; tolerates ⌊(N−1)/2⌋ failures.
   - `AGENTS` — SSH targets that will run `spurd`. Any number ≥ 1. Hosts can appear in **both** lists (hyperconverged).

3. **Transport** — `direct` (LAN, default) or `wireguard` (encrypted mesh for cross-NAT). If `wireguard`, also collect per-host WG addresses in the chosen CIDR.

Use `AskUserQuestion` for any of these the user didn't specify in their prompt. Don't guess.

> **For HA mode, explicitly warn the user** if their controller count is even or < 3:
> - `N=1` → not HA, suggest `multi-node` mode instead
> - `N=2` → call out "zero fault tolerance" before proceeding
> - even N ≥ 4 → suggest N−1 (strictly better)

## Step 1: preflight all hosts

```bash
for tgt in "${CONTROLLERS[@]}" "${AGENTS[@]}"; do
    ssh -o BatchMode=yes -o ConnectTimeout=10 "$tgt" '
        set -e
        echo "=== $(hostname) ==="
        uname -r
        nproc
        ss -tlnp 2>/dev/null | grep -E ":(6817|6818|6820|6821) " || echo "spur ports free"
        pgrep -x spurctld; pgrep -x spurd; pgrep -x spurrestd
        command -v curl tar
        ip -4 addr show | grep -E "inet " | grep -v 127.0.0.1
    ' &
done
wait
```

Fail fast if ports busy by something other than Spur, or `curl`/`tar` missing.

If `TRANSPORT=wireguard`, also: `modinfo wireguard` and `command -v wg` on every host. Install `wireguard-tools` if missing.

## Step 2: write the Ansible inventory

The playbook auto-derives the mode from the inventory:

```ini
; SINGLE-NODE — same host in both groups
[spur_controllers]
node1 ansible_host=10.0.0.10 ansible_user=root
[spur_agents]
node1 ansible_host=10.0.0.10 ansible_user=root
[all:vars]
spur_transport=direct
```

```ini
; MULTI-NODE — one controller, N agents
[spur_controllers]
ctl ansible_host=10.0.0.10 ansible_user=root
[spur_agents]
ctl   ansible_host=10.0.0.10 ansible_user=root   ; (optional) controller also a worker
gpu-1 ansible_host=10.0.0.11 ansible_user=root
gpu-2 ansible_host=10.0.0.12 ansible_user=root
[all:vars]
spur_transport=direct
```

```ini
; HA — N controllers + M agents
[spur_controllers]
ctl-0 ansible_host=10.0.0.10 ansible_user=root
ctl-1 ansible_host=10.0.0.11 ansible_user=root
ctl-2 ansible_host=10.0.0.12 ansible_user=root
[spur_agents]
gpu-1 ansible_host=10.0.0.21 ansible_user=root
gpu-2 ansible_host=10.0.0.22 ansible_user=root
[all:vars]
spur_transport=direct
```

```ini
; WIREGUARD overlay — add per-host spur_wg_address and the transport var
[spur_controllers]
ctl ansible_host=ctl.example.com ansible_user=root spur_wg_address=10.44.0.1
[spur_agents]
gpu-1 ansible_host=gpu1.example.com ansible_user=root spur_wg_address=10.44.0.2
gpu-2 ansible_host=gpu2.example.com ansible_user=root spur_wg_address=10.44.0.3
[all:vars]
spur_transport=wireguard
spur_wg_cidr=10.44.0.0/16
spur_wg_port=51820
```

Save to `deploy/ansible/inventory/hosts.ini` (or another name and pass `-i`).

## Step 3: run the playbook

```bash
cd deploy/ansible
ansible-playbook deploy.yml -i inventory/hosts.ini
```

The playbook is idempotent. To redeploy without wiping Raft state:
```bash
ansible-playbook deploy.yml -i inventory/hosts.ini -e spur_wipe_state=false
```

The play ends with `spur_verify` running a single-node test job and (when ≥ 2 agents) a multi-node test job. Look for `JobState=COMPLETED` in the output.

## Step 4: verify

```bash
# From a controller
spur nodes                          # all agents idle/mixed
spur queue                          # empty (or your test jobs)
spur show node <name>               # per-node detail
spur --version                      # daemon binaries don't accept --version; only spur CLI does

# Raft (HA mode only) — check leader was elected
grep "become leader" /root/spur/log/spurctld.log
# Expect exactly one controller to log this. Followers show vote rounds but no "become leader".
```

In HA mode you can submit through any controller — non-leaders forward to the leader internally (`crates/spurctld/src/server.rs:42`).

## Step 5: teardown

```bash
ansible-playbook teardown.yml -i inventory/hosts.ini             # stop daemons
ansible-playbook teardown.yml -i inventory/hosts.ini -e wipe=true # also rm -rf ~/spur
```

## Gotchas (all hard-won during validation — don't relearn them)

### Daemon / process management
- **`-D` means FOREGROUND, not daemonize.** Verified in `crates/spurctld/src/main.rs:34`. Use `nohup ... < /dev/null & disown`; never pass `-D` when backgrounding.
- **`pkill -f spurd` also kills `spurctld`** (substring match). Always `pkill -x` (exact name).
- **SSH backgrounding hangs without `< /dev/null` and `disown`.** Both mandatory.
- **`mkdir -p {a,b,c}` brace expansion can fail under `set -e` over SSH** — use explicit paths.

### Spur quirks
- **Output file is `spur-<N>.out` in spurd's CWD at startup**, not the submitter's CWD (despite what `spur show job` claims in `WorkDir=`). The Ansible role does `cd $SPUR_HOME` before launching spurd so the location is predictable.
- **`spur nodes` collapses by partition** — counts nodes in one row. To verify per-host registration, loop `spur show node <name>`.
- **`spur show job` uses `JobState=COMPLETED` (uppercase)**, not `State: Completed`. Parse `JobState=[A-Z]+`.
- **Raft port 6821 is hardcoded** in spurctld (not a CLI flag). Preflight must include it.
- **Harmless log spam: `invalid transition from Completed to Completed`** on followers after multi-node jobs (`crates/spurctld/src/cluster.rs:359`). Leader already reported terminal state; followers' duplicate reports get rejected. Job actually succeeded.

### Multi-node / HA specifics
- **Agent `--hostname` must match the `[[nodes]]` name in `spur.conf`** and `spur show node <name>`. Use `hostname -s` consistently.
- **Pass `--hostname` and `--address` explicitly to spurd** in multi-node. Auto-detect picks `127.0.0.1` which makes inter-node dispatch fail.
- **No shared FS assumption.** In a multi-node job, each node writes its own `spur-<JOBID>.out` locally. Fetch from every agent, not just the controller.
- **HA needs a leader-elected wait**, not just port-listening. `spurctld` binds `:6817` immediately but returns `Status::unavailable("no leader elected yet")` until the Raft quorum forms. Loop on that error message in `spur nodes` until it clears.
- **HA `peers` list order must be stable across redeploys.** `node_id` is the 1-based position; reordering breaks openraft membership. To re-order, wipe `~/spur/state/raft/` on every controller and redeploy.
- **Client-side failover is NOT implemented in Spur 0.3.0.** `spurd --controller` takes a single URL. If that controller dies, agents are stranded even if Raft still has a leader. Production HA needs an L4 VIP / DNS in front of `:6817`.

### Ansible specifics
- **`ansible.builtin.command` runs without a shell** — `command -v X` fails (it's a bash builtin). Use `shell:` with `executable: /bin/bash`.
- **`delegate_facts: true` evaluates the value on the controller, not the delegated target.** `set_fact: x: "{{ ansible_hostname }}"` with `delegate_to: y` writes the **controller's** hostname to `y`. Use `hostvars[item].ansible_hostname`.

## Report back

End the run with:
- Mode used (single-node / multi-node / ha) + transport + counts (X controllers, Y agents)
- Per-host: install version, daemon PIDs, log paths
- `spur nodes` output
- HA only: which `node_id` became leader (`grep "become leader" .../spurctld.log`)
- Test job IDs + stdout
- Any deviation from this skill — flag it so the skill can be patched

## Verified working

| Date | Mode | Hosts | Transport | Notes |
|---|---|---|---|---|
| 2026-06-03 | single-node | 1× Ubuntu 22.04 MI300X node | direct | end-to-end pass |
| 2026-06-03 | multi-node (1 ctl + 2 agents) | 2× Ubuntu 22.04 MI300X nodes on a `/24` | direct | `-N 2` fan-out OK |
| 2026-06-03 | **HA (2 ctl, hyperconverged)** | 2× Ubuntu 22.04 MI300X nodes on a `/24` | direct | leader=`node_id` 2, leader-forwarding via follower verified. N=2 = zero fault tolerance (code-path only) |
| pending | any | — | wireguard | role exists, **not validated end-to-end** |
