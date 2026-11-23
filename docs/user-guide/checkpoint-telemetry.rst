Checkpoint Telemetry
====================

Spur gives a training job an opt-in way to tell it when the job checkpoints or
restores, so operators can see — through cluster metrics — whether jobs
checkpoint, how many checkpoints succeed, and how much work a preemption,
time limit, or cancellation actually lost.

**Spur observes only.** It does not integrate with any training framework,
infer checkpoints on its own, or change scheduling based on these events. A
job that never reports an event behaves exactly as it does today, and
reporting is entirely optional.

How it works
------------

Every ``srun`` step gets its own per-step Unix socket, named in the
``SPUR_CHECKPOINT_SOCK`` environment variable. A job sends one short JSON line
to that socket each time it starts a checkpoint, finishes one, or resumes from
one. Spur attributes every event to the job's own process via kernel-verified
peer credentials — nothing you send can change job, node, or scheduler state,
and reporting is never required for a job to run.

Event format
------------

``SPUR_CHECKPOINT_SOCK`` is set on a job's task environment only when its
step's listener started successfully; it is unset when checkpoint telemetry is
unavailable on the node (see :ref:`checkpoint-telemetry-unavailable` below), so
always check for it before connecting.

Each event is a single line of JSON, at most 512 bytes, terminated by ``\n``:

.. code-block:: text

   {"v": 1, "kind": "checkpoint-begin", "step": 4000, "job_id": "1024"}
   {"v": 1, "kind": "checkpoint-end", "step": 4000, "status": "ok", "job_id": "1024"}
   {"v": 1, "kind": "restore", "step": 4000, "job_id": "1024"}

.. list-table::
   :header-rows: 1
   :widths: 20 20 60

   * - Field
     - Type
     - Meaning
   * - ``v``
     - integer
     - Wire version. Always ``1``.
   * - ``kind``
     - string
     - ``checkpoint-begin``, ``checkpoint-end``, or ``restore``.
   * - ``step``
     - integer
     - The training step the checkpoint (or restore) is at. Required and
       monotonic — a step at or below the last one Spur recorded as complete
       is rejected.
   * - ``status``
     - string
     - Only on ``checkpoint-end``, and required there: ``ok`` or ``failed``.
       Any other event must omit it.
   * - ``job_id``
     - string
     - The job ID, from ``$SPUR_JOB_ID``.
   * - ``step_id``
     - string
     - Optional, and not recommended: the step's own socket already scopes
       every event to that step. If sent, it must match the step the socket
       belongs to and must be numeric, or the event is dropped.

A message carrying anything outside this schema — an unknown ``kind``, an
extra field, a ``status`` on the wrong event, a non-numeric ``step`` — is
rejected whole and counted (see :ref:`checkpoint-telemetry-limits` below). The
protocol allows exactly one event per connection: open the socket, send one
line, read one reply byte, then close. The reply is always a single fixed
byte regardless of whether the event was accepted or dropped — it tells the
client only that the socket round-trip finished, never anything about the
event's fate.

Ways to report events
----------------------

Pick whichever of these fits the job; every one of them is built on the same
one-line-of-JSON protocol described above.

A copy-paste Python helper
~~~~~~~~~~~~~~~~~~~~~~~~~~~

Nothing beyond the Python standard library is required — no Spur package, no
CLI, nothing to install. This is the only surface jobs need toward Spur for
this feature:

.. code-block:: python

   import json, os, socket, time

   _SOCK = os.environ.get("SPUR_CHECKPOINT_SOCK")
   _JOB_ID = os.environ.get("SPUR_JOB_ID")
   _TIMEOUT = 0.05

   def emit(kind, step, status=None):
       if not _SOCK:
           return
       msg = {"v": 1, "kind": kind, "step": step, "job_id": _JOB_ID}
       if status:
           msg["status"] = status
       deadline = time.monotonic() + _TIMEOUT
       try:
           with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
               s.settimeout(max(deadline - time.monotonic(), 0))
               s.connect(_SOCK)
               s.settimeout(max(deadline - time.monotonic(), 0))
               s.sendall(json.dumps(msg).encode() + b"\n")
               s.settimeout(max(deadline - time.monotonic(), 0))
               s.recv(1)
       except OSError:
           pass

``emit`` never raises and never blocks past a single ~50 ms deadline shared
across connect, send, and reply, and swallows every ``OSError``. It is a
no-op — returns immediately without touching the network — when
``SPUR_CHECKPOINT_SOCK`` is not set. Call it with ``"checkpoint-begin"``
before a checkpoint, ``"checkpoint-end"`` with ``status="ok"`` or
``status="failed"`` after, and ``"restore"`` once at startup if you resumed
from an existing checkpoint.

