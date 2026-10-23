# AGENTS.md -- spur-spank

## Overview

SPANK plugin host for the Spur scheduler. Loads existing Slurm SPANK
plugins (`.so` shared objects) via `dlopen` and provides the `spank_*` /
`slurm_*` C callback API that plugins expect from `spank.h`. This lets
Spur run unmodified Slurm SPANK plugins without linking against Slurm
itself.

Single source file: `src/lib.rs`. Unix-only (non-unix stubs error out).

## Dependencies

- `libloading` -- runtime dlopen/dlsym of plugin .so files.
- `tracing` -- structured logging for hook invocations and plugin log
  forwarding.
- `anyhow` / `thiserror` -- error handling.

## Plugin interface

### SpankHost

Central type. Manages a list of loaded plugins.

- `load_plugin(path, args)` -- dlopen a .so, store it with its
  plugstack arguments.
- `invoke_hook(hook, handle)` -- call a hook across all loaded plugins
  in load order. Each plugin's hook receives `(handle_ptr, argc, argv)`.
  Missing hooks are silently skipped. Non-zero return stops iteration
  and returns `SpankError::HookFailed`.
- `plugin_count()` -- number of loaded plugins.

### SpankHandle

`#[repr(C)]` struct passed as the opaque `spank_t` handle to every
plugin callback. Contains:

- `context: SpankContext` -- job metadata (job_id, uid, gid, step_id,
  node counts, task_pid).
- `env: HashMap<String, String>` -- job environment, mutated by plugins
  via `spank_setenv`/`spank_getenv`/`spank_unsetenv`.
- `job_control_env: HashMap<String, String>` -- job-control env; keys
  auto-prefixed with `SPANK_` (matches Slurm convention).

The handle is caller-owned and threaded through all hooks, so env
changes from one plugin are visible to the next.

### SpankContext

Job metadata fields: `job_id`, `uid`, `gid`, `step_id`, `num_nodes`,
`node_id`, `local_task_count`, `total_task_count`, `task_pid`.
Implements `Default` (all zeros).

## Slurm SPANK compatibility

### Exported C symbols

These are `#[no_mangle] extern "C"` functions that plugin .so files
resolve at load time:

| Symbol                         | Slurm equivalent               |
|--------------------------------|--------------------------------|
| `spank_get_item`               | `spank_get_item`               |
| `spank_setenv`                 | `spank_setenv`                 |
| `spank_getenv`                 | `spank_getenv`                 |
| `spank_unsetenv`               | `spank_unsetenv`               |
| `spank_job_control_setenv`     | `spank_job_control_setenv`     |
| `spank_job_control_getenv`     | `spank_job_control_getenv`     |
| `spank_job_control_unsetenv`   | `spank_job_control_unsetenv`   |
| `spank_strerror`               | `spank_strerror`               |
| `slurm_error`                  | `slurm_error`                  |
| `slurm_info`                   | `slurm_info`                   |
| `slurm_verbose`                | `slurm_verbose`                |
| `slurm_debug` / `debug2` / `debug3` | `slurm_debug*`           |
| `slurm_spank_log`             | `slurm_spank_log`              |

### Varargs limitation

Slurm declares `spank_get_item` and `slurm_*` log functions as
C-variadic. Stable Rust cannot define C-variadic functions.
`spank_get_item` takes a single trailing pointer (ABI-compatible on
SysV x86-64 for scalar items). Log functions accept only the format
string; `%`-style specifiers are not expanded.

### SpankItem enum

`#[repr(C)]` with discriminants matching Slurm's `enum spank_item`.
Supported for read: JobUid(0), JobGid(1), JobId(2), JobStepId(3),
JobNnodes(4), JobNodeid(5), JobLocalTaskCount(6),
JobTotalTaskCount(7), TaskPid(14).
Returns `ESPANK_NOT_AVAIL`: JobNcpus, JobArgv, JobEnv, TaskId,
TaskGlobalId, TaskExitStatus. Unknown IDs return `ESPANK_BAD_ARG`.

### Error codes

Constants match `spank_err_t` from `spank.h`: `ESPANK_SUCCESS` (0)
through `ESPANK_NOT_LOCAL` (11).

## Hook points

Enum `SpankHook` -- each variant maps to the C symbol name plugins
export:

| Hook              | C symbol                          | When                        |
|-------------------|-----------------------------------|-----------------------------|
| Init              | `slurm_spank_init`                | Plugin load                 |
| InitPost          | `slurm_spank_init_post_opt`       | After option processing     |
| LocalUserInit     | `slurm_spank_local_user_init`     | Local (srun) user init      |
| UserInit          | `slurm_spank_user_init`           | Remote user init            |
| TaskInit          | `slurm_spank_task_init`           | Before task exec            |
| TaskInitPrivileged| `slurm_spank_task_init_privileged`| Before task exec (as root)  |
| TaskPost          | `slurm_spank_task_post_fork`      | After task fork             |
| TaskExit          | `slurm_spank_task_exit`           | Task exit                   |
| JobEpilog         | `slurm_spank_job_epilog`          | Job epilog                  |
| SlurmctldExit     | `slurm_spank_slurmd_exit`         | Daemon exit                 |
| Exit              | `slurm_spank_exit`                | Plugin unload               |

## Plugin lifecycle

1. Parse `plugstack.conf` via `parse_plugstack(path)`. Format:
   `required|optional <plugin.so> [args...]`. Comments (#) and blank
   lines are ignored.
2. Call `SpankHost::load_plugin()` for each entry. The .so is dlopened
   and its plugstack args are stored as `Vec<CString>`.
3. Create a `SpankHandle` seeded with `SpankContext` and the job's
   initial environment.
4. Invoke hooks in sequence (Init, InitPost, ..., Exit) via
   `invoke_hook()`. Each call iterates plugins in load order, looks up
   the hook symbol via `dlsym`, and calls it with `(handle, argc, argv)`.
   Plugins that don't export a given hook are silently skipped.
5. After all hooks, read back `handle.env` and `handle.job_control_env`
   for any environment modifications the plugins made.

Plugins stay loaded for the lifetime of `SpankHost` (dropped when the
host is dropped, which closes the `libloading::Library` handles).

## Testing

`cargo test -p spur-spank` runs unit tests covering:
- Item discriminant alignment with Slurm's enum values.
- `spank_get_item` field dispatch and error codes.
- `spank_setenv` / `spank_getenv` overwrite semantics and buffer
  handling.
- `spank_job_control_setenv` SPANK_ key prefixing.
- `plugstack.conf` parsing (valid input, missing file).
- Empty-host hook invocation (no-op success).
