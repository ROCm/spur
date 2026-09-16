Administrator Guide
===================

Configuring and operating a Spur cluster.

- :doc:`configuration` — the full ``spur.conf`` reference. Every controller and
  node setting, with its type, default, and meaning.
- :doc:`accounting` — managing accounts, users, QOS, associations, and resource
  limits. Requires a PostgreSQL-backed accounting database.
- :doc:`idle-fill-scheduling` — letting over-quota jobs use capacity nobody with a
  quota claim wants, and reclaiming it when somebody does. Off by default;
  enabling it narrows a preemption guarantee.
- :doc:`dme-integration` — attaching per-job labels (``job_id``, ``job_user``,
  ``job_partition``) to AMD Device Metrics Exporter GPU metrics via prolog/epilog
  hooks.

Partitions are defined statically in ``spur.conf`` (see
:doc:`/deployment/partitioning`), not created at runtime.
