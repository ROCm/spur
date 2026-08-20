# QoS & Preemption E2E Test Plan

Tracking missing e2e coverage for QoS and preemption. Tests 1–10 are written on
`feat/qos-e2e-tests`. Tests 11–18 are written after `feat/preemption-qos-hierarchy-clean`
is merged.

---

## Group 1 — Preemption Mode Behavior
*Testable on current `upstream/main`*

| # | Test | What It Verifies |
|---|------|-----------------|
| 1 | `test_preempt_mode_cancel_removes_job_permanently` | Partition `preempt_mode=cancel`: high-priority job arrives, low job is cancelled and does NOT reappear as pending |
| 2 | `test_preempt_mode_requeue_returns_job_to_pending` | Partition `preempt_mode=requeue`: low job returns to PENDING after eviction and eventually reruns |
| 3 | `test_preempt_mode_suspend_freezes_then_resumes_job` | Partition `preempt_mode=suspend`: scheduler (not manual scontrol) freezes low job when high-priority job arrives; low job resumes after high job finishes |
| 4 | `test_preempt_mode_off_blocks_preemption` | Partition `preempt_mode=off`: high-priority job arrives but does NOT preempt the running low-priority job — it waits |

## Group 2 — Priority Threshold
*Testable on current `upstream/main`*

| # | Test | What It Verifies |
|---|------|-----------------|
| 5 | `test_priority_just_below_threshold_prevents_preemption` | Priority gap just under the 2x threshold: no preemption fires, pending job waits |
| 6 | `test_priority_just_above_threshold_triggers_preemption` | Priority gap just above the 2x threshold: preemption fires |
| 7 | `test_priority_tier_drives_preemption_across_partitions` | Higher `priority_tier` partition job preempts a job on a lower-tier partition despite equal raw priority |

## Group 3 — QoS Preempt Mode Override
*Testable on current `upstream/main`*

| # | Test | What It Verifies |
|---|------|-----------------|
| 8 | `test_qos_preempt_mode_cancel_overrides_partition_requeue` | Partition says `requeue`, high-priority QoS says `cancel` — victim is cancelled, not requeued |
| 9 | `test_qos_preempt_mode_off_blocks_partition_cancel` | Partition says `cancel`, victim job's QoS says `preempt_mode=off` — preemption is blocked |

## Group 4 — Multi-Node Preemption
*Testable on current `upstream/main`*

| # | Test | What It Verifies |
|---|------|-----------------|
| 10 | `test_multinode_job_preemption_dispatches_to_all_agents` | Running job spans 2+ nodes; preemption signal is dispatched to all agents and job terminates cleanly |

---

## Group 5 — QoS Allow-List Hierarchy (`preempt_type=qos_priority`)
*Requires `feat/preemption-qos-hierarchy-clean` to be merged first*

| # | Test | What It Verifies |
|---|------|-----------------|
| 11 | `test_qos_not_in_allow_list_cannot_preempt` | High-priority pending job whose QoS doesn't list the victim's QoS — no preemption even with large priority gap |
| 12 | `test_qos_in_allow_list_can_preempt` | Same setup but allow-list entry present — preemption fires |

*(These already exist on the unmerged branch — port and clean up)*

## Group 6 — Burst QoS
*Requires `feat/preemption-qos-hierarchy-clean` to be merged first*

Burst QoS is a convention (not a dedicated field): a QoS with a negative priority delta that
is listed in the normal QoS's preempt allow-list. Jobs under burst QoS are opportunistic —
they fill overflow capacity but are evicted when a normal-priority job needs the slot.

| # | Test | What It Verifies |
|---|------|-----------------|
| 13 | `test_burst_qos_preempted_by_normal_qos_via_priority` | Burst job (low priority, in normal QoS's allow-list) is preempted when a normal QoS job arrives with sufficient priority gap |
| 14 | `test_burst_qos_not_preempted_by_another_burst_qos_job` | Two burst jobs contending — neither preempts the other (both low priority, gap below 2x threshold) |
| 15 | `test_burst_qos_not_preempted_by_qos_without_allow_list_entry` | A QoS that does NOT list burst in its allow-list cannot preempt a burst job even with a large priority gap |
| 16 | `test_burst_qos_full_workflow_capacity_overflow_then_preemption` | Default QoS hits `maxtresperuser` cap → user submits with burst QoS → runs on overflow capacity → normal job arrives → burst job preempted, normal job gets the slot |
| 17 | `test_burst_qos_requeue_mode_restores_job_to_pending` | Burst QoS has `preempt_mode=requeue`: evicted burst job returns to PENDING and reruns after normal job finishes |
| 18 | `test_burst_qos_partition_off_blocks_preemption` | Even with burst in the allow-list and a large priority gap, `preempt_mode=off` on the partition blocks eviction |
