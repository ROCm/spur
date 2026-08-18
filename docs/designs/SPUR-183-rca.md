# Root-Cause Analysis: SPUR-183

## Bug Summary

k0s worker join fails with `crypto/rsa: verification error ... kubernetes-ca`
after a `spur k8s down --reset` + `spur k8s up` cycle, when the same `spurctld`
leader process stays running across the cycle. The worker's `k0sworker` unit is
active but the node never appears in `kubectl get nodes`. Priority P1 (Gating).

## Environment

- **Component:** `spurctld` k0s reconcile loop (`crates/spurctld/src/cluster_k8s.rs`)
- **Trigger config:** single long-lived `spurctld` leader; teardown + rebuild of
  the k0s cluster without restarting the controller.
- **Setup:** any k0s topology (single-CP or HA); manifests on a worker join.

## Symptom

After `down --reset` (which regenerates the cluster CA on the next `up`, because
`k0s reset` wipes `/var/lib/k0s` on the control plane), a worker is handed a join
token that authenticates against the **previous** cluster's CA. k0s's worker join
validates the API server's serving cert against the CA embedded in the token and
fails: `crypto/rsa: verification error` referencing `kubernetes-ca`.

Symptom classification: **Regression / conditional** — only manifests across a
teardown+rebuild within one controller process lifetime. A fresh `spurctld` (the
documented workaround) starts with an empty cache and joins cleanly.

## Prior Investigation

The JIRA description already localizes the defect correctly (controller-side
`join_tokens` cache surviving teardown). This RCA confirms it against the merged
code and rules out the node-side mechanism as the fix site.

Related: SPUR-113 / PR #655 added a node-side token-*file* purge (`purge_unit` in
`crates/spurd/src/cluster.rs`) on the reset path. That is a **different**
mechanism and does **not** fix SPUR-183 (see "Why the node-side purge is not
enough" below).

## Root Cause

The reconcile loop caches one minted join token per node in an in-memory
`HashMap<String, String>` declared as a local in `run()`:

- `crates/spurctld/src/cluster_k8s.rs:66` — `let mut join_tokens: HashMap<..> = HashMap::new();`
  is scoped to the `spurctld` process. It is threaded by `&mut` into
  `reconcile_phase` → `converge_provisioning`.

On teardown (`spur k8s down`), the reconcile loop runs the `K0sPhase::Down` arm:

- `crates/spurctld/src/cluster_k8s.rs:164-167` — calls `stop_all_components()` and
  returns. It **never clears `join_tokens`.**
- `stop_all_components` (`cluster_k8s.rs:1050`) stops each node's component and
  clears its `k0s_role`, but does not touch the token cache.

On the next `up`, roles are re-assigned **before** convergence runs, so the one
place that *does* clear the cache never fires:

- `cluster_up` (`crates/spurctld/src/server.rs:2582`) blocks re-up until all roles
  have drained (`assigned == false`), then sets `Provisioning`.
- Next tick: `provision_assignments` re-assigns roles, **then**
  `converge_provisioning` runs. Its guard
  `if assigned.is_empty() { join_tokens.clear(); ... }`
  (`cluster_k8s.rs:695-698`) sees a non-empty `assigned`, so it does not clear.
- `converge_provisioning` then reuses the cached token for the worker
  (`cluster_k8s.rs:769` — `match join_tokens.get(&node.name)`) instead of minting
  a fresh one against the newly-bootstrapped CA.

The cached token was minted (`mint_join_token`, `cluster_k8s.rs:588`) from the
*previous* incarnation's control-plane agent, carrying the old CA. The worker is
started with it via `StartClusterComponentRequest.join_token`
(`spawn_start_component`, `cluster_k8s.rs:1071`) and fails CA verification.

### Code Path

```
spur k8s down --reset
  └─ cluster_down (server.rs:2901) → set_k0s_phase(Down, reset=true)
  └─ reconcile tick: K0sPhase::Down (cluster_k8s.rs:164)
        └─ stop_all_components  →  roles drain, join_tokens UNTOUCHED   ← defect

spur k8s up
  └─ cluster_up (server.rs:2552) → requires assigned==false → set_k0s_phase(Provisioning)
  └─ reconcile tick:
        ├─ provision_assignments  → re-assigns k0s_role to every member
        └─ converge_provisioning (cluster_k8s.rs:684)
              ├─ assigned NON-empty → clear-guard skipped                ← defect
              └─ join_tokens.get(worker) = STALE token (old CA)          ← symptom
                    └─ spawn_start_component(worker, Some(stale_token))
                          └─ k0s worker: crypto/rsa verification error
```

### Trigger Condition

Both must hold:
1. The `spurctld` leader process is **not** restarted across `down`→`up`
   (otherwise the local `join_tokens` starts empty — the current workaround).
2. The teardown regenerates the CA on rebuild (any `down`+`up` that
   re-bootstraps etcd; `--reset` guarantees it by wiping `/var/lib/k0s`).

### Why the node-side purge is not enough

Making `down` always `k0s reset` + purge the node's token *file* (the PR #655
mechanism, and the behavior requested for this fix) does **not** by itself close
SPUR-183. On the next `up` the controller **pushes a fresh token file** to the
worker as part of `StartClusterComponent`, overwriting whatever the node has.
If the pushed token is the controller's stale cached one, a pristine
freshly-reset node still fails CA verification. The authoritative token source is
the controller cache, so the cache is where the fix must live.

