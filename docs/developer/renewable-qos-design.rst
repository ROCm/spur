Renewable QoS: implementation and validation gates
==================================================

Status and scope
----------------

The implementation adds the dedicated ``RenewJob`` RPC and
``spur control renew`` command. It is disabled by default. No live-cluster
validation has been performed; this change is not permission to resume jobs or
change cluster policy. An ordinary successful ``UpdateJob`` response is not a
renewal receipt. The historical analysis and acceptance checklist below remain
useful review context; not every listed integration gate has been executed.

Implemented interface
~~~~~~~~~~~~~~~~~~~~~

After upgrading **every controller**, explicitly acknowledge the WAL format
upgrade and qualify a QoS in ``spur.conf``::

    [renewal]
    upgraded_controllers = true

    [renewal.qos.qualified]
    enabled = true
    class = "non_burst"
    max_runway_seconds = 86400

``class`` defaults to ``unqualified``; ``burst`` refuses renewal even with
``enabled = true``. Names do not confer eligibility. The administrator owns the
classification: do not classify an existing burst policy as non-burst. Removing
the grant prevents new renewals, not previously committed expiry extensions.
Never enable the upgrade acknowledgement while an old controller can lead or
replay new entries. Downgrading after new renewal entries is unsupported.

Inspect ``spur show job JOB`` for ``RunAttempt``, ``DeadlineRevision`` and
``AllocationExpiry``, then request an explicit absolute expiry::

    spur control renew 42 --run-attempt 1 --revision 0 \
        --request-id owner-chosen-unique-id --expires-at 2026-09-17T12:00:00Z

Only an authenticated, verified owner may renew; no administrator ownership
bypass or claimed-user fallback is accepted. The RPC has no mixed-edit fields.
Old servers return UNIMPLEMENTED. The CLI checks the mandatory receipt's job,
run, revision, request ID and expiry and never falls back to cancel/requeue.
Identical retries return a historical receipt, including after the run ends;
a conflicting reuse refuses. Receipts are retained with the job, bounded to
4096 successful renewals per job; further requests refuse instead of evicting
idempotency history. Refused requests do not consume receipt storage.

Runway is not lifetime. An expiry 24 hours from now after one elapsed hour
requires at least 25 hours of QoS, association and partition lifetime budget.
Existing MaxWall/MaxTime are never silently raised or reinterpreted. Configure
an appropriately qualified policy before requesting that runway. Group-wall
capped QoS is conservatively ineligible because cached accounting cannot reserve
future group wall atomically. Jobs with suspension history are ineligible.

**Existing-running-allocation limitation:** only runs launched after enabling the
upgrade gate carry a replicated canonical start time and job specification.
Pre-upgrade running allocations refuse renewal; this implementation cannot extend
an already-running legacy holder in place. It does not cancel/requeue that holder
to manufacture eligibility. A separately reviewed replicated adoption mechanism
would be needed for safe migration of such allocations.

Acceptance changes only the time budget and deadline revision. Processes, run,
node/device allocations, resource accounting and start time are not reset.
Preemption, failure, cancellation, inactivity enforcement and maintenance still
apply; a receipt is not a guarantee of runtime or node health. The controller is
the allocation wall-time enforcer; no node lease-update RPC is required.

The watchdog commits a run/deadline-guarded timeout claim and checks its actual
apply result before sending SIGTERM. A stale or duplicate claim sends nothing.
Once claimed, renewal refuses. Grace completion is guarded by run and claim
before finalizing and sending SIGKILL. Legacy runs cannot renew and therefore use
run fencing without comparing their historically replica-local deadlines; the
leader's replicated timeout instant is shared by every replica. Qualified new
runs additionally compare their canonical deadline and revision.

QoS and association policy publication is serialized with renewal commit. Renewal
holds publication guards, not cache read locks: ordinary readers that hold the
job-table lock remain able to read caches while a refresh waits to publish. Legacy active-job deadline/account/QoS/
partition edits are refused to prevent bypassing this fence; pending edits retain
their ordinary path with lifetime-wall checks.

