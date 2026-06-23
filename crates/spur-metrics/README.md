# spur-metrics

Cluster metrics aggregation and Prometheus/OpenMetrics text encoding for spurctld.

## Inventory

See [docs/developer/metrics.rst](../../docs/developer/metrics.rst) for the full
metric catalog: data sources, HTTP endpoints, gRPC/CLI/REST coverage, stubs, and
consistency gaps.

## Layout

| Module | Purpose |
|--------|---------|
| `job.rs` | `JobMetricsSnapshot` — scan controller job map |
| `node.rs` | `NodeMetricsSnapshot` — scan controller node map |
| `export/jobs.rs` | Encode job gauges for `/metrics/jobs` |
| `export/nodes.rs` | Encode node gauges for `/metrics/nodes` |
| `export/partitions.rs` | Stub |
| `export/scheduler.rs` | Stub |

Snapshots are built on each scrape in spurctld; this crate does not hold
persistent counters.
