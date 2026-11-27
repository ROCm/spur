# AGENTS.md -- spur-spank

Hosts unmodified Slurm SPANK plugins (`dlopen`) and exports the `spank_*` /
`slurm_*` C symbols they resolve, without linking against Slurm.

## Invariants

- `SpankItem` discriminants must match Slurm's `enum spank_item` in `spank.h`.
  Plugins pass the raw numbers, so never reorder or renumber variants.
- `ESPANK_*` constants must match `spank_err_t` values in `spank.h`.
- `SpankHost` owns the `libloading::Library` handles. Dropping the host unloads
  every plugin, so no plugin function pointer may outlive it.

## FFI / ABI pitfalls

- Slurm declares `spank_get_item` and the `slurm_*` log functions C-variadic;
  stable Rust cannot define those. `spank_get_item` takes a single trailing
  pointer, which is ABI-compatible only on SysV x86-64 for scalar items.
  Multi-argument items (`JobArgv`, `JobEnv`, ...) return `ESPANK_NOT_AVAIL`.
- The log functions take only the format string; `%` specifiers are not
  expanded.
- Callers fill only `job_id`, `uid`, and `gid` in `SpankContext`. The other
  fields stay 0 and `spank_get_item` returns them with `ESPANK_SUCCESS` (not
  `ESPANK_NOT_AVAIL`), so plugins silently read zeros.

## Coupling with spurd

`SpankHook` defines every Slurm hook, but only four are invoked today:

- `Init` and `TaskInit`: `spurd::executor`, before the workload spawns, skipped
  entirely when `pmix_multi_task` is set. The handle is seeded with the job env
  and `handle.env` is read back afterward; `job_control_env` is never read back.
- `TaskExit` and `JobEpilog`: `spurd::stepd`, only for the step that owns the
  job lifetime, with an empty env. Nothing is read back.
- All hooks run as root in the supervisor, never as the job user.
- A non-zero hook return stops the remaining plugins for that hook
  (`SpankError::HookFailed`); callers only warn and never abort the job.
- A `required` plugstack entry that fails to load also only warns.
