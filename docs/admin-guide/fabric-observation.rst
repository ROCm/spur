External fabric observations
============================

Spur can report GPU membership in a fabric partition configured by an external
manager. With ``[devices] auto_detect = true`` (the default), ``spurd`` reads
AMD UALink attributes during local device discovery and inventory refresh.
No intermediate inventory file or AFM credentials are required on the agent.
Discovery performs no fabric configuration writes.

Observation is enabled independently of scheduling policy. Administrators can
opt GPU partitions into single-block placement as described below. This mode
uses membership configured by the external manager; it neither reserves a
whole topology block nor verifies AFM readiness or fabric isolation.

Inspecting a node
-----------------

Use ``scontrol show node <node>`` (or ``spur control show node <node>``).
The response includes ``InventoryGeneration``, ``FabricBindingVersion``, ``BlockFileRevision``, and, for GPUs with topology
metadata, full PCI BDF, node-local PCI host bridge, NUMA locality, exact VRAM
bytes, and fabric observations. For example:

.. code-block:: text

   GPUDevice=0 StableId=... PCI=0001:04:00.0 PCIRoot=pci0001:00 NUMA=unknown VRAMBytes=309237645312
      FabricSource=amd-ualink Observation=observed PhysicalDomain=... FabricDomain=15 Endpoint=63 EndpointState=active Transport=UALoE
      PhysicalCapacity=72 FabricCapacity=4 ActiveEndpoints=[60,61,62,63] LocalEndpoints=[60,61,62,63]

``observed`` means the attributes were read consistently. It does not mean
``Ready`` in AFM. Other statuses are ``incomplete`` (missing, malformed, or
unreadable fields), ``inconsistent`` (conflicting or changing observations),
``unavailable`` (the source/device could not be observed), and ``unknown``.
``ObservationIssues`` identifies affected fields and stable diagnostic kinds.
Unknown NUMA locality remains ``unknown`` rather than becoming NUMA node zero.

The displayed data is the controller's last acknowledged inventory, not a
live health query. Use the node state and heartbeat alongside it. The existing
refresh interval and two-observation debounce apply. Membership-only changes
participate in refresh even when device IDs and GPU counts remain constant.

Identity and capacity
---------------------

``PhysicalDomain`` identifies a physical pod; ``FabricDomain`` is a vPod ID
scoped to that physical pod. Equal vPod numbers on different physical pods do
not imply connectivity. The controller scopes domain
identities by the configured ``cluster_name`` administrative namespace. PCI host bridge names and GPU
stable IDs are local to the Spur node.

Capacities and membership count physical fabric endpoints. Logical GPU
partitions may share one endpoint, so endpoint counts cannot be substituted
for schedulable logical-GPU counts. Discovery enumerates active KFD devices;
inactive DRM/XCP entries do not add schedulable GPUs. Exact VRAM comes from
the KFD logical-device inventory. No GPU marketing name or performance rating
is inferred from these fabric attributes.

Discovery boundary
------------------

The AMD adapter reads only these files below each GPU's PCI ``ualink`` directory:

* ``ppod_id``, ``ppod_size``
* ``vpod_id``, ``vpod_size``, ``vpod_active_accels``
* ``local_accels``, ``accel_id``, ``accel_state``, ``link_type``

Reads are bounded and endpoint sets are normalized. Two consecutive snapshots
must agree; local GPU views of the same domain are also checked for conflicts.
This cannot provide an atomic driver snapshot or prevent an external manager
from changing the fabric after discovery. Programming interfaces such as
``config``, ``setup``, and ``stations`` are not accessed. Zero bandwidth or
latency values are not used to invent scheduling costs.

On-disk CDI specs still supply device selection and injection edits. When
auto-detection is enabled, live topology is overlaid for matching AMD render
devices, with explicit PCI BDF and nonzero stable IDs checked against discovery.
A missing match is reported as unavailable. Static specs do not freeze live membership.
With ``auto_detect = false``, CDI metadata is static and updates require
regenerating the specs. GRES-only entries do not acquire topology automatically.

Integration contract
--------------------

``GpuResource.topology`` is optional in persisted inventory and the gRPC API.
Older agents and saved inventories omit it. New agents send typed
``DeviceTopology`` and ``FabricObservation`` messages through registration;
node/resource responses preserve them. Unknown wire status values map to
``unknown``. Existing allocated device identities remain accounted for during
inventory changes, while missing devices' old fabric observations become
unavailable. A full discovery dropout also marks retained declaration-only GPUs
unavailable, even if they previously had no native topology metadata. During
partial loss, a missing held GPU retains its accounting identity with an
unavailable marker. These markers participate in inventory refresh and disable
affected block placement; identity retention does not establish device presence.

