# AGENTS.md -- spur-ffi

C FFI shim (`libspur_compat.so`) exporting Slurm-named `extern "C"` functions
that forward to `spurctld` over gRPC. Surface, type mappings, and the
controller address are in `docs/knowledge/common/slurm-compatibility.md`.

## Pitfalls

- Not ABI-compatible with libslurm, despite the `types.rs` module doc
  ("matching Slurm's ABI layout"). Function names match; struct layouts are simplified and
  Spur-specific.
- The `*Msg` containers (`SlurmJobInfoMsg`, `SlurmNodeInfoMsg`,
  `SlurmPartInfoMsg`) lack `#[repr(C)]`; only the leaf record structs have it.
- Exported functions omit `unsafe` to match Slurm's C calling convention, not
  because they are safe (hence `#[allow(clippy::not_unsafe_ptr_arg_deref)]`).
  They null-check pointers but cannot detect dangling, misaligned, or
  undersized ones. Not a security boundary.
- `NO_VAL` (`u32::MAX`) means "field not set". `num_tasks = NO_VAL` must map to
  proto 0 so the controller defaults to one task per node.

## Memory ownership

- Strings handed to C come from `string_to_c_str` (`CString::into_raw`) and
  must be reclaimed by `free_c_str` (`CString::from_raw`).
- The `_job_storage` / `_node_storage` / `_part_storage` Vecs own the records
  that the C-visible `*_array` pointer aliases; they must stay alive while
  that pointer is in use.
- `slurm_free_job_info_msg` is the only free function.

## Conventions

- Return 0 on success, -1 on error (Slurm convention).
- All RPC-issuing calls block on one lazily initialized tokio runtime (`runtime()`,
  `OnceLock`) shared across FFI calls.
