# AGENTS.md -- spur-mpi-pmix

PMIx MPI plugin for Spur. Built as a shared library (`spur_mpi_pmix.so`)
loaded at runtime when a job launches with `--mpi=pmix`.

## What this crate is

A C-only shared library compiled and linked via `build.rs`. The Rust side
(`lib.rs`) exists solely to drive the C test binary in `cargo test`. All
real logic lives in `c/` and `include/`.

When `pkg-config` finds `libpmix`, `build.rs` compiles the real PMIx server
(`pmix_server.c` + `modex_exchange.c`) and links against the system PMIx.
When libpmix is absent, it compiles `stub_server.c` -- a no-op that returns
errors telling the caller to install libpmix development packages.

## File layout

```
include/spur_mpi_plugin.h   Plugin ABI (spur_mpi_launch_plan_t, version, entry points)
c/pmix_server.c              PMIx server: session management, namespace registration,
                             fence handling, fork-env setup
c/modex_exchange.c           TCP modex exchange: peer-to-peer blob relay for multi-node
c/modex_exchange.h           Modex session API, wire constants, port derivation
c/modex_exchange_test.c      C test suite for modex (compiled + run from Rust test harness)
c/stub_server.c              Stub when libpmix is not available
build.rs                     cc + pkg-config build; links .so, stages it for release
src/lib.rs                   Rust test wrapper only
Cargo.toml                   Workspace crate; build-deps: cc, pkg-config
```

## Plugin ABI

Defined in `spur_mpi_plugin.h`. Current version: `SPUR_MPI_PLUGIN_API_VERSION = 4`.

Entry points exported from the `.so`:

| Symbol | Purpose |
|---|---|
| `spur_mpi_pmix_version` | Returns plugin API version |
| `spur_mpi_pmix_runtime_version` | Writes the linked PMIx library version string |
| `spur_mpi_pmix_server_start` | Registers a PMIx namespace + clients, starts modex listener |
| `spur_mpi_pmix_server_stop` | Tears down namespace, aborts in-flight fences |
| `spur_mpi_pmix_verify_peers` | TCP-connects to every peer node to confirm reachability |
| `spur_mpi_pmix_setup_fork_env` | Calls `PMIx_server_setup_fork` and returns `KEY=VALUE` env |
| `spur_mpi_pmix_setup_fork_env_free` | Frees the env array from `setup_fork_env` |

The caller passes a `spur_mpi_launch_plan_t` containing job/step IDs,
namespace, universe size, local process ranks, peer hostnames, tmpdir,
credentials, and modex timeout overrides.

## PMIx server lifecycle

1. `server_start` calls `PMIx_server_init` once (kept alive across jobs).
2. Registers the namespace via `PMIx_server_register_nspace` with topology
   info (node map, proc map, local peers, universe size). PMIx 5 vs 6 have
   different register signatures -- both are handled via `#if PMIX_VERSION_MAJOR`.
3. Registers each local client via `PMIx_server_register_client`.
4. For multi-node jobs (`num_nodes > 1`), creates a modex session (TCP
   listener) for cross-node fence data exchange.
5. `server_stop` deregisters namespace/clients and tears down the modex session.

The server module registers three callbacks: `client_connected`,
`client_finalized`, and `fence_nb`. The fence callback is always registered
even for single-node jobs to avoid an OpenPMIx 5.x reinit bug.

## Session management

Up to 64 concurrent sessions (`SPUR_PMIX_MAX_SESSIONS`), each up to 256
local processes (`SPUR_PMIX_MAX_LOCAL_PROCS`). Sessions are keyed by
namespace string. A global mutex (`g_session_lock`) serializes session
lookup, allocation, and fence state transitions.

Duplicate `server_start` calls with the same namespace are idempotent if
the universe size and local rank set match; mismatches return an error.

## Fence / modex exchange

When all local ranks arrive at `fence_nb`, the plugin:

1. Concatenates local modex blobs into one buffer.
2. Drops the session lock and calls `spur_modex_fence_collect` (blocking TCP).
3. Re-acquires the lock and delivers the merged result to each rank's callback.

OpenPMIx 5 sends a single `PMIX_RANK_WILDCARD` fence per node instead of
per-rank arrivals. The plugin detects this and short-circuits the local
arrival count.

Merged data is shared across callbacks via a refcounted `spur_modex_shared_t`
to avoid copying per rank.

## TCP modex protocol

Wire format: 32-byte packed header (`spur_modex_hdr_t`) + payload.

```
magic(4) | version(4) | job_id(4) | step_id(4) | node_index(4) | fence_seq(4) | data_len(4) | flags(4)
```

Protocol version is 3 (bumped from 2 when `step_id` was added). A v2 peer
connecting to a v3 listener is rejected on version mismatch. The MAGIC is
`0x53505552` ("SPUR").

Each node listens on a deterministic port derived from `(job_id, step_id)`:
```
port = 16819 + ((job_id * 2654435761 + step_id) % 8000)
```
This derivation is duplicated in Rust (`spur_core::mpi::modex_port_for_step`)
and must stay in sync -- the C test suite pins known vectors against it.

Port collisions between unrelated jobs are handled: every frame is gated on
`(job_id, step_id)` identity before processing. Foreign frames are silently
dropped. Only an abort frame matching the session's own identity triggers
session abort.

## Modex session internals

- `spur_modex_session_create`: allocates session, sets peer hosts and timeouts.
- `spur_modex_session_start`: binds the listen port, spawns an accept thread.
- `spur_modex_fence_collect`: pushes local blob to all peers, waits for all
  remote blobs, merges them in node-index order.
- `spur_modex_verify_peers`: TCP-connects to each peer to confirm liveness
  before the first fence.
- `spur_modex_session_abort`: sets abort flag, broadcasts to all peers.
- Refcounted (`retain`/`release`) so fence_nb can hold a reference while the
  session lock is dropped during blocking TCP I/O.

Default timeouts: connect 5s, fence 120s, verify 30s. Overridable via
`spur_mpi_launch_plan_t`.

## Process mapping

For PMIx < 6, the plugin builds topology info manually:
- Node map: `PMIx_generate_regex` from comma-separated hostlist.
- Proc map: `PMIx_generate_ppn` from semicolon-delimited rank lists per node.
- Local peers: comma-separated rank list for the local node.
- Local leader: lowest-ranked local process.

Assumes uniform task distribution (`universe_size / num_nodes` must divide
evenly). Non-uniform layouts return `PMIX_ERR_NOT_SUPPORTED`.

PMIx >= 6 passes proc structs directly to `PMIx_server_register_nspace`.

## Testing

`cargo test` compiles and runs `modex_exchange_test.c` (pure C, no PMIx
dependency). Tests cover:
- Listener lifecycle (start/stop/restart, refcount leak detection)
- Port derivation pinned against Rust vectors
- Wire-level frame gating (foreign job, foreign step, own abort, own data)
- v2-to-v3 protocol version rejection
- Port collision isolation between concurrent sessions

## Debug

Set `SPUR_MPI_DEBUG=1` to enable stderr tracing of fence arrivals, modex
progress, namespace registration, and setup_fork calls.
