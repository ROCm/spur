# AGENTS.md -- spur-update

Self-update for Spur binaries from GitHub releases, plus the separate k0s binary installer
(`k0s.rs`). User-facing behavior is in `docs/knowledge/deployment/operations.md` ("Single-host self-update").

## Pitfalls

- `install::atomic_replace` is NOT atomic, despite its name and doc comments (and the crate docs).
  It renames each binary to `<name>.spur-old`, then `fs::copy`s the new one in place. On a copy
  error it renames each `.spur-old` back (errors ignored) and bails; a failed backup rename aborts
  with no restore, and a crash mid-copy can leave a truncated binary.
- A binary listed in `SPUR_BINARIES` but absent from the tarball is skipped on copy, yet its
  `.spur-old` backup is still deleted in cleanup, so the binary vanishes.
- SHA-256 verification of the Spur tarball is optional: it only runs when the release has a
  `.tar.gz.sha256` sidecar asset. Without one, verification is skipped with a debug log only.
- `Channel::from_str` never fails: any string other than `nightly` silently means stable.
- The nightly channel always reports an update available, regardless of version.
- The installer and startup auto-update do not restart daemons.

## k0s

- Independent of the Spur self-update pipeline. The pinned version lives in
  `spur_core::k0s::K0S_PINNED_VERSION` and is passed in by the caller.
- `sha256sums.txt` is fetched before the large binary so a bad version fails fast. The manifest
  is required (unlike the Spur sidecar); cosign `.sig` signatures are NOT verified.
- Install is atomic (temp sibling on the same filesystem, then rename) with no rollback, since it
  is a fresh install, not a replacement.
- `ensure_k0s` is a no-op whenever a binary already exists at the destination; it does not check
  the existing binary's version.
