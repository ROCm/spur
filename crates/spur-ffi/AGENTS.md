# spur-ffi

C FFI compatibility shim (`libslurm-compat.so`). Exports `extern "C"` functions
matching a subset of Slurm's libslurm ABI so existing tools that `dlopen` or
link against libslurm can use Spur as a drop-in backend.

## Overview

The crate builds a `cdylib` shared library. Each exported function translates
the C call into a gRPC request to the Spur controller via `spur-client` and
`spur-proto`, blocking on a lazily-initialized tokio runtime.

## Source layout

| File | Purpose |
|------|---------|
| `lib.rs` | Exported `extern "C"` functions and helpers |
| `types.rs` | `#[repr(C)]` struct definitions matching Slurm's ABI layout |

## Exported symbols

### Job management
- `slurm_init_job_desc_msg(desc)` -- zero a `SlurmJobDescMsg` to defaults
- `slurm_submit_batch_job(desc, job_id)` -- submit via `SlurmController::SubmitJob`
- `slurm_kill_job(job_id, signal, flags)` -- cancel via `SlurmController::CancelJob`

### Query
- `slurm_load_jobs(update_time, msg, flags)` -- list jobs via `GetJobs`
- `slurm_load_node(update_time, msg, flags)` -- list nodes via `GetNodes`
- `slurm_load_partitions(update_time, msg, flags)` -- list partitions via `GetPartitions`

### Memory management
- `slurm_free_job_info_msg(msg)` -- free a job info message (frees inner CStrings)

### Error handling
- `slurm_strerror(errnum)` -- static error string table
- `slurm_get_errno()` / `slurm_seterrno(errnum)` -- global mutex errno
- `slurm_perror(msg)` -- print error to stderr

## Slurm library compatibility

- Controller address: `SLURM_CONTROLLER_ADDR` env var, default `http://localhost:6817`.
- `NO_VAL` (u32::MAX): Slurm's "field not set" sentinel. `num_tasks = NO_VAL`
  maps to proto 0, letting the controller default to one task per node.
- Return convention: 0 = success, -1 = error (matching Slurm).
- Only a subset of libslurm is implemented (MVP: 6 core functions + error handling).

## Type mappings (types.rs)

| C type | Rust type | Notes |
|--------|-----------|-------|
| `SlurmJobDescMsg` | `#[repr(C)]` struct | Job submission descriptor |
| `SlurmJobInfo` | `#[repr(C)]` struct | Job info record |
| `SlurmJobInfoMsg` | struct | Container with `_job_storage: Vec` to keep Rust Vec alive |
| `SlurmNodeInfo` | `#[repr(C)]` struct | Node info record |
| `SlurmNodeInfoMsg` | struct | Container with `_node_storage: Vec` |
| `SlurmPartInfo` | `#[repr(C)]` struct | Partition info record |
| `SlurmPartInfoMsg` | struct | Container with `_part_storage: Vec` |

## Safety concerns

- All `extern "C"` functions check for null pointers before dereferencing.
- `#[allow(clippy::not_unsafe_ptr_arg_deref)]` is used throughout. Callers
  must pass non-null, properly aligned, validly allocated pointers with
  sufficient storage. The functions check for null but cannot detect dangling,
  misaligned, or undersized pointers. Signatures omit `unsafe` to match
  Slurm's C calling convention (not a safety guarantee).
- CString ownership: `string_to_c_str` transfers ownership to the caller via
  `CString::into_raw()`; the matching `free_c_str` reclaims it via
  `CString::from_raw()`.
- The tokio runtime is lazily initialized via `OnceLock` and shared across all
  FFI calls.
- The `_job_storage` / `_node_storage` Vec fields keep the Rust-owned data alive
  while the C-visible raw pointer (`job_array`, etc.) is in use.

## Dependencies

- `spur-proto` -- gRPC service clients
- `spur-client` -- channel connection
- `tokio` -- async runtime (blocked on from C)
- `prost-types` -- protobuf Duration
