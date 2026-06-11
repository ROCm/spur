# Spur — Ansible Deployment

One playbook (`deploy.yml`) deploys Spur in three shapes, all driven by inventory:

| Shape | Inventory pattern | Transport |
|---|---|---|
| Single-node | one host in **both** `spur_controllers` and `spur_agents` | local loopback |
| Multi-node — direct LAN | one host in `spur_controllers`, all compute in `spur_agents` | LAN IP, unencrypted |
| Multi-node — WireGuard mesh | as above + `spur_transport=wireguard` and `spur_wg_address` per host | encrypted mesh on `spur0` |
| **HA — multi-controller Raft** | **≥ 2 hosts in `spur_controllers`** (any number in `spur_agents`); auto-enabled | direct or wireguard |

The playbook is idempotent — re-running it on a healthy cluster only restarts daemons and re-applies config.

## Requirements

- Control node: `ansible-core >= 2.14`
- Target hosts: SSH-reachable, sudo or root, `curl` + `tar` present
- WireGuard transport only: kernel module `wireguard` and package `wireguard-tools`

```bash
python3 -m pip install --user 'ansible-core>=2.14'
```

## Quickstart

```bash
cd deploy/ansible
cp inventory/hosts.example.ini inventory/hosts.ini
# Edit inventory/hosts.ini to match your hosts (pick ONE of the three shapes)
ansible-playbook deploy.yml -i inventory/hosts.ini
```

The play ends by running both a single-node and (when ≥ 2 agents) a multi-node test job.
Look for `spur nodes` output and the job stdout in the play output.

## Inventory examples

### Single-node

```ini
[spur_controllers]
node1 ansible_host=10.0.0.10 ansible_user=root

[spur_agents]
node1 ansible_host=10.0.0.10 ansible_user=root
```

### Multi-node, direct LAN

```ini
[spur_controllers]
ctl ansible_host=10.0.0.10 ansible_user=root

[spur_agents]
ctl   ansible_host=10.0.0.10 ansible_user=root   ; controller also runs an agent
gpu-1 ansible_host=10.0.0.11 ansible_user=root
gpu-2 ansible_host=10.0.0.12 ansible_user=root

[all:vars]
spur_transport=direct
```

### Multi-node, WireGuard mesh

```ini
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

> WireGuard hosts must each have a unique `spur_wg_address` in the `spur_wg_cidr`. The controller defaults to `.1`; agents must be set explicitly.

### HA — multi-controller Raft

Put `N` hosts in `[spur_controllers]` and the playbook auto-enables Raft consensus across them. Quorum math: tolerates `floor((N-1)/2)` controller failures.

```ini
[spur_controllers]
ctl-0 ansible_host=10.0.0.10 ansible_user=root
ctl-1 ansible_host=10.0.0.11 ansible_user=root
ctl-2 ansible_host=10.0.0.12 ansible_user=root   ; 3 → tolerates 1 failure

