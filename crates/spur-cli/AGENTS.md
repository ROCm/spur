# AGENTS.md -- spur-cli

Multi-call CLI binary for the Spur scheduler. Compiles to a single `spur`
binary that dispatches based on argv[0], so symlinks named after Slurm
commands (`sbatch`, `squeue`, `sinfo`, ...) work out of the box.

## Overview

`spur-cli` is the user-facing CLI for Spur, an AI-native job scheduler and
Slurm drop-in replacement. It provides two invocation modes:

1. **Symlink mode** -- invoke as `sbatch`, `squeue`, etc. (argv[0] dispatch).
2. **Native mode** -- invoke as `spur <command>`, where commands use
   human-friendly names (`submit`, `queue`, `nodes`, `cancel`, ...).

Both modes parse the same args and hit the same gRPC controller (spurctld).
The binary restores SIGPIPE default handling before any I/O so piped output
exits cleanly.

## Directory structure

```
src/
  main.rs              Multi-call entry point, argv[0] dispatch, native subcommand routing
  authclient.rs        gRPC channel + auth interceptor (JWT and native plugin)
  auth_keys.rs         `spur auth-keys` JWKS generation
  clap_exit.rs         Clap parse helpers (exit codes matching Slurm)
  env_defaults.rs      SPUR_*/SBATCH_*/SLURM_* env-var resolution for all commands
  exec.rs              `spur exec` -- exec into a running container job
  exit_fmt.rs          Exit code:signal formatting (Slurm's N:S convention)
  export_env.rs        --export ALL/NONE resolution (sbatch + srun shared)
  format_engine.rs     Slurm-compatible %-format string parser and renderer
  image.rs             `spur image` -- import/list/remove container images
  interactive.rs       PTY session, keepalive, Ctrl-C cancel, agent connection
  job_id_arg.rs        Job ID parsing (comma-separated)
  jobtime.rs           Effective start/end time helpers for squeue/sacct
  k8s.rs               `spur k8s` -- k0s cluster management (up/down/status/kubeconfig/install-k0s/add-nodes/remove-nodes)
  mock_agent.rs        [test] Mock agent for PTY/streaming tests
  mock_controller.rs   [test] Mock controller for wait/step tests
  net.rs               `spur net` -- WireGuard mesh (init/join/status)
  node.rs              `spur node` -- node management subcommands
  nodelist.rs          --nodelist/--nodefile resolution
  privilege.rs         Client-side privilege check (root or sudo/wheel group)
  reason.rs            Node reason formatting (user@time)
  sacct.rs             sacct -- job accounting history
  sacctmgr.rs          sacctmgr -- account/user/QOS management
  salloc.rs            salloc -- interactive resource allocation
  sattach.rs           sattach -- attach to a running job's I/O
  sbatch.rs            sbatch -- batch job submission
  scancel.rs           scancel -- cancel jobs
  scontrol.rs          scontrol -- admin control (show/update/create/delete)
  scrontab.rs          scrontab -- recurring cron-style jobs
  sdiag.rs             sdiag -- scheduler diagnostics
  sinfo.rs             sinfo -- node/partition information
  smd.rs               smd -- node health monitoring
  sprio.rs             sprio -- job priority breakdown
  spur_config.rs       Config file loader (/etc/spur/spur.conf or $SPUR_CONF)
  squeue.rs            squeue -- job queue viewer
  sreport.rs           sreport -- usage reports
  srun.rs              srun -- interactive/parallel job execution
  sshare.rs            sshare -- fair-share information
  sstat.rs             sstat -- running job statistics
  strigger.rs          strigger -- event trigger management
  submitline.rs        Reconstruct the submit command line for accounting
  timearg.rs           Time argument parser (ISO 8601, relative, Slurm forms)
  timefmt.rs           Timestamp/duration formatters
  token.rs             spur token -- admission and user token management
tests/
  broken_pipe.rs       SIGPIPE handling regression test
  help_output.rs       Help text validation
```

