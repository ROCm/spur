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

1. A renamed symbol (type, field, function, file) that a doc still names
   under its old name.
2. A diff that breaks a documented invariant, pitfall, or coupling rule
   ("if you change X, also change Y") without updating the doc.
3. A reordered lifecycle sequence (e.g., teardown steps) that contradicts
   the documented ordering.
4. A change to a value a doc states as an invariant (not a value it merely
   mentions in passing).

## What NOT to flag

- Missing detail is fine. Crate `AGENTS.md` files hold only crate-local
  invariants, pitfalls, coupling, and rationale; they deliberately do not
  restate module trees, type tables, enum variants, or constant values.
  Only flag when a stated claim is wrong.
- Do flag a PR that adds such restatements to an `AGENTS.md`.
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