[spur_agents]
gpu-1 ansible_host=10.0.0.21 ansible_user=root
gpu-2 ansible_host=10.0.0.22 ansible_user=root
```

What the playbook does in HA mode:

- Writes the same `peers = [...]` list to every controller's `spur.conf` (order matters — don't reorder controllers between deploys).
- Assigns `node_id` = the controller's 1-based position in `groups['spur_controllers']`.
- Waits for a leader to be elected before proceeding to agent registration.
- Non-leader controllers forward client RPCs to the leader internally — clients can talk to any controller.

**Always use an odd `N` ≥ 3 in production.** Even N gives the same fault tolerance as `N-1` and is strictly worse. `N=2` has zero fault tolerance (both must be up) — only useful for exercising the HA code path on a 2-node lab.

**Client-side failover is NOT automatic** in Spur 0.3.0. `spurd --controller` accepts a single URL — the playbook points it at `groups['spur_controllers'][0]`. If that host dies, agents lose their connection even though the Raft cluster still has a leader on surviving controllers. Production HA needs:

- An L4 VIP / DNS round-robin in front of `:6817` across all controllers, and
- Setting `ansible_host` on the first controller (or overriding `spur_controller_addr`) to that VIP / DNS name.

A full HA inventory template lives at `inventory/hosts.ha.example.ini`.

## Variables (defaults in `group_vars/all.yml`)

| Variable | Default | What it does |
|---|---|---|
| `spur_cluster_name` | `spur-cluster` | `cluster_name` in `spur.conf` |
| `spur_version` | `latest` | Install channel: `latest` / `nightly` / `vX.Y.Z` |
| `spur_install_dir` | `/root/.local/bin` | Where the installer drops binaries (also added to `/etc/environment`) |
| `spur_home` | `/root/spur` | Per-host state/log/etc root |
| `spur_transport` | `direct` | `direct` or `wireguard` |
| `spur_wg_cidr` / `spur_wg_port` / `spur_wg_interface` | `10.44.0.0/16` / `51820` / `spur0` | WireGuard mesh settings |
| `spur_controller_port` / `spur_agent_port` / `spur_raft_port` | `6817` / `6818` / `6821` | Listen ports |
| `spur_log_level` | `info` | Daemon log verbosity |
| `spur_wipe_state` | `true` | Wipe `~/spur/state` on (re)deploy. Set `false` in prod after first run. |

Override per-run with `-e key=value`:

```bash
ansible-playbook deploy.yml -i inventory/hosts.ini -e spur_version=nightly -e spur_wipe_state=false
```

## Tear down

```bash
ansible-playbook teardown.yml -i inventory/hosts.ini             # stop daemons
ansible-playbook teardown.yml -i inventory/hosts.ini -e wipe=true # also rm -rf ~/spur
```

## Hard-won gotchas baked into these roles

These are real bugs we hit during validation — listed here so anyone reading the playbook understands why the roles look the way they do.

- **`-D` is `--foreground`, not "daemonize".** Confirmed in `crates/spurctld/src/main.rs:34`. The roles never pass `-D` and rely on each binary's built-in daemonize behavior plus `nohup ... < /dev/null & disown` to survive SSH disconnect.
- **`pkill -f spurd` also kills `spurctld`** (substring match). All roles use `pkill -x` exclusively.
- **SSH backgrounding hangs without `< /dev/null` and `disown`.** Without both, Ansible's `shell` module can keep stdin open or the daemon can die when the connection closes.
- **Agent `--hostname` must match the `[[nodes]]` name in `spur.conf`.** The template uses `ansible_hostname` (short) on both sides to stay consistent.
- **Output file goes to submitter's CWD, not `/tmp`.** `spur_verify` runs `cd {{ spur_home }}` before submitting and reads from `{{ spur_home }}/spur-<JOBID>.out`.
- **No shared-FS assumption in multi-node verify.** The play fetches `/root/spur-<JOBID>.out` from each agent via `delegate_to`.
- **Harmless log spam: `invalid transition from Completed to Completed`** on follower nodes after a multi-node job. Origin: `crates/spurctld/src/cluster.rs:359`. The leader already reported terminal state; the follower's redundant report is rejected. Job actually succeeded.
- **Raft port 6821 is hardcoded** in spurctld and isn't a flag in 0.3.0. The preflight checks it alongside 6817/6818.
- **Job stdout file lands in spurd's working directory at startup**, not the submitter's CWD as Slurm-compatibility docs claim. `spur show job` reports `WorkDir=…` but that's the submitter's CWD; the *actual* file is wherever spurd was launched from. The role explicitly `cd {{ spur_home }}` before starting spurd so the file is predictably at `{{ spur_home }}/spur-<JOBID>.out`.
- **The single-node job's host is unpredictable.** The backfill scheduler picks any idle node, so `spur_verify` searches every agent in parallel rather than assuming the controller node.
- **`spur nodes` collapses by partition** — the "NODES" column shows count, not one row per node. To check that *all expected* hostnames registered, loop `spur show node <name>` per host instead of counting rows.
- **`spur show job` uses `JobState=COMPLETED` (uppercase)**, not `State: Completed`. The wait-loop greps `JobState=[A-Z]+` for robustness across versions.
- **`delegate_facts: true` evaluates the `set_fact` value on the *controller*, not the delegated target.** So `spur_node_name: "{{ ansible_hostname }}"` with `delegate_to: x` writes the controller's hostname to host `x`. Use `hostvars[item].ansible_hostname` instead.
- **`ansible.builtin.command` runs without a shell** — `command -v X` fails because `command` is a bash builtin. Use `ansible.builtin.shell` with `executable: /bin/bash`.
- **HA needs a leader-elected wait**, not just a port-listening wait. `spurctld` binds `:6817` instantly but returns `Status::unavailable("no leader elected yet")` until the Raft quorum forms. The controller role greps for that error message in `spur nodes` output and loops until it goes away.
- **`peers` list order matters across controllers.** `node_id` is the 1-based position in `controller.peers`; openraft refuses to start if a node's `node_id` doesn't match its position. The role derives both from `groups['spur_controllers']` and writes the same list to every controller, so don't reorder that group between deploys without wiping state.

If you hit a new gotcha, please add it here and (where applicable) encode the fix in the roles.

## Verified

`spur 0.3.0`, Ubuntu 22.04, validated four ways on 2026-06-03:

- **2-node, direct LAN, single controller**: fresh deploy → both nodes idle, `-N 2` job produced distinct `$SPUR_TASK_OFFSET` 0/1.
- **Idempotent re-run** (`-e spur_wipe_state=false`): daemons restarted cleanly, tests passed again.
- **Single-node** (same host in both groups): multi-node test correctly skipped, single-node test passed.
- **HA — 2 controllers + 2 agents, hyperconverged**: both `spurctld` started, openraft elected one as leader (`node_id=2`), `-N 2` job dispatched cleanly. Confirmed leader forwarding by submitting via the follower — submission succeeded, job scheduled and ran. Note: `N=2` has zero fault tolerance; this test exercised the code path, not real fault tolerance.

WireGuard transport is implemented in `roles/spur_wireguard` but was not validated end-to-end because both test hosts shared a `/24`. Review before relying on it in production.
