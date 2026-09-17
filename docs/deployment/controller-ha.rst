Controller high availability
============================

``spurctld`` replicates its state with Raft. Every deployment runs a Raft
cluster, even a single controller, which is a cluster of one member. This page
tells how to add a controller to a running cluster and how to remove one,
without a state wipe and without downtime.

.. contents::
   :local:
   :depth: 1

How membership works
--------------------

``controller.peers`` in ``spur.conf`` is the member list of the **first**
bootstrap only. After that it is a seed list: a starting controller asks the
addresses in it whether a cluster already runs. The true member list is the
membership that Raft replicates in its own log.

A controller that you add at runtime therefore does **not** have to appear in
``controller.peers`` of the other controllers. The leader learns its address
from the membership entry.

A member is one of two kinds:

**Learner**
   Receives the replicated log but does not vote. A new controller is always a
   learner first, while it catches up.

**Voter**
   Counts in the quorum and can become leader.

Add a controller
----------------

1. Install the new controller as usual and give it an id that no member uses.
   Set it explicitly, because the node is not in ``controller.peers``:

   .. code-block:: toml

      [controller]
      node_id = 4
      peers = ["ctrl1:6821", "ctrl2:6821", "ctrl3:6821"]

   Give it the same ``peers`` as the others. The list lets the new controller
   find the cluster and see that it is not yet a member.

   In Kubernetes every replica reads the same configuration, so an explicit id
   is not possible. There it is not necessary either: a StatefulSet replica
   whose hostname ordinal is past the end of ``controller.peers``, such as
   ``spurctld-3`` with three peers, takes that ordinal as its id. Raise the
   replica count and the new Pod gets id 4 on its own.

2. Start it. It finds the cluster, does not bootstrap, and waits. While it
   waits it does not serve the client API, and ``/readyz`` on the health port
   answers 503, so a load balancer or a Kubernetes Service does not send work
   to it. ``/livez`` answers 200, because the process is alive. The log says:

   .. code-block:: text

      peer ctrl1:6821 runs a cluster that does not list node 4; not bootstrapping

3. Add it as a learner. Give the Raft address, which is the
   ``controller.raft_listen_addr`` port, 6821 by default:

   .. code-block:: bash

      spur admin raft add-learner 4 ctrl4:6821

4. Wait until it has caught up. Compare ``MATCHED`` with the leader's
   ``last_log_index``:

   .. code-block:: bash

      spur admin raft status

   .. code-block:: text

      answered by node 1 (Leader), leader 1, last_log_index 1284
      NODE     ADDRESS                        ROLE     MATCHED
      1        ctrl1:6821                     voter    1284
      2        ctrl2:6821                     voter    1284
      3        ctrl3:6821                     voter    1284
      4        ctrl4:6821                     learner  1284

   ``MATCHED`` is how far the leader has replicated to that member. Only the
   leader keeps this figure, so ask the leader; another member prints ``-``.

5. Promote it to a voter:

   .. code-block:: bash

      spur admin raft promote 4

   ``promote`` refuses a learner that the leader has not reached yet, and one
   that is more than 5000 entries behind ``last_log_index``. That figure is
   openraft's replication lag threshold, past which a member is served by
   snapshot; a voter that far behind could stall every write until it caught
   up. Wait, look at ``status`` again, and run the command again. ``promote``
   also refuses when a voter does not answer; see the note below.

.. important::

   Every controller must run a build that supports dynamic membership before
   you promote. An older voter cannot reach a node that only the replicated
   membership names, so it would lose the new member as soon as it became
   leader. ``promote`` asks each voter and refuses if one is too old, or if
   one does not answer at all: a voter that is gone must be removed before the
   quorum grows around it. Update the controllers first, remove a dead voter,
   then promote. ``add-learner`` does not make this test, because a learner
   is not in the quorum.

Kubernetes
----------

Three things in the manifests decide whether a scale-up can work at all. All
are set in ``examples/k8s/spurctld.yaml``.

**The headless Service needs** ``publishNotReadyAddresses: true``.
A controller that waits for its membership is not ready, and DNS leaves out an
unready Pod. Without this field the leader cannot resolve the new Pod, so the
Pod never becomes a member, so it never becomes ready. Nothing in the log says
this; the learner simply stays at ``MATCHED -`` for ever.

**The liveness probe must watch** ``/livez`` **on the health port, not the
client port.** A waiting controller serves Raft and the health port but not
the client API. A liveness probe on 6817 kills the Pod about a minute after it
starts, before an operator can add it. ``/livez`` answers while the node waits,
and ``/readyz`` does not, which is what the two probes need.

To see that a waiting Pod gets no client traffic, read the endpoints of the
two Services: ``kubectl get endpoints spurctld-client`` must not name the Pod,
and ``kubectl get endpoints spurctld`` must, because peer DNS has to answer
for it.

**Clients should use the** ``spurctld-client`` **Service.** The headless Service
now answers for waiting Pods too, so it no longer keeps work away from a
controller that is not ready. ``spurctld-client`` is readiness-gated and does.
The client ports stay on the headless Service as well, so an older deployment
that points at ``spurctld`` keeps working.

Scale the StatefulSet up by one, then add and promote the new Pod as above. Its
id comes from the hostname ordinal, so ``spurctld-3`` with three peers is
node 4.

After a ``remove``, delete the Pod's volume before you scale back up:

.. code-block:: bash

   kubectl delete pvc spool-spurctld-3 -n spur

A StatefulSet keeps the volume when it scales down. A Pod that comes back with
the old volume reads a membership that still lists it and serves that stale
state, exactly as a restarted removed controller does.

Remove a controller
-------------------

.. code-block:: bash

   spur admin raft remove 4

``remove`` takes a voter or a learner, and refuses an id that is not a member.
The node leaves the membership completely. Keep an odd number of voters, and
remember that a quorum is a majority of the voters: three voters tolerate one
failure, five tolerate two. Remove the member from the cluster before you stop
its process, so the quorum never counts a member that is gone.

Stop the removed controller afterwards, and keep it stopped. A removed node
cannot be told that it left: the cluster commits the removal without it. Until
you stop it, it still answers the client API with the state it last replicated,
which grows more stale every minute, and it never accepts a write, because it
can no longer elect a leader. The same holds if you start it again later: it
reads its last membership from disk, still finds itself in it, and serves that
stale state. To use the machine as a controller again, wipe its state directory
first and add it as a new learner.

Read the status
---------------

.. code-block:: bash

   spur admin raft status

``answered by node N (state)``
   Which controller answered, and its Raft role: ``Leader``, ``Follower``,
   ``Candidate`` or ``Learner``.

``leader``
   The node id of the leader, or ``none`` during an election.

``MATCHED``
   Highest log index replicated to that member, from the leader's view. A
   learner is ready for a promote when its ``MATCHED`` is close to
   ``last_log_index``.

Permissions
-----------

The three mutations need an operator: a cluster admin credential, or a caller
that ``root`` or a member of ``sudo``/``wheel`` runs. This is stricter than the
ordinary admin check, which lets an unidentified caller through in the default
``permissive`` authentication mode. ``status`` is a read and needs no operator.

Limits
------

- The operator gives the node id. The controller does not allocate one.
- ``add-learner`` refuses an id that is already a member. To change the
  address of a member, remove it and add it again.
- There is no option to force a promote past the version test, the lag test,
  or a voter that does not answer.
- The methods are gRPC and CLI only. They have no REST endpoint.
