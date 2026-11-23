---
applyTo: "crates/*/src/**/*.rs,proto/**/*.proto"
---

# Knowledge Base Sync Check

When reviewing changes to Rust source or proto files, verify that the
corresponding knowledge base documentation is still accurate:

1. Find the crate's `AGENTS.md` (same directory as `Cargo.toml`).
2. Check if the diff changes any fact documented there:
   - Invariants, pitfalls, and coupling rules ("if you change X, also change Y")
   - Symbols the doc names (renamed or removed types, functions, files)
   - Lifecycle orderings (startup, teardown, dispatch sequences)
   - Compatibility or security constraints
3. If a documented fact is now wrong, flag it with the specific AGENTS.md
   line and what needs to change.
4. Also check `docs/knowledge/**/*.md` for cross-cutting claims about the
   changed component (architecture, compatibility, execution pipeline).

Don't flag missing detail. AGENTS.md holds only what the code cannot show,
so it deliberately omits module trees, type tables, enum variants, and
constant values. Do flag a diff that adds such restatements to AGENTS.md.