## Command mapping

### Symlink dispatch (argv[0])

| Symlink     | Spur native alias        | Description                              |
|-------------|--------------------------|------------------------------------------|
| `sbatch`    | `spur submit`            | Submit a batch job script                |
| `srun`      | `spur run`               | Run a parallel/interactive job           |
| `salloc`    | `spur alloc`             | Allocate resources interactively         |
| `squeue`    | `spur queue` / `jobs`    | View the job queue                       |
| `scancel`   | `spur cancel` / `kill`   | Cancel jobs                              |
| `sinfo`     | `spur nodes` / `info`    | View node/partition information          |
| `sacct`     | `spur history` / `acct`  | Job accounting history                   |
| `sacctmgr`  | `spur accounts`/`acctmgr`| Manage accounts, users, QOS              |
| `scontrol`  | `spur show`/`control`/`ctl` | Admin: show/update/create/delete      |
| `sprio`     | `spur priority`/`prio`   | View job priority breakdown              |
| `sshare`    | `spur share`/`fairshare` | Fair-share information                   |
| `sstat`     | `spur stat`/`jobstat`    | Running job statistics                   |
| `sdiag`     | `spur diag`/`diagnostics`| Scheduler diagnostics                    |
| `sreport`   | `spur report`/`usage`    | Usage reports                            |
| `strigger`  | `spur trigger`/`triggers`| Event triggers                           |
| `sattach`   | `spur attach`            | Attach to job I/O                        |
| `scrontab`  | `spur crontab`/`cron`    | Recurring cron-style jobs                |
| `smd`       | `spur health`/`monitor`  | Node health monitoring                   |

### Spur-only commands (no Slurm equivalent)

| Command        | Description                                    |
|----------------|------------------------------------------------|
| `spur net`     | WireGuard mesh network (init/join/status)      |
| `spur k8s`     | k0s cluster management (up/down/status/kubeconfig/install-k0s/add-nodes/remove-nodes) |
| `spur image`   | Container image management (import/list/remove)|
| `spur exec`    | Exec into a running container job              |
| `spur token`   | Admission and user token management            |
| `spur auth-keys` | Generate JWKS for native auth                |
| `spur self-update` | Auto-update from GitHub releases           |

### Special: `spur show`

`spur show node X` maps to `scontrol show node X` -- the implicit `show`
subcommand is injected automatically so users do not need to write
`spur control show node X`.

## Key commands by domain

### Job submission (sbatch, srun, salloc)

All three share the same resource flags (`-N`, `-n`, `-c`, `--gres`,
`--gpus`, `--mem`, `--time`, `--constraint`, `--container-image`, etc.).

**sbatch** resolves args with Slurm precedence: CLI > env > `#SBATCH`
directives > defaults. Directives are parsed from script comments before
submission. `#PBS` directives are auto-converted for PBS migration.

**srun** has three modes:
- Standalone: submits a job, waits for it, streams output.
- Step mode (inside `$SPUR_JOB_ID`): creates a job step in the allocation.
- PTY mode (`--pty`): opens an interactive pseudo-terminal session.
- Daemon placement (`--jobid + --external-launcher`): PRRTE orted fan-out.

**salloc** allocates resources, exports `SPUR_JOB_*` env vars, then execs a
user shell. The shell inherits `SPUR_JOB_ID`, so nested `srun` commands
automatically create steps in the allocation.

### Job query (squeue, sacct, sstat)

**squeue** supports Slurm format strings (`-o "%.18i %.9P"`) and named
field format (`-O JobID:10,Partition`). Sorting via `-S` uses the same
format-spec letters. Default sort: partition asc, state asc, priority desc,
jobid asc. `--start` shows projected start times for pending jobs.

**sacct** queries the accounting service (same gRPC port). Format uses
comma-separated field names (`--format=JobID,JobName,State`).

