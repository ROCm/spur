# Copilot Code Review Instructions — Spur

## Knowledge Base Sync Rule

When a PR modifies source files under `crates/*/src/`, `proto/`, or
`docs/knowledge/`, cross-reference the changes against the knowledge base:

- **Per-crate docs**: `crates/<crate>/AGENTS.md`
- **Cross-cutting docs**: `docs/knowledge/**/*.md`
- **Root navigation**: `AGENTS.md`

Treat the source as authoritative. When a diff contradicts a documented
claim, ask for the doc to be updated in the same PR. Question the code
only when the doc states a compatibility or safety invariant (WAL serde,
lock order, proto field numbers) and the PR gives no reason for changing
it, and phrase that as a question, not a revert suggestion.

Examples of contradictions worth flagging:

1. A renamed enum variant, struct field, or function that is still listed
   under its old name in AGENTS.md.
2. A changed default value (port, timeout, interval) not reflected in docs.
3. A diff that adds or removes an item in a list the doc presents as
   complete (variant tables, RPC lists, module trees, port tables).
4. A reordered lifecycle sequence (e.g., teardown steps) that contradicts
   the documented ordering.
5. A removed or replaced module file still listed in the directory structure.

## What NOT to flag

- Missing detail in prose is fine — AGENTS.md files are curated summaries,
  not exhaustive API docs. Only flag when a stated claim is wrong, or when
  a list presented as complete is now missing an item.
- Test files, CI configs, and build scripts do not need KB coverage.
- Comments in source code do not need to match AGENTS.md verbatim.

## Code Review Standards

See root `AGENTS.md` (Conventions, Breaking Changes, Do Not) for the full
set. Key rules for review:

- Code must pass the CI clippy invocation with the `[workspace.lints.clippy]`
  set; do not apply `clippy::pedantic`.
- Proto: field numbers are append-only; never reuse or renumber.
  `package slurm;` is frozen (gRPC paths depend on it).
- SPDX license headers are enforced by CI.
