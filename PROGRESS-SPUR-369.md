---
schema_version: 1
jira_id: SPUR-369
jira_cloud_id: amd.atlassian.net
jira_project: SPUR
epic_id: SPUR-367
story_summary: "GPU-local core reservation with NUMA affinity (+ SPUR-386 core/memory specialization, shared spec)"
active_skill: story-design
current_phase: complete
design_doc_dir: docs/designs
design_doc_file: SPUR-369-core-mem-reservation-design.md
last_updated: 2026-10-29T12:00:00
---

# Design progress — SPUR-369 (+ SPUR-386)

Resumed 2026-10-29 from /codie:autodev SPUR-367 (planner agent lacked JIRA
tools, so story-design runs in the main session).

## Decisions

- One shared spec for SPUR-369 and SPUR-386 (SPUR-386 implements Task
  SPUR-328). Post `## codie-design-doc` on both Stories when approved.
- Architecture A: controller computes the core layout (spec / reserved per
  GPU / general) in the apply step from agent-reported topology; schedules
  with counts only (general-pool check, one `reserved_cpus` count per
  allocation); agent picks real CPU IDs. No per-socket launch field.
- Bad layout → derived `core_layout_error`, node unschedulable and shown as
  DRAIN `core-layout: …`; not an admin drain.
- `-c` unset sent as 0; resolved at submit (`def_cpu_per_gpu`: job >
  partition > scheduler).
- Story split: shared groundwork + spec → SPUR-386; reserved per GPU →
  SPUR-369. One Sub-task per build target per Story (12). spur-ffi out of
  scope.

## Section status

| Section | Status |
|---|---|
| (a) Requirements | Approved 2026-10-28 |
| (b) Architecture | Approved 2026-10-29 |
| (c) Build targets / code paths | Approved 2026-10-29 |
| (d) Interfaces | Approved 2026-10-29 |
| (e) Error handling / upgrade | Approved 2026-10-29 |
| (f) Testing strategy | Approved 2026-10-29 |
| Design review | Done (2 passes); final approval 2026-10-29 |

## Not done

- SPUR-369 status left at Opened: the only transition (Move to Triaged)
  requires a Priority, which is Undefined.