### Node management (sinfo, scontrol)

**sinfo** groups nodes by display state and compresses hostlists. Overlay
states (plnd, resv, maint) are computed client-side. `-R` lists unavailable
nodes grouped by admin reason with compressed nodelists.

**scontrol** supports both flag-style (`--name`, `--nodes`) and Slurm
inline key=value syntax (`PartitionName=gpu Nodes=n[1-4]`). Hostlist
patterns are expanded client-side via `spur_core::hostlist`.

### Accounting management (sacctmgr)

Full CRUD for accounts, users, QOS, and associations. Supports parsable
output (`-p`/`-P`). Commands: `add`, `modify`, `delete`, `show`, `list`.

### Admin (scontrol)

- `show job/node/partition/reservation/config/assoc_mgr/federation/steps`
- `create/update/delete` for partitions and reservations
- `hold`, `release`, `suspend`, `resume`, `requeue`
- `reconfigure` -- live config reload
- `show hostnames/hostlist/hostlistsorted` -- pure-local transforms

## Output formatting

The format engine (`format_engine.rs`) implements Slurm's `%`-specifier
syntax:

```
%[#][.][width]<letter>
```

- `.` prefix: truncate to width
- `#` prefix: left-aligned (no padding when width is omitted)
- Negative width: left-align
- `%%`: literal percent

Each command has its own `header_map` (letter -> column name) and
`resolve_field` (letter -> value). `squeue` also supports the `-O` named
field form (`-O JobID:10,Partition`).

Default formats:
- squeue: `%.18i %.9P %.8j %.8u %.2t %10M %6D %R`
- sinfo:  `%#P %5a %.10l %.6D %.6t %N`
- sacct:  `%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x`

## Connection to spurctld

All commands connect via `authclient::connect(endpoints)`, which:

1. Reads `SPUR_CONTROLLER_ADDR` env or `--controller` flag (default
   `http://localhost:6817`).
2. Falls back to `[controller] hosts` from `/etc/spur/spur.conf` (or `$SPUR_CONF`).
3. Opens a tonic gRPC channel with configurable timeouts from the
   `[spurd]` config section.
4. Wraps with an `AuthInterceptor` that attaches credentials per-request.

Multiple controller endpoints (comma-separated) are supported for HA
failover.

## Authentication

Two auth plugins, selected by `[auth] plugin` in `spur.conf` or
`$SPUR_AUTH_PLUGIN`:

### JWT plugin (default)

- Reads a bearer token from `$SPUR_AUTH_TOKEN` or `~/.spur/token`.
- Token file must be `chmod 600`; group/other readable files are skipped
  with a warning (the CLI continues without a credential).
- `spur token user --user alice` mints a token locally from `[auth] jwt_key`.
- Static token attached via `Authorization: Bearer <token>` on every RPC.

### Native plugin (`plugin = "spur"`)

- Calls unauthenticated `Ping` first to learn the audience and boot epoch.
- Mints a fresh HMAC-signed credential per RPC from the local Unix socket
  (`/run/spur/<cluster>/auth.sock`).
- Credential carries uid, username, audience, and epoch; verified server-side.
- `spur auth-keys` generates the JWKS file the verifier loads.

## Environment variable precedence

Flags resolve from multiple sources in priority order:

