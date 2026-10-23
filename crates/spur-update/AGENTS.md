# spur-update

Self-update mechanism for Spur binaries. Checks GitHub releases, downloads
verified tarballs, and atomically replaces binaries on disk.

## Modules

| File | Purpose |
|---|---|
| `lib.rs` | Public API: `spawn_startup_check`, `do_self_update`, `self_update_cli` |
| `check.rs` | GitHub releases API client, semver comparison |
| `download.rs` | Tarball download + SHA-256 verification |
| `install.rs` | Extract tarball, atomic binary replacement with rollback |
| `cache.rs` | Disk-backed update-check cache (1h TTL) |
| `k0s.rs` | Download + install pinned k0s binary from GitHub |

## Update protocol

Three phases, all driven from `lib.rs`:

1. **Check** (`check.rs`): GET GitHub releases API (`/releases/latest` for
   stable, `/releases/tags/nightly` for nightly). Parses `tag_name` as semver.
   Stable: update available when remote tag > current version. Nightly: always
   considered an update . 10s HTTP timeout.

2. **Download** (`download.rs`): Fetch the `.tar.gz` asset from the release.
   If a `.tar.gz.sha256` sidecar asset exists, download it and verify the
   tarball hash. Checksum format: `<hex>  <filename>` or bare hex. Mismatched
   hash deletes the tarball and bails. 300s timeout. No checksum asset = skip
   verification (debug log only).

3. **Install** (`install.rs`): Three-phase atomic replacement:
   - Backup: rename each existing binary to `<name>.spur-old`
   - Copy: extract tarball, copy new binaries from `<root>/bin/`, chmod 0755
   - Cleanup: remove `.spur-old` files on success
   - Rollback: on any copy failure, rename `.spur-old` back and bail

## Binaries managed

Defined in `SPUR_BINARIES`: `spur`, `spurctld`, `spurd`, `spurstepd`, `spurauthd`.

Tarball layout: `spur-{version}-linux-amd64/bin/{binaries}`. The installer
searches one level deep for a `bin/` directory.

Install directory auto-detected from `std::env::current_exe()` parent.

## Entry points

- `spawn_startup_check()` -- non-blocking tokio task for daemon startup.
  Checks cache first; fetches GitHub only when cache is stale/missing.
  If `auto_update` is true, downloads and replaces binaries (does NOT
  restart daemons).

- `self_update_cli()` -- blocking CLI flow (`spur self-update`). Check +
  download + replace, with user-facing println output.

- `do_self_update()` -- shared download+install logic used by both paths.

## Cache management

File: `<cache_dir>/update-check.json`. JSON with `checked_at`, `current_version`,
`latest_tag`, `update_available`, `channel`.

TTL: 1 hour (`CACHE_TTL_HOURS`). Compared against `checked_at` timestamp.
Stale or missing cache triggers a GitHub API call. Write failures are silently
ignored (non-critical path).

## k0s handling

`k0s.rs` manages a separate download flow for the k0s Kubernetes binary,
independent of the Spur self-update pipeline.

- Source: `k0sproject/k0s` GitHub releases. Pinned version lives in
  `spur_core::k0s::K0S_PINNED_VERSION`, passed by caller.
- Asset naming: `k0s-{version}-{arch}` where arch is `amd64` or `arm64`.
- Verification: downloads `sha256sums.txt` manifest first (fails fast on
  bad version before the ~260MB binary fetch). Parses both standard
  (`<hex>  <name>`) and binary-mode (`<hex> *<name>`) checksum lines.
- Install: write to temp sibling on same filesystem, chmod 0755, atomic
  rename over destination. No rollback (fresh install, not replacement).
- `ensure_k0s()` is the main entry: no-op if binary already exists at dest,
  otherwise downloads. 600s timeout.
- Cosign `.sig` signatures published by k0s are NOT verified.

## Channels

Two channels parsed via `FromStr` on `Channel`:
- `stable` (default, fallback for any unrecognized string)
- `nightly`

## Dependencies

reqwest (HTTP), semver (version comparison), sha2 (SHA-256), flate2+tar
(tarball extraction), chrono (timestamps/TTL), serde_json (cache serialization).