The shared model uses vendor-neutral physical/fabric domain references,
endpoint IDs, locality, capacity, observation status, and diagnostics.
``amd-ualink`` is its first discovery source. Internally, CDI carries the
model as JSON in the ``spur.amd.com/topology`` annotation. An invalid annotation
is reported as unavailable rather than silently disappearing. This annotation
is an inventory transport, not a validated controller static-import API.

Typed GPU predicates, per-device static import, segments, topology exclusivity,
and authoritative fabric health are separate capabilities and are not enabled
by these observations. No job flags are added for those capabilities.

Controller block inspection
---------------------------

The controller assembles blocks from explicit GPU membership on each query. Node
ordering, rack size, and the legacy ``topology.plugin = "block"`` setting do not
create fabric membership. A block key consists of the cluster namespace,
physical domain, and fabric domain. Members retain node names, stable logical
GPU IDs, physical endpoint IDs, and inventory generations.

.. code-block:: console

   spur control topology show
   scontrol show topology
   scontrol topology show --json
   scontrol topology export > fabric.json
   scontrol topology validate fabric.json

``show`` includes invalid blocks and GPUs whose membership cannot be resolved.
Diagnostics cover disagreement between nodes, endpoints claimed by multiple
blocks or physical devices, missing registered endpoints, unavailable nodes,
and missing or expired heartbeats. The controller's configured heartbeat timeout
applies (90 seconds by default). A current heartbeat only establishes agent
liveness; it does not establish when the external fabric last changed.

``Membership=consistent`` means the registered reports agree and cover the
reported active endpoints. It does not establish fabric health, available job
capacity, isolation, or a scheduling guarantee. Multiple logical GPU partitions
may share a physical endpoint when they report the same node and PCI identity.

``export`` prints a version 1 JSON candidate membership document only when every
GPU has resolved membership and every block is consistent. The document contains
scoped block keys and exact node/stable-ID pairs. It deliberately contains no
health or availability claims. ``validate`` resolves all declared blocks and
devices against current controller inventory, rejecting duplicates, unknown
identities, namespace mismatches, changed membership, and invalid live blocks.
Unknown JSON fields and unsupported versions are errors; input is limited to
8 MiB. A document can describe a subset of blocks, but each declared block must
contain its complete registered membership.

Validation is read-only. It does not install the document, change membership,
reserve resources, or freeze a fabric configuration. Automatic discovery remains
the default; no administrator-generated intermediate file is required. Installing an optional whole-node definition file is a separate operation,
described below.

Single-block placement policy
-----------------------------

Upgrade every controller before enabling this configuration. Upgrade agents on
the participating nodes and verify ``FabricBindingVersion`` is at least 1 in node inspection;
older agents report zero and cannot satisfy this policy. Add the following to
the controller's existing TOML configuration, using existing partition names:

.. code-block:: toml

   [topology.external]
   single_block_partitions = ["rack"]

The controller reads this policy and rebuilds membership on each scheduling
cycle. Reconfiguration updates the policy without rebuilding a fixed rack list.
``topology show`` includes ``SingleBlockPartitions``. A GPU job requesting any
listed partition, including one in a comma-separated partition list, must bind
its selected GPUs within one consistent block. The default partition follows
the same rule. Ordinary GPU quantity and type options continue to apply to
batch, interactive allocations, and standalone runs. ``--topology`` remains a
locality hint and cannot disable this requirement.

Partition names have no built-in rack meaning: ``rack`` is the existing partition
named in this policy. When no partition is supplied through the CLI, environment,
or batch directives, Spur selects the partition marked ``default = true``. If no
partition is marked default, it selects the first partition in the controller's
list. There is no automatic fallback to ordinary GPU placement when a constrained
job cannot fit. To keep ordinary GPU scheduling as the default, mark a partition
outside ``single_block_partitions`` as default and select ``-p rack`` for jobs
that require containment.

Bare ``--exclusive`` retains whole-node allocation semantics. Under this policy,
every GPU reserved on an exclusive node must belong to the same chosen block;
a node spanning multiple blocks cannot satisfy an exclusive request. CPU-only
jobs without node exclusivity do not require a fabric binding. ``ExclusiveTopo``
is rejected by partition create/update commands because whole-block exclusivity
is not implemented.

For each job, the scheduler considers complete, consistent blocks whose candidate
nodes advertise binding enforcement. It filters each node's inventory to that
block's exact GPU IDs and runs the existing resource and backfill solver. It
chooses the earliest feasible start, breaking block ties by scoped block key.
Node lists, GPU types and quantities, CPU/memory requirements, and reservations
still apply. Selected GPU IDs must remain free throughout the proposed interval.
Future shadow reservations hold those same concrete resources so shorter jobs
can fill gaps without consuming them at the reserved start.