## Proposed Fix

Two complementary changes. **[1] is required to close the bug; [2] is the
requested UX/behavior change and is safe hygiene but not sufficient alone.**

### [1] Clear the controller token cache on teardown (the actual fix)

Clear `join_tokens` in the `K0sPhase::Down` arm so the cache lifetime is bound to
the cluster incarnation, not the process lifetime. This mirrors the existing
`assigned.is_empty()` clear and guarantees the next `up` mints fresh tokens
against the new CA.

### [2] Make `spur k8s down` always purge node state (requested behavior)

Drop the `--reset`-gated distinction so `down` unconditionally performs the
destructive teardown (`k0s reset` + `purge_unit` on every node), matching the
"leave no state behind" behavior of PR #655's remove path. This removes the
soft-stop semantics of a bare `down` (see Risk).

### Changes Required

| File | Change | Reason |
|------|--------|--------|
| `crates/spurctld/src/cluster_k8s.rs` (`reconcile_phase`, `Down` arm ~L164) | Clear `join_tokens` on the `Down` phase | **[1]** Bind token-cache lifetime to the cluster incarnation; stop reusing a token minted against the old CA |
| `crates/spurctld/src/server.rs` (`cluster_down` ~L2925) | Force `reset=true` (or drop the field from the decision) so teardown is always destructive | **[2]** Always purge node-side k0s state/files |
| `crates/spur-cli/src/k8s.rs` (`Down { reset }` ~L84, `cmd_down` ~L304) | Remove/deprecate the `--reset` flag; always request a full teardown | **[2]** No explicit `--reset` required |
| `docs/` (k8s teardown page) | Document that `down` is now always destructive; note `--reset` removal/deprecation | User-facing behavior + CLI change (AGENTS.md docs rule) |

### Risk Assessment

**Medium.**
- **[1]** is low-risk and surgical: clearing an in-memory cache on teardown. The
  only tokens dropped are for a cluster being torn down; a node still mid-join
  would simply be re-minted a fresh (valid) token on the next tick.
- **[2]** is the higher-risk half: it changes user-facing CLI semantics and makes
  a bare `down` unconditionally destructive (`k0s reset` wipes `/var/lib/k0s`).
  This removes the ability to stop-and-restart a cluster without a full rebuild.
  Removing the `--reset` flag is a **breaking CLI change** (Slurm-compat surface
  per AGENTS.md) — prefer deprecating the flag (accept-and-ignore) over removing
  it, to avoid breaking existing scripts. Confirm the intent to always-destroy.

### Dependencies

None. `ClusterDownRequest.reset` stays on the proto for wire compat even if the
CLI stops exposing it (do **not** renumber/remove the proto field).

## Test Plan

- **Unit (fix [1], the regression guard):** drive `reconcile_phase` through a
  `Provisioning → Ready → Down → Provisioning` sequence on a `ClusterManager`
  test fixture (pattern already in `cluster.rs` tests, e.g.
  `reconcile_phase_reports_no_error_for_down_and_degraded` at `cluster.rs:14835`
  and the reconcile wiring tests around `cluster.rs:14705-14807`). Seed a token
  into `join_tokens`, run the `Down` tick, assert the cache is empty afterward so
  the rebuild cannot reuse it. This exercises the real `converge`/`reconcile`
  code, not a simulation.
- **Unit (behavior [2]):** `cluster_down` sets `reset_requested=true`
  unconditionally (extend the `server.rs` `cluster_down` tests); CLI `cmd_down`
  always sends a full-teardown request (extend `parses_down_reset_and_status` at
  `k8s.rs:475`).
- **Regression guard:** assert a bare `down` (no flag) still drains roles and, on
  the following `up`, workers are handed a **freshly minted** token (cache miss),
  not a cached one.
- **Manual E2E:** single long-lived `spurctld`; `up` → `down` → `up`; confirm the
  worker joins and appears in `kubectl get nodes` with no `kubernetes-ca`
  verification error, without restarting the controller.