The target is an owner-requested, in-place extension of an administrator-qualified,
non-burst, running allocation. It must preserve job ID, run attempt, nodes,
devices, processes, cgroups, and inputs; it must not requeue, run lifecycle hooks,
or restart a process. Automatic hourly renewal is a client policy, not part of
the initial controller implementation.

Source baseline and concrete blockers
-------------------------------------

References below are relative to the repository at
``467d5215373795bbb4e29a0c090a96f341682817``; line numbers describe that baseline.

* ``proto/slurm.proto:386,623-633``: ``UpdateJob`` already accepts a time limit
  but returns ``Empty``. It has no expected run attempt, deadline revision,
  idempotency key, confirmed expiry, or renewal capability negotiation. Adding
  request fields alone is unsafe: an old server may ignore them and return
  success for an ordinary update.
* ``crates/spurctld/src/server.rs:1535-1556``: the existing RPC checks ownership
  and derives administrator override from verified identity. Without verified
  identity, the ownership comparison uses the claimed user. Renewal must require
  verified identity, not merely reuse that permissive fallback.
* ``crates/spurctld/src/cluster.rs:2947-3075``: ``update_job`` validates account
  membership, partition access, and partition time limits on a candidate, then
  mutates time limit and other fields directly, outside the WAL. Validation and
  mutation use separate lock acquisitions. There is no running-state/run-attempt
  guard and no QoS or association per-job wall-limit check in this path.
  **Existing policy issue:** an owner update can bypass those wall checks; the
  renewal work must not depend on or perpetuate this omission.
* ``crates/spurctld/src/scheduler_loop.rs:2035-2091``: the watchdog snapshots
  running jobs, checks their deadlines, calls ``signal_time_limit(job_id, now)``,
  and then sends SIGTERM. The kill phase similarly uses the earlier snapshot.
* ``crates/spurctld/src/cluster.rs:6184-6191``: applying
  ``JobTimeLimitSignaled`` checks only active state and an unset timeout marker.
  It does not compare the run attempt or the deadline that was observed.
  ``signal_time_limit`` reports proposal success, not whether that particular
  timeout claim was accepted.
* ``crates/spurctld/src/scheduler_loop.rs:2381-2414`` and
  ``proto/slurm.proto:1014-1020``: cancel RPCs carry a run attempt, which helps
  protect replacement runs, but not a newer expiry in the *same* run. Cancel
  delivery here is fire-and-forget. Existing run-attempt fencing does not solve
  the renewal/timeout race.
* ``crates/spur-core/src/job.rs:1053-1075``: runtime and effective deadline account
  for suspension. A raw ``start + duration`` renewal calculation would disagree
  with enforcement. Absolute wall expiry and suspended runtime budget cannot be
  treated as interchangeable.
* ``crates/spurctld/src/cluster.rs:4646-4698``: reservation end enforcement and
  overlap validation are separate paths. Renewal must not promise runway across
  maintenance/reservation boundaries merely because it passed partition MaxTime.

The critical race is reproducible without a cluster:

#. The watchdog snapshots an expired deadline for run A.
#. An update extends that same run and returns success.
#. The watchdog commits its stale timeout marker and sends SIGTERM for run A.

Adding WAL persistence only to the extension does not fix this ordering. An
extension must not be delivered until timeout ownership is fenced in the same
state machine.

Proposed minimum safe contract
------------------------------

Entitlement and policy
~~~~~~~~~~~~~~~~~~~~~~

* Add administrator-managed per-QoS renewable policy, absent/disabled by default.
  Require an explicit qualification as non-burst in administrator policy as well
  as the renewable grant; absence of ``burst`` in a name is not qualification.
  An explicitly burst-classified QoS must be rejected even if misconfigured with
  a grant. The exact policy storage/schema is a review decision, not a currently
  supported configuration key.
* Require current verified ownership, RUNNING state, a finite unexpired deadline,
  matching run identity, and a currently loaded entitlement on every request.
  Fail closed if policy or required accounting data is unavailable. Initial scope
  excludes administrator override and jobs with suspension history; neither gets
  an implicit bypass. Suspension/resume ordering still needs fencing before this
  restriction can be relied on.