**sbatch**: CLI > env (`SPUR_*` then `SBATCH_*`) > `#SBATCH` directives >
defaults. `SPUR_JOB_NAME` and `SPUR_NODELIST` are NOT read by sbatch
(they are output vars injected into running jobs, and reading them would
make a nested sbatch inherit the parent's placement).

**srun**: CLI > env (`SPUR_*` then `SLURM_*`) > defaults. srun reads
`SLURM_*` because those are inherited from the allocation. `--export`
inheritance: `SRUN_EXPORT_ENV` > `SLURM_EXPORT_ENV`.

**salloc**: same pattern as srun, with `SALLOC_*` variants.

The `env_defaults.rs` module provides `apply_str`, `apply_num`, `apply_csv`,
`apply_flag` helpers that all respect `was_cli_set()`.

## Testing

### Unit tests

Each command module has inline `#[cfg(test)] mod tests` with focused tests.
Test patterns:

- **Parser well-formedness**: `main.rs::every_command_parser_is_well_formed`
  calls `debug_assert()` on every command's clap parser.
- **Mock controller**: `mock_controller.rs` provides an in-process gRPC server
  for testing wait loops, step dispatch, and node operations without network.
- **Mock agent**: `mock_agent.rs` for PTY and streaming tests.
- **Env isolation**: Tests that mutate `SPUR_*/SBATCH_*/SLURM_*` env vars use
  `serial_test::serial(env_injection)` and `EnvGuard` to prevent races.
- **Format-spec coverage**: Every registered header letter/field name is
  asserted to have a matching render arm (prevents half-registered fields).
- **Precedence tests**: sbatch has extensive tests for CLI-over-directive,
  env-over-directive, and CLI-over-env precedence.

### Integration tests (`tests/`)

- `broken_pipe.rs` -- verifies SIGPIPE handling does not panic.
- `help_output.rs` -- validates help text for all commands.

### Running tests

```bash
cargo test -p spur-cli
```

Env-sensitive tests (`#[serial(env_injection)]`) must run serially; `cargo
test` handles this via the `serial_test` crate.

## Common pitfalls

### Slurm compatibility

- **`-h` is `--noheader`** in squeue/sinfo (Slurm convention), NOT `--help`.
  `--help` is re-added as a long-only flag. Tests verify this.
- **`--noconvert`** is accepted but is a no-op. Memory columns always render
  as raw megabytes.
- **`--gres` is last-wins** (via `overrides_with = "self"`), not cumulative.
  Comma-delimited values within one `--gres=gpu:2,fpga:1` expand, but a
  second `--gres` replaces the first. This matches Slurm.
- **`--container-readonly`** is rejected at submission (not silently ignored).
  The runtime does not enforce it; accepting the flag would create a false
  sense of isolation.
- **GPU request exclusivity**: only one of `--gpus`, `--gpus-per-node`,
  `--gpus-per-task`, or `--gres=gpu:*` may be set. Validated client-side.
- **Exit codes**: completed=propagated, failed=max(code,1),
  cancelled/timeout/nodefail/deadline=1. Matches Slurm.

### Version string

Ten symlink names (`srun`, `sbatch`, `sinfo`, `squeue`, `scancel`, `salloc`,
`scontrol`, `sacct`, `sacctmgr`, `sshare`) report `slurm_version_string()`;
the remaining symlinks and native `spur` binary report `version_string()`.
This matters for tools that parse `--version` output.

### argv rewriting

The native `spur <command>` dispatch rewrites argv so each subcommand
sees itself as argv[0]. This is necessary because each command calls
`try_parse_from(args)` and expects `args[0]` to be its own name.
`SPUR_ARGV0_OVERRIDE` env is set during the rewrite.

### Config loading

`spur_config.rs` loads `/etc/spur/spur.conf` (or `$SPUR_CONF`). On failure
it returns a default config with `cluster_name = "spur"` rather than
erroring, so the CLI works without a config file (just needs
`--controller` or `SPUR_CONTROLLER_ADDR`).

### Controller address resolution

Priority: `--controller` flag > `SPUR_CONTROLLER_ADDR` env > `[controller] hosts`
from config file > `http://localhost:6817` default.

The config-file lookup happens once in `main()` via
`load_controller_addr_from_config()` and sets `SPUR_CONTROLLER_ADDR` so all
subcommands pick it up through clap's `env` attribute.

### Privilege checks

Reservation management requires root or membership in the `sudo`/`wheel` group.
This is checked client-side (`privilege.rs`) for a clear error, but enforced
server-side. The client check is not a security boundary.
