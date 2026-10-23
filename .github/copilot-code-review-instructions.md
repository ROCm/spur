# Copilot Code Review Instructions — Spur

## Knowledge Base Sync Rule

When a PR modifies source files under `crates/*/src/`, `proto/`, or
`docs/knowledge/`, cross-reference the changes against the knowledge base:

- **Per-crate docs**: `crates/<crate>/AGENTS.md`
- **Cross-cutting docs**: `docs/knowledge/**/*.md`
- **Root navigation**: `CLAUDE.md`

Flag any change that contradicts a documented claim. Examples:

1. A renamed enum variant, struct field, or function that is still listed
   under its old name in AGENTS.md.
2. A changed default value (port, timeout, interval) not reflected in docs.
3. A new RPC, WAL operation, or config section not added to the docs.
4. A reordered lifecycle sequence (e.g., teardown steps) that contradicts
   the documented ordering.
5. A removed or replaced module file still listed in the directory structure.

When flagging, cite the specific doc file and line, and the source file
that contradicts it. Suggest the fix (update the doc or revert the source).

## What NOT to flag

- AGENTS.md files omitting details is fine — they are curated summaries,
  not exhaustive API docs. Only flag when a stated claim is wrong.
- Test files, CI configs, and build scripts do not need KB coverage.
- Comments in source code do not need to match AGENTS.md verbatim.

## Code Review Standards

- Rust: follow `clippy::pedantic` with the project's existing allows.
- Proto: field numbers are append-only; never reuse or renumber.
- Shell scripts: `set -euo pipefail`, SPDX header.
- Security: no hardcoded credentials, no `unsafe` without justification.