* Separate maximum forward runway from lifetime limits. For example, at elapsed
  one hour, expiry ``now + 24h`` requires a lifetime budget of at least 25 hours.
  Existing QoS/association MaxWall and partition MaxTime retain their meaning;
  they are not silently reinterpreted as rolling runway. Stop renewing at the
  tightest applicable lifetime limit. Indefinite renewal and changing existing
  wall-limit semantics are out of scope.
* Recheck resolved QoS and account association authorization even when the request
  does not change them. Reject mixed renewal requests containing account, QoS,
  partition, hold/release, priority, or other edits. Later standalone edits must
  not bypass the same policy fence.
* Preserve resource limits, preemption rights, group usage limits, reservation
  windows, maintenance policy, and fair-share charging. Reuse the real policy
  functions in ``crates/spur-core/src/qos.rs`` and ``account_limits.rs`` where
  applicable; do not rerun submit-count checks as if the running job were new or
  count its resources twice. Renewal needs a strict per-job wall check even where
  submission uses pending-on-limit semantics. Validate exact durations at cap
  boundaries rather than silently truncating seconds to minutes.

Wire contract and retries
~~~~~~~~~~~~~~~~~~~~~~~~~

Prefer an explicitly discriminated extension mode on ``UpdateJob``, preserving
ordinary update behavior. It needs an absolute desired expiry, expected run
attempt, expected deadline revision, and a caller-scoped idempotency key. Persist
request fingerprint and result so an identical retry returns the original
outcome; a conflicting fingerprint or stale run/revision refuses without
mutation. After a later change or termination, a replay is a historical receipt,
not evidence that the allocation still runs or that the old expiry is current.

The response must identify the run/revision and prior/new confirmed expiry, plus
typed refusals (unauthorized, ineligible, not running, expired/timeout claimed,
stale revision, policy limit, reservation conflict, unsupported, unavailable).
The current ``Empty`` response is a compatibility decision requiring explicit
review. Prefer an additive capability advertisement and a mandatory positively
identified renewal receipt; an empty response from an old server is never renewal
success. If this cannot be made safe and compatible on ``UpdateJob``, use an
additive dedicated RPC rather than pretending an ignored field is supported.
No client silently falls back to cancel/requeue or successor submission.

Durability and timeout fencing prerequisite
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#. Introduce a guarded timeout-claim operation carrying expected run attempt and
   expected deadline revision (or equivalent exact deadline identity). Apply it
   atomically against current state. Return an explicit accepted/refused result;
   a committed no-op is not permission to signal.
#. Make the watchdog send SIGTERM only for an accepted timeout claim. Fence the
   grace-period completion by the same run and claim. Once a claim wins, reject
   renewal; never clear the claim to rescue a job which may already be signalled.
#. Introduce a guarded renewal WAL operation and persisted receipt/revision.
   Apply run/state/expiry/claim checks atomically. If renewal wins, a stale timeout
   claim must refuse and emit no signal. If timeout wins, renewal must refuse.
   Cancellation, requeue, completion, suspension, reservation changes, and policy
   updates must not invalidate checks between validation and commit. Define a
   consistent lock/proposal ordering or replicated policy/reservation revisions;
   a pre-proposal snapshot alone is insufficient.
#. Legacy ``UpdateJob`` deadline/policy mutations must participate in that ordering
   and in wall validation, or be refused for active renewable runs. Otherwise the
   new guarded path can still be bypassed through the old path.
#. Persist the deadline, run revision, timeout claim, and idempotency outcome in
   Raft/WAL and snapshots. New fields require backward-compatible serde defaults;
   new WAL variants require an explicit controller upgrade gate. Default-off
   entitlement alone is not sufficient mixed-version replication safety.

These are prerequisites to enabling renewal, not optional follow-up hardening.
Tests must call real state-machine and watchdog decision code, with deterministic
ordering, rather than simulate a second implementation of the race.