The controller validates the selected block again before recording the placement.
Each per-node allocation persists its block key, active endpoints, exact logical
GPU membership, binding version, and inventory generation. ``scontrol show job``
displays the resulting per-node fabric binding. A failed search reports
``FabricUnavailable`` in the pending reason description.

The agent checks the binding under its allocation lock before admitting the
resources. AMD UALink bindings also reread the allowlisted attributes of the bound
PCI devices. Missing observations, changed domains/endpoints, a changed inventory
generation, or an unsupported binding version reject the launch. Other discovery
sources use their current typed inventory for binding checks. No device or fabric
configuration is written. A supervisor retains the parent binding across an
agent restart, and numbered/interactive steps validate it before executing;
they cannot acquire devices outside the parent allocation. Capable agents start
with a nonzero random inventory epoch, fencing bindings from an earlier agent
instance. An inventory change or agent restart requires requeuing a stale
allocation before new steps can launch.

This is a snapshot-based placement contract. An external manager can still
change a fabric after preflight or while a job is running; operators must
coordinate such changes with running allocations. Observed membership is not
an assertion of fabric readiness, bandwidth, security isolation, or an atomic
fabric lease.

Heterogeneous job groups are left pending under this policy until group-wide
binding is supported. Policy-constrained jobs do not trigger automatic preemption,
idle-fill reclamation, or federation forwarding, whose current selection paths
do not carry fabric requirements. Ordinary backfill, including future resource
reservations, remains available. Segments, whole-block exclusivity, and per-device static import remain future
extensions.

Optional administrator or operator block file
---------------------------------------------

For a rack system whose placement groups are maintained by an administrator or
external operator, set ``block_file`` alongside the partition policy:

.. code-block:: toml

   [topology.external]
   single_block_partitions = ["gpu"]
   block_file = "/etc/spur/topology.conf"

This selects file-defined blocks for the controller. Omitting ``block_file``
keeps automatic discovery. The filename has no special meaning; an absolute
path is required. Install the same configuration setting and file on every
participating agent and controller. Agents must advertise
``FabricBindingVersion`` of at least 2. All nodes can remain in one partition:
``single_block_partitions`` selects which partitions require containment, and
the file defines the placement boundaries within that candidate node pool.
Ordinary partitions retain their existing scheduling behavior.

The file is text, with one record per block:

.. code-block:: text

   # Two groups in one rack system
   BlockName=group-a Nodes=node[01-02]
   BlockName=group-b Nodes=node[03-04]

Each record requires ``BlockName`` and ``Nodes``. Field names are case
insensitive; field order is arbitrary. Node lists support bracket ranges and
comma-separated names. ``#`` begins a comment and a trailing backslash joins
lines. Block names use letters, digits, underscores, dots, and hyphens, with a
maximum of 128 bytes. Node names use the same characters, with a maximum of
255 bytes. Files are limited to 1 MiB and 65,536 declared nodes. Duplicate
fields, block names, or node memberships are errors.

This is a deliberately small subset of the block record format. ``BlockSizes``
and other fields are rejected: higher-level aggregation is not implemented,
and each declared block is a hard placement boundary. A node can belong to
only one declared block. The record covers all currently registered GPUs on
each named node; it cannot describe separate groups within a node. GPU counts,
types, identities, free resources, and health are never taken from this file.
Nodes omitted from the file cannot supply GPUs to constrained jobs, but remain
available to ordinary scheduling.

If the selected GPUs have native fabric observations, the entire declared
block must resolve to one complete, consistent native block. A file can
subdivide a native block by whole nodes. It cannot merge distinct observed
vPods or override incomplete or contradictory observations. If only some GPUs
have observations, missing observations make the declared block unavailable.
Native endpoint coverage still applies, including native members outside a
subdivision.

If none of a declared block's GPUs reports native membership, the file is an
explicit administrator declaration. This path is vendor neutral. Spur enforces
node membership and exact live GPU identities without asserting connectivity,
bandwidth, fabric health, or external-manager readiness. An external operator
is responsible for ensuring its declaration reflects the configured system.
This does not add fabric programming or manager credentials to Spur.

Creating and validating a candidate
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

An administrator can write the records directly. When automatic discovery
already produces whole-node groups, it can provide an initial candidate:

.. code-block:: console

   spur control topology export --format block > topology.conf.candidate
   spur control topology validate --format block topology.conf.candidate

