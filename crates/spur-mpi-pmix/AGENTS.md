# AGENTS.md -- spur-mpi-pmix

C-only plugin (`spur_mpi_pmix.so`, ABI in `include/spur_mpi_plugin.h`) loaded at
runtime for `--mpi=pmix`. All logic lives in `c/`; `src/lib.rs` only drives the C
test binary from `cargo test`.

## Build

- `build.rs` links the real server only when `pkg-config` finds `pmix`. Otherwise it
  silently builds `c/stub_server.c`, whose entry points fail with "install libpmix".
  A build without libpmix succeeds but produces a plugin that cannot launch jobs.
- PMIx 5 and 6 have different callback and `PMIx_server_register_nspace` signatures.
  Any PMIx call you touch needs both `#if PMIX_VERSION_MAJOR` branches.

## Invariants

- `PMIx_server_init` runs once and stays alive across jobs. `fence_nb` is always
  registered, even for single-node jobs: toggling it via finalize/reinit breaks
  `PMIx_server_setup_fork` on OpenPMIx 5.x.
- `g_session_lock` guards session lookup, allocation, and fence state. `spur_fence_nb`
  must drop it before the blocking `spur_modex_fence_collect`, holding a
  `spur_modex_session_retain` reference so the modex session outlives the unlock.
- Duplicate `server_start` for the same namespace is idempotent only if universe size
  and local rank set match; a mismatch is an error.
- PMIx < 6 path builds node/proc maps itself and assumes uniform distribution
  (`universe_size / num_nodes` exact); non-uniform layouts return `PMIX_ERR_NOT_SUPPORTED`.

## Pitfalls

- OpenPMIx 5 sends one `PMIX_RANK_WILDCARD` fence per node carrying all local modex,
  not per-rank arrivals. `spur_fence_nb` short-circuits the arrival count for it;
  explicit-rank callers still wait for every local rank.
- The modex listen port is a hash of `(job_id, step_id)` into a fixed span, so
  unrelated sessions collide on ports. `frame_is_for_session` gates every frame
  before its flags; only an abort frame with the session's own identity aborts it.

## Coupling

- `spur_modex_port_for_step` (`c/modex_exchange.h`) mirrors
  `spur_core::mpi::modex_port_for_step` exactly. Change both; the C test suite pins
  the Rust test vectors.
- `SPUR_MODEX_VERSION` must be bumped on any `spur_modex_hdr_t` wire change; peers on
  a different version are rejected.

## Debug

- `SPUR_MPI_DEBUG=1` enables stderr tracing of fences, modex, nspace registration,
  and setup_fork.