Enforcement, forecasts, and accounting
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The inspected wall-time watchdog is controller-side. Do not invent an agent
lease-update requirement without first auditing every launch/executor/stepd,
reconnect, and step timeout path. If no agent enforces an allocation expiry,
renewal should require no agent RPC, and its receipt confirms the controller's
durable decision, not node health or guaranteed runtime. If an agent deadline is
found, capability-gated acknowledged update and reconnect reconciliation become
release prerequisites; partial delivery must not be reported as full success.

Use one canonical confirmed deadline in the watchdog, job display, scheduler
availability forecast, and reservation conflict evaluation. Invalidate/recompute
planned starts on acceptance and prevent a stale scheduler plan from dispatching
against the old availability. Reject an extension that conflicts with a protected
reservation or maintenance interval; do not remove another job's reservation.
Preemption, failure, and explicit cancellation remain allowed and must be stated
in the user-visible meaning of a receipt.

Do not reset start time, run attempt, resource allocations, suspension accounting,
usage history, or fair-share usage factor. Actual elapsed usage remains chargeable
across all renewals. Audit accounting event projection and group-wall consumption
before enabling the feature; renewal is not a fresh submission/completion pair.

Implementation acceptance gates (not executed)
----------------------------------------------

* Policy fixtures: default-off, explicitly qualified non-burst success, burst
  rejection even with a grant, unknown/unloaded policy, revoked association,
  unauthenticated/spoofed identity, non-owner, terminal/pending/suspended state,
  mixed edits, expired deadline, and QoS/account/partition/group caps. Every refusal
  leaves job, accounting, reservation, and revision state unchanged.
* Time fixtures: one elapsed hour plus 24-hour runway succeeds only with sufficient
  lifetime budget; exact cap and one-second-over-cap cases; overflow, invalid
  timestamps, timeout grace, and clock changes. Inject time rather than sleeping.
* Concurrency fixtures: both orderings of timeout versus renewal, two simultaneous
  renewals, policy/reservation changes, suspension, cancel, completion, and requeue.
  Assert a stale timeout emits no agent signal and an old request cannot touch a
  replacement attempt.
* Retry/recovery fixtures: identical retry returns its persisted receipt,
  conflicting key/revision refuses, WAL replay and snapshot restore preserve
  expiry and timeout claim, and a lost response followed by leader failover does
  not add time twice. Read old snapshots/log records with new code.
* Enforcement fixtures: original expiry no longer triggers timeout; accepted
  expiry triggers the existing grace/kill sequence exactly once. Preserve node,
  device, run, and resource identity; assert no launch, teardown, prolog, epilog,
  or cgroup-reset call. Unit tests alone cannot establish unchanged real PIDs.
* Forecast/accounting fixtures: reservation/maintenance overlap refuses; forecast
  reflects the new end, stale plans cannot allocate overlapping resources, usage
  remains cumulative, and preemption rights remain unchanged.
* Compatibility fixtures: old server, old agent, and mixed controller versions
  refuse or gate unsupported renewal. Node reconnect reconciles the same deadline
  if agents enforce it; communication failure cannot manufacture a receipt.
* Isolated integration gate, separately authorized: verify unchanged PIDs/cgroups/
  devices and actual termination timing. No live beam workload is a test fixture.

Implementation validation and migration
---------------------------------------

Controller unit/regression tests exercise real Raft application, retry receipts,
snapshot restoration, timeout ordering, policy refusal and preserved allocation
identity. These do not establish unchanged real PIDs/cgroups or successful live
termination timing. No live-cluster acceptance has been performed.

**Compatibility change:** ordinary active-job deadline, account, QoS and partition
updates now refuse rather than bypassing durable renewal authorization. Migrate
qualified deadline extensions to ``RenewJob``/``spur control renew``. Other
active-job policy edits require ending the allocation or an independently safe
administrative workflow. This behavioral tightening requires a breaking-change
release note; existing default and burst submission/scheduling policy is not
automatically granted renewal eligibility.
