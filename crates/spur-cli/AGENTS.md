# AGENTS.md -- spur-cli

Single `spur` multi-call binary dispatching on argv[0], so Slurm-named
symlinks (`sbatch`, `squeue`, ...) and native `spur <command>` share one parser
and one gRPC path. The `--version` split and Slurm behavioral differences
(`-h` is `--noheader`, `--noconvert` no-op, `--gres` last-wins, exit codes)
live in `docs/knowledge/common/slurm-compatibility.md`; the symlink mapping is
in `docs/user-guide/slurm-compatibility.rst`.
Auth plugin mechanics live in `docs/knowledge/security/auth-model.md`.

## Invariants

- Native dispatch rewrites argv so each subcommand sees its own name as
  `args[0]` (each calls `try_parse_from(args)`), and sets `SPUR_ARGV0_OVERRIDE`.
  `spur show ...` injects the implicit `show` so it maps to `scontrol show`.
- `load_controller_addr_from_config()` runs once in `main()` and exports
  `SPUR_CONTROLLER_ADDR`, so subcommands only see the config-file address
  through clap's `env` attribute. Priority: `--controller` > env >
  `[controller] hosts` > localhost default.
- `spur_config.rs` falls back to a default config (`cluster_name = "spur"`)
  instead of erroring, so the CLI works with no config file.
- `-c` is sent as 0 when unset so a partition's or `[scheduler]`'s
  `def_cpu_per_gpu` can size the job.
- `resource_opts.rs` implements Slurm's option-source rule for
  `--cpus-per-gpu` vs `-c` and the `--mem*` forms: two explicit sources (or two
  env sources) is an error, explicit beats env silently. Per-GPU options
  without a GPU request are rejected client-side.
- Env-backed flags must go through the `env_defaults.rs` `apply_*` helpers so
  `was_cli_set()` keeps CLI values winning.

## Environment variable precedence

General CLI > env > defaults ordering is in
`docs/knowledge/common/slurm-compatibility.md`. Crate-specific rules:

- Output-twin exclusions: sbatch never reads `SPUR_JOB_NAME` or
  `SPUR_NODELIST`, and sbatch/salloc read only `SBATCH_*`/`SALLOC_*` for
  `--cpus-per-gpu`/`--mem-per-gpu`, never the `SPUR_*_PER_GPU` twins. These are
  injected into running jobs; reading them would make a nested submit inherit
  the parent's placement or sizing.
- srun reads `SLURM_*` because those are inherited from the allocation. `--export`:
  `SPUR_EXPORT_ENV` > `SRUN_EXPORT_ENV` > `SLURM_EXPORT_ENV`.
- **salloc**: same pattern as srun, with `SALLOC_*` variants.

## Pitfalls

- srun step mode: a `ResourceExhausted` from the agent (step share does not fit
  beside other typed steps) is retried via `BusyStepWait` with Slurm's "step
  creation temporarily disabled" message, not surfaced as failure. Ctrl-C
  during the wait ends the step `CANCELLED` instead of launching it.
- `--container-readonly` is rejected at submission because the runtime does
  not enforce it; accepting it would give a false sense of isolation.
- Older-controller compat: `scontrol show node` omits
  `Sockets=/CoresPerSocket=/ThreadsPerCore=` (`node_cpu_line`), GPU
  `ReservedCPUs=/AffinityCPUs=`, and `Capabilities=` when the controller sent
  nothing, never printing `0` or empty. `sinfo %z` renders `N/A` for missing
  topology and for grouped rows that disagree.
- Each format-driven command keeps a `header_map` and a `resolve_field`; a
  letter/name registered in one needs the other (tests assert every header
  has a render arm).

## Security

- JWT token file (`~/.spur/token`) with any group/other permission bit is skipped with a
  warning, and the CLI continues without a credential.
- `privilege.rs` (root or `sudo`/`wheel` for reservations) is a UX check only;
  enforcement is server-side.
- Without a credential (auth disabled or permissive), the server trusts the
  client-supplied user, so `user="root"` passes reservation checks.
  Reservation authz is a security boundary only under `required` mode.

## Testing

- Tests mutating `SPUR_*`/`SBATCH_*`/`SLURM_*` env must use
  `#[serial(env_injection)]` plus `EnvGuard`. A test that only reads them
  through `env_defaults` cannot see another test's values: in test builds each
  read takes `ENV_LOCK`, which an `EnvGuard` holds for its whole test.
- Run `spur-cli`'s suite on its own, not combined with other crates in one
  `cargo test`: several srun PTY tests hang alongside other crates' binaries.
