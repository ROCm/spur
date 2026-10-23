# Slurm Compatibility Surface

Spur is a drop-in replacement for Slurm. This document covers the exact
compatibility surface: what works unchanged, what differs, what is deliberately
not supported, and how the FFI and REST layers map.

## CLI Compatibility

### Multi-Call Binary

`spur-cli` compiles to a single `spur` binary that dispatches based on
`argv[0]`. Symlinks named after Slurm commands work out of the box:

| Symlink | Spur Native | Description |
|---------|-------------|-------------|
| `sbatch` | `spur submit` | Batch job submission |
| `srun` | `spur run` | Interactive/parallel execution |
| `salloc` | `spur alloc` | Interactive resource allocation |
| `squeue` | `spur queue` | Job queue viewer |
| `scancel` | `spur cancel` | Cancel jobs |
| `sinfo` | `spur nodes` | Node/partition information |
| `sacct` | `spur history` | Job accounting history |
| `sacctmgr` | `spur accounts` | Account/user/QOS management |
| `scontrol` | `spur show` | Admin show/update/create/delete |
| `sprio` | `spur priority` | Job priority breakdown |
| `sshare` | `spur share` | Fair-share information |
| `sstat` | `spur stat` | Running job statistics |
| `sdiag` | `spur diag` | Scheduler diagnostics |
| `sreport` | `spur report` | Usage reports |
| `strigger` | `spur trigger` | Event triggers |
| `sattach` | `spur attach` | Attach to job I/O |
| `scrontab` | `spur crontab` | Recurring cron-style jobs |
| `smd` | `spur health` | Node health monitoring |

Ten symlink names (`srun`, `sbatch`, `sinfo`, `squeue`, `scancel`, `salloc`,
`scontrol`, `sacct`, `sacctmgr`, `sshare`) report `slurm_version_string()` from
`--version`; the remaining symlinks and native `spur` binary report
`version_string()`. Tools parsing `--version` output should be aware of this.

### What Works Unchanged

- **Job script directives.** `#SBATCH` lines are parsed identically. `#PBS`
  directives are auto-converted on a best-effort basis.
- **Resource flags.** `-N`, `-n`, `-c`, `--gres`, `--gpus`, `--gpus-per-node`,
  `--gpus-per-task`, `--mem`, `--mem-per-cpu`, `--time`, `--constraint`,
  `--container-image`, etc.
- **Environment variables.** Spur sets a `SLURM_*` twin for every `SPUR_*`
  variable injected into jobs, so MPI launchers and training frameworks see the
  `SLURM_*` names they expect.
- **Format strings.** `squeue -o "%.18i %.9P"`, `sinfo`, `sacct` all support
  Slurm's `%`-specifier syntax: `%[#][.][width]<letter>`.
- **Env precedence.** `sbatch`: CLI > env (`SPUR_*` then `SBATCH_*`) >
  `#SBATCH` directives > defaults. `srun`: CLI > env (`SPUR_*` then `SLURM_*`)
  > defaults.
- **`sacctmgr`** full CRUD: `add`/`modify`/`remove`/`show`/`list`/`dump`/`load`
  for accounts, users, QOS, clusters, associations.
- **Exit codes.** completed=propagated, failed=max(code,1),
  cancelled/timeout/nodefail/deadline=1. Matches Slurm.

### Behavioral Differences

**`-h` is `--noheader`** in squeue/sinfo (Slurm convention), not `--help`.
`--help` is re-added as a long-only flag.

**`--noconvert`** is accepted on squeue, sinfo, sacct, sstat, sshare, and
sreport but is a no-op. Spur always reports raw values.

**`--gres` is last-wins** (`overrides_with = "self"`), not cumulative. Comma-
delimited values within one `--gres=gpu:2,fpga:1` expand, but a second `--gres`
replaces the first. Matches Slurm.

**Partitions** can be defined in `spur.conf` and also managed at runtime via
`scontrol create/update/delete partition`. Runtime changes are persisted
through Raft and survive restarts.

**scontrol show** output is always multi-line `Key=Value`; no `--oneliner`.
`NodeList=(null)` on pending jobs (not omitted). `ReqTRES` omits Slurm's
`billing=` component. `StartTime` shows projected start for pending jobs (check
`JobState` instead of `StartTime != N/A`).

**sshare/sreport usage units** are cpu-seconds (Slurm uses cpu-minutes in some
contexts). `sshare -l` includes a `GPURawUsage` column (gpu-seconds). Fair-share
factor ranges 0-10 (1.0 = exactly fair), not Slurm's 0-1 (0.5 = exactly fair).

**QOS GrpWall** is enforced at scheduling admission only: over-budget QOS stops
admitting, but running jobs are never killed. Consumption is a trailing window,
not decayed. Cap exists on QOS only, not on associations.

**sacctmgr -p/-P** delimited output covers `show account`, `show qos`, and
`show txn`. For `show user`, `show association`, and `show tres` it errors
instead of printing unparseable output.

**scancel arrays.** Array-element syntax (`123_4`) is not yet supported
client-side; cancel by plain job ID.

**sacct --jobs** filters are applied server-side: `GetJobHistoryRequest.job_ids`
is forwarded to the accounting DB as a `WHERE job_id IN (...)` predicate.

**scontrol show assoc_mgr** uses a simplified layout. Filters to one user
(`users=<name>`); Slurm's `accounts=`, `qos=`, and `flags=` selectors are not
accepted.

### Flags Accepted for Compatibility (No-Op)

