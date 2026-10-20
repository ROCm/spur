External fabric observations
============================

Spur can report GPU membership in a fabric partition configured by an external
manager. With ``[devices] auto_detect = true`` (the default), ``spurd`` reads
AMD UALink attributes during local device discovery and inventory refresh.
No intermediate inventory file or AFM credentials are required on the agent.
Discovery performs no fabric configuration writes.

This release reports observations. It does **not** enforce single-vPod job
placement, reserve a whole topology block, or verify AFM readiness. Existing
GPU scheduling policy remains in effect. Do not use an observed vPod as a
job containment or security guarantee.

Inspecting a node
-----------------

Use ``scontrol show node <node>`` (or ``spur control show node <node>``).
The response includes ``InventoryGeneration`` and, for GPUs with topology
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
not imply connectivity. A future controller block view must also scope domain
identities by an administrative namespace. PCI host bridge names and GPU
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
unavailable.

The shared model uses vendor-neutral physical/fabric domain references,
endpoint IDs, locality, capacity, observation status, and diagnostics.
``amd-ualink`` is its first discovery source. Internally, CDI carries the
model as JSON in the ``spur.amd.com/topology`` annotation. An invalid annotation
is reported as unavailable rather than silently disappearing. This annotation
is an inventory transport, not a validated controller static-import API.

Controller block assembly, policy configuration, static import/validation,
typed GPU predicates, mandatory containment, and pre-launch fabric binding
validation are subsequent implementation stages. No new job flags are accepted
for those unimplemented guarantees.
