# TRES Billing Weights — Implementation Plan

**Status:** Complete (CPU+GPU). Memory billing on follow-on branch.

## Problem

Fairshare bills CPU-seconds and GPU-seconds at 1:1. A user holding 8 GPUs for
an hour and a user running 128 CPUs for an hour accumulate nearly the same
`RawUsage`. On GPU clusters this undercharges GPU consumers.

## Design decisions

1. **Billing computed at read time, not stored.** The `usage` table stores raw
   `cpu_seconds` and `gpu_seconds`. The weighted billing scalar is computed on
   every fairshare refresh from the configured weights. Changing weights
   retroactively reweights all historical usage — no migration.

2. **Slurm-compatible config format.** `tres_billing_weights = "CPU=1.0,GRES/gpu=16.0"`
   parses the same string Slurm accepts. Case-insensitive, unknown keys
   silently ignored for forward compatibility.

3. **Empty = backward compatible.** No config → `{cpu: 1.0, gpu: 1.0}` →
   `billable = cpu_seconds + gpu_seconds`, identical to pre-feature behavior.

4. **Memory deferred.** The `Mem=` key is parsed and accepted but memory
   seconds are not tracked. Memory billing will follow in a separate change.
   When it lands, jobs that omit `--mem` will use the partition's
   `DefMemPerCPU` / `DefMemPerGPU` defaults — that default propagation is
   covered by a separate spec.

## File map

| File | What changed |
|------|-------------|
| `crates/spur-core/src/config.rs` | `BillingWeights` parser + `UsageResetPeriod` enum. 13 unit tests. |
| `crates/spurctld/src/accounting/fairshare.rs` | `compute_fairshare()` takes `&BillingWeights` instead of hardcoded sum. 13 unit tests. |
| `crates/spurctld/src/accounting/db.rs` | `ModifyUsage` — set/reset/subtract raw usage in the DB. |
| `crates/spurctld/src/accounting/grpc.rs` | `ModifyUsage` RPC. Billing weight fields on `GetUsageResponse`. |
| `crates/spurctld/src/accounting/mod.rs` | `attach_billing_weights()`, usage reset timer goroutine. |
| `crates/spurctld/src/fairshare_cache.rs` | Passes weights through to `fairshare_factors()`. |
| `crates/spurctld/src/server.rs` | Wires billing weights into accounting startup. |
| `crates/spurctld/src/audit/registry.rs` | Audit event type for `ModifyUsage`. |
| `crates/spur-cli/src/sacctmgr.rs` | `modify user/account set rawusage=N` (zero, arbitrary, negative subtract). |
| `crates/spur-cli/src/scontrol.rs` | `show config` prints `TRESBillingWeights` and `UsageResetPeriod`. |
| `crates/spur-cli/src/sshare.rs` | Weighted `RawUsage`/`NormUsage` display; `-l` retains per-TRES columns. |
| `proto/slurm.proto` | `ModifyUsageRequest`, billing weight + reset period fields on responses. |
| `crates/spur-k8s/src/agent.rs` | Config forwarding. |
| `crates/spurd/src/agent_server.rs` | Config forwarding. |

## Test coverage

26 unit tests total:

- **Config parsing (10):** Slurm-format string, empty/default, case-insensitive, negative clamping, unknown keys, `Mem=` with `G` suffix.
- **Fairshare with weights (13):** Weighted billing scalar, half-life decay unaffected, hierarchical share computation, `CPU=0.0` GPU-only billing, all-zero degenerate case.
- **Usage reset period (3):** Calendar boundary scheduling, config parsing, `none` default.

## What's next

1. **Memory billing** — adds `mem_mb_seconds` column and `Mem=` weight in fairshare. Follows after this branch lands.
2. **Partition defaults** — separate spec for `DefMemPerCPU` / `DefMemPerGPU` / default TRES applied to jobs that omit explicit resource requests.