``export --format block`` prints deterministic ``BlockName``/``Nodes`` records.
It preserves names for file-defined groups and generates names such as
``block0`` for automatic groups. It rejects invalid groups, a node shared by
multiple groups, or unresolved GPUs on an included node. Review and validate
the output before installation. It contains no inferred topology hierarchy.

``validate --format block`` checks syntax locally, then resolves membership
against the controller's current live inventory through a read-only RPC. The
original bounded text is sent unchanged, so expansion of a compact hostlist
does not inflate the validation request beyond the file-size limit. Validation
does not require the candidate revision to be deployed to agents, so it can
be used before installation. Missing nodes, stale agents, missing GPUs, or
conflicting native membership are reported as errors. Success does not test
fabric connectivity or install the file. Plain ``export`` and ``validate``
retain the existing JSON format; JSON manifests are inspection/validation
artifacts and are not accepted as ``block_file`` input.

Deployment and updates
~~~~~~~~~~~~~~~~~~~~~~

For first activation, upgrade all controllers and participating agents, install
the validated file at the configured path, and restart agents to load the new
``block_file`` setting. Reconfigure or restart the controller to enable the
setting. A controller can start before nodes register; affected blocks remain
unavailable until complete live inventory and matching revisions arrive.
Reconfigure requires every candidate block to resolve against current
inventory, so wait for registration before using it for activation.

For later membership changes:

#. Generate a complete candidate and run ``topology validate --format block``.
#. Coordinate with running allocations. Membership changes invalidate old
   bindings for new steps; requeue affected allocations as appropriate.
#. Atomically replace the file on participating agents and controllers. Use a
   temporary file in the same directory followed by rename; keep the directory
   readable by the daemon and writable only by the administrator/operator.
#. Run ``spur control reconfigure`` on the controller. It parses and validates
   the complete candidate before changing partitions or the active config.
#. Inspect ``spur control topology show`` and ``spur control show node NODE``.
   Resume constrained submissions once the desired blocks have no issues and
   the participating agents report the expected revision.

There is no automatic controller filesystem watch. Agents reread the file
through the existing inventory refresh/debounce loop and directly before
admission and new steps. The controller retains a parsed immutable snapshot
until reconfigure/restart. A malformed or missing candidate causes reconfigure
to fail while preserving the previous configuration. A missing, malformed, or
different agent file fences that agent's declared block; another valid block
can still schedule. Restore the matching file and wait for inventory refresh
to make the affected block available again.

Revisions hash normalized block names and expanded node memberships. Comments,
whitespace, field order, and record order do not change the revision. Membership
or block-name changes do. During rollout, revision differences deliberately
make affected placement unavailable; there is no fallback to automatic groups.
The revision covers the whole file, so even an edit to another record fences
allocations using an older revision. Plan updates as a coordinated operation.
Step dispatch is not an atomic multi-node barrier: during partial rollout, a
step may start on nodes that still validate the old binding while another node
rejects it. Quiesce affected allocations before changing membership.
Existing running processes are not moved or killed by a file update. A stale
parent allocation must be requeued before it can start further steps; restoring
the old file may still leave a newer inventory generation that fences it.

High-availability deployments follow the existing controller configuration
model: ``reconfigure`` applies on the active leader, and does not replicate
file contents or local configuration to followers. Deploy matching files and
configuration to every controller and restart followers with that configuration
before relying on failover. This feature adds no distributed file rollout or
atomic fabric lease.

Scheduling and inspection
~~~~~~~~~~~~~~~~~~~~~~~~~

File blocks use the same backfill algorithm and exact-device admission checks
as automatic blocks. Earliest feasible start wins; block-key order breaks ties.
A smaller job may share a block when its CPUs, memory, and GPUs are free. A job
cannot combine two blocks even if their summed free GPUs meet its request.
``--exclusive`` still reserves whole nodes, not a whole block.

For four nodes with four GPUs each and the two records above:

.. code-block:: console

   spur submit -p gpu -N 2 -n 2 --gpus 6 -t 00:10:00 --wrap 'sleep 300'
   spur submit -p gpu -N 2 -n 2 --gpus 6 -t 00:10:00 --wrap 'sleep 300'
   spur submit -p gpu -N 4 -n 4 --gpus 12 -t 00:10:00 --wrap 'sleep 300'
   spur control topology show
   spur control show job JOB_ID

The first two jobs can use different eight-GPU blocks. The twelve-GPU job stays
pending because it cannot fit within either block. Omitting ``-p`` uses the
configured default partition and that partition's containment policy.
``topology show`` includes ``Source=file``, ``Revision``, and
``NativeObservation``. Job inspection includes ``BlockFileRevision`` and
``BindingVersion=2`` alongside exact per-node GPU bindings. No new job
ID environment variable or submission flag is required.
