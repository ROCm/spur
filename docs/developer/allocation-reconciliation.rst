Allocation reconciliation
=========================

The controller's Raft state owns job placement and the resource charge for each
run. An agent ledger describes what a node still holds locally. Reconciliation
compares these two views using the job ID, run attempt, and node; a job ID alone
cannot distinguish a delayed report from the current run.

Controller support
------------------

The controller requests a ledger after agent registration, when a heartbeat
requests reconciliation, on leadership gain, during an hourly sweep, and after
an allocation conflict rejects a batch dispatch or standalone allocation
registration. Abandoned placements also trigger a pull to resolve any cleanup
left by a partially accepted dispatch.
An administrator can also set ``UpdateNodeRequest.reconcile`` through the
controller gRPC API. The
``scontrol update ... Reconcile=yes`` spelling is not implemented yet.

A reconciliation request can change job and node state. The controller may
cancel an unrecorded live claim, acknowledge a finished claim through
``SettleRun``, or settle a recorded run that a complete ledger proves missing.
Resource disagreements and evidence the controller cannot safely resolve hold
the node out of scheduling for operator inspection. The controller does not
rewrite an allocation to match an agent's assertion.
An unrecorded live claim keeps the node held after cancellation is requested,
until a fresh ledger proves absence or the agent confirms settlement.

The controller protects absence decisions against launches concurrent with the
ledger request. Incomplete inventories provide no evidence that an omitted run
has ended. A stale agent session or leadership term invalidates the pass.
Registration inventories predate receipt of the request, so they do not license
absence decisions; a fresh controller-requested ledger is needed.
Starting a newer ledger pass for a node invalidates older passes, so a delayed
result cannot undo a hold based on newer evidence. Lifting a reconciliation hold
also requires a cut with no overlapping dispatch.
Destructive actions also require fresh Raft quorum confirmation, so an isolated
former leader cannot cancel work using its stale allocation records.

Each reservation commits one launch validity window before dispatch. Every
``LaunchJob`` or ``RegisterJobAllocation`` for that attempt carries the same
issue and expiry times, including launches sent after PMIx preparation. The
window allows 120 seconds plus twice
``controller.dispatch_timeout_secs`` for preparation and launch (720 seconds
with the default configuration). Setting the RPC timeout to zero removes that
wait limit but leaves a finite 120-second launch admission window. An expired
request cannot regain authority by reconnecting or retrying. The bundled agent
does not yet enforce these launch fields; see the deployment boundary below.

Reconciliation scheduling gates belong to active tasks in one leadership term
and are not persisted in snapshots. A registration whose Raft outcome is still
unknown keeps the node gated across term changes until its write resolves;
competing registrations receive an ``UNAVAILABLE`` response and may retry. Job
resource ownership and cleanup obligations remain durable in Raft independently
of these temporary gates.
An unresolved Raft write that sets or clears a reconciliation hold also retains
its scheduling gate and exclusive ownership of node reconciliation actions until
the write resolves, including across term changes. A caller timing out does not
allow a newer cut to act ahead of that pending write.

Cleanup and requeue
-------------------

An agent can declare ``runs_job_epilog`` at registration. For a placement on
such a node, a job's final state does not by itself release the node's resource
slice: its completion report must also discharge the cleanup obligation.
A requeue requested while cleanup is outstanding retains the old run's identity
and resources until that obligation is resolved. Agents that omit the capability
retain the existing completion behavior.

The same obligation applies when dispatch only partly succeeds: cancellation
acknowledges a stop request, not completed cleanup. The abandoned reservation
waits in ``REQUEUED`` with its retry or hold intent until node completion or a
fresh complete ledger discharges it. The controller requests those ledgers after
dispatch ends, so a node that never admitted the run need not send a completion.
For such an abandoned run, absence only releases its charge when the inventory
was taken after a confirmed fence, preventing a delayed launch from arriving
after those resources have been reassigned.
The fence covers the committed expiry, even when the new controller's clock is
behind the issuer's. Older reservations without a committed validity window
retain their speculative cleanup charge when only absence is available; they
still accept completion or explicit node removal.
A successful ``SettleRun`` for a finalized run also clears that node's controller
obligation in the same reconciliation pass.

If a finalized run still appears live in the ledger, the controller retries its
fence and cancellation. This recovers a stop request lost during failover while
retaining the resource charge until cleanup finishes. A run still active in
Raft must deliver its completion report before ``SettleRun`` can discharge that
reporting obligation.

Deployment boundary
-------------------

The bundled ``spurd`` currently registers without a ledger and does not declare
the epilog capability. Its ``RequestNodeLedger``, ``FenceRun``, and ``SettleRun``
handlers return ``UNIMPLEMENTED``. The controller preserves existing execution
when an agent does not support these RPCs.

The durable local admission store is present, but launch and allocation paths do
not call it yet. Submitting or running a job therefore creates controller Raft
records and supervisor runtime state, but no local admission record. Agent
admission, ledger reporting, fencing, and settlement enforcement are a separate
integration step. There is no feature flag that enables that missing wiring.
That integration must enforce the committed admission window and run cutoff on
both batch launches and allocation registration. A legacy request without
admission timestamps cannot supply evidence that a particular cutoff has fenced it.

Local admission uses numeric job IDs and run attempts. An independent agent
state root and chunked immutable inventory transfer are separate work.
