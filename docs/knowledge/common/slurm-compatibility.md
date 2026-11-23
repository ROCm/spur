# Slurm Compatibility Surface

Maintainer notes on the Slurm-compatible CLI, REST, and FFI surfaces: compat
hazards and deliberate deviations not obvious from the code. User-facing
differences and limitations live in `docs/migration-from-slurm.rst`; the
command map lives in `docs/user-guide/slurm-compatibility.rst`. Keep them there,
not here.

## CLI

- `--version` is split by invocation name: the Slurm symlinks matched in
  `main()` (`sbatch`, `srun`, `squeue`, ...) print `slurm_version_string()`;
  other symlinks and native `spur` print `version_string()`. Tools parsing
  `--version` see different strings depending on argv[0].
- `-h` is `--noheader` on `squeue`/`sinfo` (Slurm convention), so clap's auto
  help flag is disabled and `--help` is re-added long-only.
- `--noconvert` is accepted on `squeue`, `sinfo`, `sacct`, `sstat`, `sshare`,
  and `sreport` but is a no-op. `sacct -X`/`--allocations` is also a no-op
  (history has one row per job, no step rows).
- `sacct -a`/`--allusers` is accepted. The default already shows every job the
  caller may read, so `-a` only matters against `-u`; the last of `-a`/`-u`
  wins, as in Slurm.
- `--gres` is last-wins, not cumulative (`overrides_with`), matching Slurm:
  commas within one value expand, a second `--gres` replaces the first.
- Only one GPU request form may be set: `--gpus`, `--gpus-per-node`,
  `--gpus-per-task`, or a `gpu:` entry in `--gres` (`validate_gpu_flags`,
  checked client-side).
- Exit codes match Slurm: completed propagates the job's code, failed is
  `max(code, 1)`, cancelled/timeout/node-fail/deadline are 1
  (`terminal_exit_code` in `sbatch`).

## Environment

- Precedence. `sbatch`: CLI > env (`SPUR_*` then `SBATCH_*`) > `#SBATCH`
  directives > defaults. `srun`: CLI > env (`SPUR_*` then `SLURM_*`) >
  defaults. In `sbatch`, env is layered after the directive merge
  (`resolve_sbatch_env` after `merge_resolved`), which is what puts it above
  directives. Crate-specific exclusions are in `crates/spur-cli/AGENTS.md`.
- Job environment twins: `SpurEnv::set_with_slurm_twin` adds a `SLURM_*` copy,
  but variables set with plain `SpurEnv::set` have none (e.g.
  `SPUR_TASK_OFFSET`, `SPUR_JOB_GPUS`, `SPUR_NODE_RANK`, `SPUR_PEER_NODES`,
  `SPUR_TARGET_NODE`, `SPUR_BURST_BUFFER`). Not every `SPUR_*` variable has a
  `SLURM_*` twin.
- Credential variables are stripped from the job environment
  (`SpurEnv::into_map`).

## Output semantics

- `scontrol show job` `ReqTRES` omits Slurm's `billing=` component
  (`requested_tres`), even though `[scheduler] tres_billing_weights` exists for
  fair-share.
- Fair-share factor is capped at 10 (1.0 = exactly fair), not Slurm's 0-1
  (0.5 = exactly fair); user ranking is the same. The cap is duplicated across
  crates; see `docs/knowledge/scheduler/scheduling.md` ("Pass structure and
  ordering").

## REST API

Served in-process by `spurctld` (no `slurmrestd`) and exposes only a subset of
`slurmrestd` (jobs, nodes, partitions). `rest_auth` applies the same
`BearerAuth` policy as gRPC: Permissive mode lets anonymous callers through.
REST-specific invariants are in `crates/spurctld/AGENTS.md`.

## FFI (`spur-ffi`)

- `libspur_compat.so` exports `extern "C"` functions named after a subset of
  libslurm's API, forwarding each call over gRPC to `spurctld`. Only that subset
  is implemented; the rest of libslurm is absent.
- Not ABI-compatible with libslurm: function names match, struct layouts are
  Spur-specific. Pitfalls and memory ownership are in `crates/spur-ffi/AGENTS.md`.
- `slurm_init_job_desc_msg` does not zero the descriptor; it applies
  `SlurmJobDescMsg::default()` (`min_nodes`/`max_nodes` 1, `cpus_per_task` 1,
  `num_tasks = NO_VAL` so the controller defaults to one task per node).
- Controller address comes from `SLURM_CONTROLLER_ADDR` (not
  `SPUR_CONTROLLER_ADDR`), default `http://localhost:6817`.
- To use it in place of libslurm, symlink `libslurm.so` to `libspur_compat.so`
  or relink the consumer; `LD_LIBRARY_PATH` alone does not make `-lslurm`
  resolve to the compat library.