Wrapping any save function
~~~~~~~~~~~~~~~~~~~~~~~~~~~

A context manager built on ``emit`` wraps any save call without repeating the
begin/end pair at every call site:

.. code-block:: python

   import contextlib

   @contextlib.contextmanager
   def checkpointing(step):
       emit("checkpoint-begin", step)
       try:
           yield
           emit("checkpoint-end", step, status="ok")
       except Exception:
           emit("checkpoint-end", step, status="failed")
           raise

   with checkpointing(step):
       save_my_state(step)

A decorator reads the same way when a function already takes the step number
as its first argument:

.. code-block:: python

   import functools

   def checkpointed(fn):
       @functools.wraps(fn)
       def wrapper(step, *args, **kwargs):
           with checkpointing(step):
               return fn(step, *args, **kwargs)
       return wrapper

   @checkpointed
   def save(step):
       save_my_state(step)

PyTorch training loop
~~~~~~~~~~~~~~~~~~~~~

Wrap the save call with ``checkpoint-begin``/``checkpoint-end``, and report a
``restore`` once if the run resumed from an existing checkpoint:

.. code-block:: python

   def save(step):
       emit("checkpoint-begin", step)
       try:
           torch.save(state_dict(), f"{ckpt_dir}/step_{step}.pt")
           emit("checkpoint-end", step, status="ok")
       except Exception:
           emit("checkpoint-end", step, status="failed")
           raise

   step = load_latest(ckpt_dir)
   if step > 0:
       emit("restore", step)
   while step < max_steps:
       train_step(batch())
       step += 1
       if step % 1000 == 0:
           save(step)

Async or distributed checkpointing
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

When the save call itself returns a future rather than blocking until the
checkpoint is on disk, emit ``checkpoint-begin`` when the save starts and
``checkpoint-end`` only once that future actually completes — after a
blocking ``.result()``, or from a done callback if the caller does not want
to block at all:

.. code-block:: python

   def save_async(step, start_checkpoint):
       emit("checkpoint-begin", step)
       future = start_checkpoint(step)

       def _on_done(fut):
           status = "failed" if fut.exception() else "ok"
           emit("checkpoint-end", step, status=status)

       future.add_done_callback(_on_done)
       return future

or, blocking on the result instead of a callback:

.. code-block:: python

   def save_async_blocking(step, start_checkpoint):
       emit("checkpoint-begin", step)
       future = start_checkpoint(step)
       try:
           future.result()
           emit("checkpoint-end", step, status="ok")
       except Exception:
           emit("checkpoint-end", step, status="failed")
           raise

``start_checkpoint`` stands in for whatever asynchronous or distributed save
API the training framework provides; only its return value (something with
``.result()`` or ``.add_done_callback()``) matters here.

Hugging Face Transformers ``Trainer``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

A ``TrainerCallback`` whose ``on_save`` reports each checkpoint ``Trainer``
writes, without touching the training loop itself:

.. code-block:: python

   from transformers import TrainerCallback

   class CheckpointTelemetryCallback(TrainerCallback):
       def on_save(self, args, state, control, **kwargs):
           # Called after Trainer has already saved the checkpoint, so only
           # checkpoint-end is reported here — see "checkpoint-begin is
           # optional" below for what that means.
           emit("checkpoint-end", state.global_step, status="ok")

   trainer = Trainer(..., callbacks=[CheckpointTelemetryCallback()])

or, on an already-constructed ``trainer``:

.. code-block:: python

   trainer.add_callback(CheckpointTelemetryCallback())

``checkpoint-begin`` is optional
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Every example above pairs ``checkpoint-begin`` with ``checkpoint-end``, but
only ``checkpoint-end`` is required: a bare ``checkpoint-end`` with
``status="ok"`` still counts the checkpoint as completed, since applying an
end event does not depend on a matching begin having been applied first.
What a missing ``checkpoint-begin`` costs is visibility while the save is
still running — it is the only event that marks a step ``in_progress``, so a
run killed mid-save with no ``checkpoint-begin`` on record for that step
looks exactly like a run that was simply idle, not one caught mid-checkpoint.
Report ``checkpoint-begin`` when you can; skip it only when wrapping a save
function is impractical, as with the ``Trainer`` callback above.

Shell without Python
~~~~~~~~~~~~~~~~~~~~~

