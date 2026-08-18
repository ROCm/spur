**Spur preemption behavior vs. Slurm — current state and upcoming fixes**

For anyone running into preemption issues: here's a clear picture of where Spur stands today relative to Slurm, and what's changing.

**What Spur already matches Slurm on**
- `preempt_mode` (`cancel`, `requeue`, `suspend`, `off`) — configurable per partition, with QOS override
- Partition `priority_tier` — jobs in a lower-tier partition are protected from preemption by equal-or-lower-tier jobs
- Named reservation protection — jobs running inside an active time-windowed reservation are shielded from same-or-lower-tier preemption

**Current gaps (and what's being fixed)**

The two gaps that matter most in practice are being addressed:

**[1] No QOS preemption hierarchy** (Slurm: `PreemptType=preempt/qos`)
Today, any job with 2× higher effective priority can preempt any other job regardless of which QOS or pool it belongs to. There is no way to say "tier A may preempt tier B but not tier C." This means a team's quota provides no runtime guarantee — a higher-priority job from another pool can still displace running work that is well within its allocated quota.
Fix: adding a `preempt` allow-list per QOS and a global `preempt_type = qos_priority` switch that enforces it. When enabled, a job may only preempt jobs in QOS tiers explicitly listed in its own preempt allow-list.

**[2] No minimum guaranteed running time** (Slurm: `PreemptExemptTime`)
A newly started job is immediately eligible for preemption. There is no grace window.
Fix: adding `preempt_exempt_time` at global config, partition, and QOS level. A job running for less than the configured threshold is skipped as a preemption candidate.

**Workaround today:** setting `preempt_mode = off` on a partition makes its jobs fully immune to preemption. It's a blunt instrument but is the reliable protection available now.

Both fixes are implemented and shipped in this PR. See `docs/admin-guide/accounting.rst` for the new QOS fields and the preemption hierarchy section, and `docs/admin-guide/configuration.rst` for `scheduler.preempt_type` and `scheduler.preempt_exempt_time`.
