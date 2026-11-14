Administrator Guide
===================

Configuring and operating a Spur cluster.

- :doc:`configuration` — the full ``spur.conf`` reference. Every controller and
  node setting, with its type, default, and meaning.
- :doc:`rbac` — User, Coordinator, Operator, and Administrator bindings.
- :doc:`fabric-observation` — read-only GPU fabric membership discovery and inspection.
- :doc:`accounting` — managing accounts, users, QOS, associations, and resource
  limits. Requires a PostgreSQL-backed accounting database.
- :doc:`partition-resource-limits` — per-partition caps on the CPUs, memory,
  and GPUs a partition may use on a shared node, and per-job memory limits and
  defaults.
- :doc:`idle-fill-scheduling` — letting over-quota jobs use capacity nobody with a
  quota claim wants, and reclaiming it when somebody does. Off by default;
  enabling it narrows a preemption guarantee.
- :doc:`dme-integration` — attaching per-job labels (``job_id``, ``job_user``,
  ``job_partition``) to AMD Device Metrics Exporter GPU metrics via prolog/epilog
  hooks.
- :doc:`karma-scoring` — per-user job lifecycle counters for computing a cluster
  citizenship karma score in Prometheus.
- :doc:`tres-billing-weights` — configurable TRES billing weights for fairshare,
  so GPU-heavy clusters bill GPU-seconds at a higher rate than CPU-seconds.
- :doc:`fabric-health-check` — a per-node RDMA fabric check
  (``spur-fabric-check``) to run as a ``[[health.checks]]`` program.

Partitions are defined in ``spur.conf`` (see :doc:`/deployment/partitioning`).
``scontrol create/update/delete partition`` change them at runtime until the
next ``scontrol reconfigure``.