A job with no Python at all — a minimal container image, for example — can
still reach the socket with OpenBSD netcat or ``socat``. Guard on the
environment variable being set and ignore failures, the same way ``emit``
does, so a job never fails because of telemetry:

.. code-block:: bash

   json='{"v":1,"kind":"checkpoint-end","step":4000,"status":"ok","job_id":"'"$SPUR_JOB_ID"'"}'
   [ -n "$SPUR_CHECKPOINT_SOCK" ] &&
     printf '%s\n' "$json" | nc -U -w1 -q1 "$SPUR_CHECKPOINT_SOCK" || true

or with ``socat``:

.. code-block:: bash

   [ -n "$SPUR_CHECKPOINT_SOCK" ] &&
     printf '%s\n' "$json" | socat -t1 - UNIX-CONNECT:"$SPUR_CHECKPOINT_SOCK" || true

Any other language
~~~~~~~~~~~~~~~~~~~~

Nothing above is special beyond the standard library it was written in —
every client speaks the same protocol:

1. Connect a Unix stream socket to ``$SPUR_CHECKPOINT_SOCK``.
2. Write one line of JSON (at most 512 bytes, terminated by ``\n``) matching
   the schema above.
3. Read one reply byte, then close the connection.

Use a short timeout on every step, and ignore every error — telemetry must
never be allowed to affect the job it describes.

Restore on resume
-----------------

Report ``restore`` with the step you resumed from once, early in the run,
whenever the job starts from an existing checkpoint rather than from scratch
(including after a requeue). Spur uses the restore step together with the
checkpoints the previous run completed to show how many training steps a
preemption or failure actually cost, and counts the run on
``spur_job_runs_restored_total`` once it finalizes — see
:doc:`/admin-guide/checkpoint-metrics`.

Which ranks should report
--------------------------

Any rank may call ``emit``; Spur merges events for the same training step
across every rank and node, so reporting from every rank is correct and the
simplest default. For a very wide job where a synchronized checkpoint would
otherwise open one connection per rank per event, emitting only from each
node's local rank 0 cuts that by the number of local ranks with no loss of
information, since every rank checkpoints the same step at the same time.
A rank that finishes a save after another rank has already reported a newer
one is ignored, not counted as an error.

.. _checkpoint-telemetry-unavailable:

When Spur is unavailable
-------------------------

Checkpoint telemetry is best-effort end to end. If the node hardened no
checkpoint socket directory, ``SPUR_CHECKPOINT_SOCK`` is simply never set and
``emit`` is a no-op. If the step's listener is gone, overloaded, or not
accepting connections for any other reason, the client's bounded timeout
fires and the event is silently lost. Either way, the job itself is never
blocked, delayed, or failed because of this feature — only the telemetry is
missing.

Containers
----------

Spur-managed containers get the step's checkpoint socket automatically: Spur
bind-mounts it at the same path inside the container, so
``SPUR_CHECKPOINT_SOCK`` and the helper above work unchanged in a container
job with no extra flag. Only the step's own socket is mounted, never the
whole checkpoint directory. This mount is best-effort and separate from a
job's own ``--container-mounts``: if the image's rootfs cannot take it (for
example, something else already occupies that path), the container still
launches normally, ``SPUR_CHECKPOINT_SOCK`` is simply absent, and ``emit`` is
a no-op, exactly as in :ref:`checkpoint-telemetry-unavailable`. A container a
job starts itself (for example with ``docker``, ``podman``, or
``apptainer``) needs its own bind mount of that path, since those runtimes
move the process out of the step's cgroup that Spur uses to attribute
events.

.. _checkpoint-telemetry-limits:

Limits
------

Every event Spur's listener receives is either applied, dropped, or rejected
as invalid, and every one of the three is counted — nothing is silently
ignored once it reaches the listener. Common reasons a sent event does not
count:

- **Rate limit.** Each step allows a burst of events with a steady refill
  rate; a job that emits far more often than every checkpoint needs will have
  the excess dropped.
- **Oversize.** A line over 512 bytes is rejected as invalid.
- **Mismatched IDs.** A ``job_id``/``step_id`` that does not match the step
  the socket belongs to is rejected as invalid.
- **Invalid schema.** Anything outside the fixed v1 vocabulary (unknown
  ``kind``, extra fields, a ``step`` that is not a non-negative integer, a
  ``status`` in the wrong place) is rejected as invalid.

See :doc:`/admin-guide/checkpoint-metrics` for the full list of dropped and
invalid-event reasons and how to monitor them.
