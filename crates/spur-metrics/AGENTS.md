# AGENTS.md -- spur-metrics

OpenMetrics 1.0 export for `spurctld`. Routes, leader gating, and the HTTP server live in
`spurctld/src/metrics_server.rs`; this crate only builds snapshots and encodes them.

## Invariants

- Snapshots (`*Snapshot`) are rebuilt on every scrape and encoded into a fresh `Registry`.
  `K8sMetrics` is the exception: a long-lived accumulator registered once at construction,
  because counters/histograms cannot be rebuilt from a point-in-time snapshot.
- Per-state arrays are indexed by proto wire discriminant (`to_proto_i32`) so indices stay stable.
  State metric suffixes come from `job_state_metric_suffix()` / `node_state_metric_suffix()`.
- `K8sMetrics::ensure_series()` pre-creates drift/refusal counters at zero so alerting rules find
  them before the first event. `K8sMetrics::remove_node()` must be called when a node leaves k0s
  inventory, or decommissioned nodes export stale series forever.

## Pitfalls

- `prometheus-client` appends `_total` to counter names on encode. Register counters without it;
  user-facing docs reference the suffixed name.
- Karma counters are not monotonic since process start: `reset_karma_stats()` clears them on
  leadership gain (`spurctld` `scheduler_loop.rs`).
- Karma counters have no route of their own. They are appended to `/metrics/jobs-users-accts`
  via `concat_encoded()`, which strips intermediate `# EOF` markers; concatenating raw bodies
  leaves everything after the first `# EOF` unparsed by strict scrapers.
- `spur_nodes_liveness_breaker_tripped` and `spur_nodes_silent_answering` are set by `spurctld`
  (`node_liveness.rs`), not by `NodeMetricsSnapshot::collect`.
- `/metrics/jobs-users-accts` checks `metrics.high_cardinality` (404) before the leader check.
  Every endpoint except `/metrics/auth` and `/metrics/audit` returns not-leader on followers.

## Coupling

- Metric names are user-facing (dashboards, alert rules). Renaming or removing one is a breaking
  change. When adding or changing a metric, update `docs/user-guide/monitoring-jobs.rst`,
  `docs/knowledge/deployment/operations.md` (Monitoring section), and for karma
  `docs/admin-guide/karma-scoring.rst`.
