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

Two complementary changes, both required. Hardware testing (see below) showed
that **[1] alone does not close the bug**: on a real teardown the operative stale
state is the node-side `/etc/k0s/token` file, and it only survives when spurd's
`k0s reset` cannot complete — at which point the controller sees the node's unit
`active` (k0s crash-looping on the stale token) and never re-pushes a fresh one.

### [1] Clear the controller token cache on teardown

Clear `join_tokens` in the `K0sPhase::Down` arm so the cache lifetime is bound to
the cluster incarnation, not the process lifetime. Mirrors the existing
`assigned.is_empty()` clear. Necessary defense-in-depth so the controller never
re-pushes a token minted against the old CA.

### [2] Purge the node join token even when `k0s reset` fails (the operative fix)

In spurd's teardown (`ClusterSupervisor::stop` and `K0sAgent::stop_untracked`),
`k0s reset` was awaited with `?`, so a failing reset (broken/half-installed k0s
binary, or a node whose k0s was swapped out) short-circuited **before**
`purge_unit()` ran. The spurd-owned unit (`Restart=always`) then restarted k0s
with the **stale token file**, which was minted against the torn-down CA →
`crypto/rsa: verification error ... kubernetes-ca` on the next join. Purge the
unit + token file **unconditionally**, then surface the reset failure afterward.

### Changes Required

| File | Change | Reason |
|------|--------|--------|
| `crates/spurctld/src/cluster_k8s.rs` (`reconcile_phase`, `Down` arm) | Clear `join_tokens` on the `Down` phase | **[1]** Bind token-cache lifetime to the cluster incarnation |
| `crates/spurd/src/cluster.rs` (`ClusterSupervisor::stop`) | Run `purge_unit()` even when `k0s reset` fails; surface the error after | **[2]** Never leave a stale-CA token/unit behind |
| `crates/spurd/src/cluster.rs` (`K0sAgent::stop_untracked`) | Same unconditional purge for the untracked (post-restart) teardown path | **[2]** Same guarantee when spurd has no tracked supervisor |

### Risk Assessment

**Low.** Both changes only reorder existing teardown steps so the purge is not
skipped on a failed reset, and add a defensive in-memory cache clear. No proto,
config, or user-facing CLI change. `down --reset` semantics are unchanged; the
purge simply becomes robust to a failing `k0s reset`.

### Dependencies

None.

## Test Plan

- **Unit (fix [1]):** `reconcile_phase_reports_no_error_for_down_and_degraded`
  (`spurctld/src/cluster.rs`) extended to seed a token into `join_tokens` and
  assert the `Down` tick clears it. Exercises the real reconcile code. — DONE, passes.
- **Unit (fix [2]):** `reset_and_purge_removes_token_even_when_k0s_reset_fails`
  (`spurd/src/cluster.rs`): drives `ClusterSupervisor::reset_and_purge` with a
  non-existent k0s binary and asserts (a) it returns `Err` (reset surfaced) and
  (b) the token + unit files are still removed. Fully hermetic (no systemd/k0s
  host state), so it runs in normal CI. — DONE, passes.
- **Hardware E2E (done, 2026-08-18, testbed master 10.11.98.229 + galena
  10.11.194.197):**
  - *Baseline reproduced on pre-fix build `f9c31da9`:* with galena's k0s made
    non-executable (so `k0s reset` fails on teardown), after `down --reset` + `up`
    on the same controller, galena's journal showed continuous
    `crypto/rsa: verification error ... kubernetes-ca` and `kubectl get nodes`
    reported "No resources found" while `spur k8s status` showed the unit active —
    exactly the reported symptom. Confirmed the master CA regenerates across the
    cycle (`29368e8e…` → `4160457669…`).
  - *Fixed build `c6ec2134`:* same scenario — the patched spurd purged the stale
    `/etc/k0s/token` on teardown **despite the failed `k0s reset`** (verified
    token file gone), and on the next `up` galena rejoined with a fresh token: no
    `kubernetes-ca` errors, reaching `kubectl get nodes` → `Ready`, controller
    never restarted.