- `--noconvert` on squeue, sinfo, sacct, sstat, sshare, sreport
- `--container-readonly` (rejected at submission, not silently ignored)

## REST API Parity

The REST API is served directly by the controller (`spurctld`) on port 6820
(configurable via `controller.rest_addr`). It is disabled by default
(`rest_api.enabled = false`); enable it in `spur.conf`. When enabled, requests
are authenticated via the `[auth]` config section (JWT bearer). There is no separate
`slurmrestd` daemon. The API surface matches Slurm's REST endpoints for job
submission, job query, node listing, and partition listing.

## FFI / libslurm ABI (spur-ffi)

`spur-ffi` builds a `cdylib` shared library (`libspur_compat.so`) that exports
`extern "C"` functions matching a subset of Slurm's libslurm ABI. Each exported
function translates the C call into a gRPC request to the controller via
`spur-client`, blocking on a lazily-initialized tokio runtime.

### Exported Symbols

**Job management:**

- `slurm_init_job_desc_msg(desc)` -- zero a `SlurmJobDescMsg`
- `slurm_submit_batch_job(desc, job_id)` -- submit via `SubmitJob` RPC
- `slurm_kill_job(job_id, signal, flags)` -- cancel via `CancelJob` RPC

**Query:**

- `slurm_load_jobs(update_time, msg, flags)` -- list jobs via `GetJobs`
- `slurm_load_node(update_time, msg, flags)` -- list nodes via `GetNodes`
- `slurm_load_partitions(update_time, msg, flags)` -- list partitions

**Memory / error handling:**

- `slurm_free_job_info_msg(msg)` -- free a job info message
- `slurm_strerror(errnum)` / `slurm_get_errno()` / `slurm_seterrno()` /
  `slurm_perror(msg)` -- global mutex errno, static error table

### Type Mappings

All C-facing structs are `#[repr(C)]` with Vec-backed storage to keep Rust
allocations alive while C pointers are in use. `NO_VAL` (`u32::MAX`) is Slurm's
"field not set" sentinel; `num_tasks = NO_VAL` maps to proto 0.

### Controller Address

Read from `SLURM_CONTROLLER_ADDR` env var, default `http://localhost:6817`.

### Limitations

Only 7 core functions + error handling are implemented (MVP surface). The FFI
is not a security boundary. Callers must pass non-null, properly aligned,
validly allocated pointers. Null checks are present but cannot detect dangling
or misaligned pointers. Signatures omit `unsafe` to match Slurm's C calling
convention.

## Configuration Model

Slurm's `slurm.conf` + `slurmdbd.conf` are replaced by a single TOML file,
`spur.conf` (default `/etc/spur/spur.conf`).

- No `slurmdbd` daemon: accounting is served by the controller under
  `[accounting]`.
- No `slurmrestd` daemon: REST API is built into the controller.
- State is Raft-replicated, not file-based (`StateSaveLocation` does not exist).
- HA is always on via Raft (openraft); no backup-controller configuration.

### Accounting Config Mapping

| Slurm | Spur |
|-------|------|
| `AccountingStorageEnforce=qos` | `[accounting] require_qos = true` |
| `AccountingStorageEnforce=associations` | `[accounting] require_association = true` |
| `slurm.conf PrivateData` | `[auth] private_data = ["jobs", "usage"]` |
| `slurmdbd.conf DbdAddr` | `[accounting] database_url` |

Unlike Slurm, root has no implicit admin exemption. Grant it a role explicitly.

## What Is Deliberately NOT Compatible

- **`slurm.conf` format.** Spur uses TOML, not the Slurm key-value format.
- **`sstat` per-process metrics.** `Ave*`/`Max*` fields show `N/A`.
- **TRES billing weights.** `ReqTRES` omits `billing=`.
- **Fair-share scale.** 0-10 (Spur) vs 0-1 (Slurm); user ranking is identical.
- **Multiple config files.** No `slurmdbd.conf`, `cgroup.conf`, `topology.conf`
  as separate files.

## Migration Path

1. Install Spur (`spurctld` + `spurd`) and write `spur.conf` from the existing
   `slurm.conf` values (see the configuration reference).
2. Create Slurm-named symlinks for the CLI (`sbatch` -> `spur`, etc.).
3. Set `SPUR_CONTROLLER_ADDR` or configure `[controller] hosts` in `spur.conf`.
4. Test the golden path: `sbatch` a job, `squeue`, `scancel`, `sacct`.
5. Accounting: `sacctmgr add account`, `sacctmgr add user` work as in Slurm.
6. For FFI consumers: point `LD_LIBRARY_PATH` at `libspur_compat.so`.

Spur-only commands (`spur net`, `spur k8s`, `spur image`, `spur exec`,
`spur token`, `spur auth-keys`, `spur self-update`) have no Slurm equivalents.

## Key Source Files

| File | Purpose |
|------|---------|
| `spur-cli/src/main.rs` | Multi-call dispatch, argv[0] routing |
| `spur-cli/src/sbatch.rs` | sbatch implementation |
| `spur-cli/src/squeue.rs` | squeue implementation |
| `spur-cli/src/sacctmgr.rs` | sacctmgr implementation |
| `spur-cli/src/format_engine.rs` | Slurm %-format string parser |
| `spur-cli/src/env_defaults.rs` | SPUR_*/SBATCH_*/SLURM_* env resolution |
| `spur-ffi/src/lib.rs` | FFI exported functions |
| `spur-ffi/src/types.rs` | `#[repr(C)]` struct definitions |
| `docs/migration-from-slurm.rst` | User-facing migration guide |
