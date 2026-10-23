---
applyTo: "crates/*/src/**/*.rs,proto/**/*.proto"
---

# Knowledge Base Sync Check

When reviewing changes to Rust source or proto files, verify that the
corresponding knowledge base documentation is still accurate:

1. Find the crate's `AGENTS.md` (same directory as `Cargo.toml`).
2. Check if the diff changes any fact documented there:
   - Enum variants (added, removed, renamed)
   - Default values (ports, timeouts, intervals)
   - Public function signatures
   - RPC definitions
   - Module file listings
   - Config struct fields or section names
   - Lifecycle orderings (startup, teardown, dispatch sequences)
3. If a documented fact is now wrong, flag it with the specific AGENTS.md
   line and what needs to change.
4. Also check `docs/knowledge/**/*.md` for cross-cutting claims about the
   changed component (architecture, compatibility, execution pipeline).

Do NOT flag omissions — AGENTS.md is a curated summary. Only flag claims
that the diff makes incorrect.
