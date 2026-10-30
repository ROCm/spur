GPU Sharing with Kubernetes
===========================

On a :doc:`Spur-managed k0s cluster <managed-kubernetes>`, an enrolled node is
usually reserved for Kubernetes as a whole. Spur does not place jobs on it.

A **shared node** is different. Spur jobs and Kubernetes pods use the GPUs of
the same node at the same time. The split is per GPU and changes with the load.
For example, a node with 8 GPUs can run 3 GPUs of Spur jobs and 5 GPUs of pods.

Spur is the ledger of record for the GPUs of a shared node. A native Spur job
makes no Kubernetes object. Spur reads the allocated claims of the node and
reports their GPUs as **holds**, and does not give a held GPU to a new job. A
pod gets its GPUs from Spur through a bridge: the bridge submits a Spur job for
the pod and gives the GPUs of that job to the pod (see `Bridge contract`_).

Kubernetes does not see the GPUs of native Spur jobs. Only admission control on
the Kubernetes side stops a pod that does not use the bridge from taking such a
GPU (see `Limits`_). Live mode changes can also break the protection; see
`Mark a node as shared`_ and `Failure cases`_.

.. note::

   GPU sharing needs ``[cluster].enabled = true`` and a running
   Spur-managed k0s cluster. A node that you do not mark as shared keeps the
   usual rule: when it is enrolled, it is reserved for Kubernetes.

Contract with the Kubernetes side
---------------------------------

Spur does not install a GPU driver, a DaemonSet or a DeviceClass for
Kubernetes. The contract is one rule:

   A shared node has the AMD DRA driver ``gpu.amd.com``. The driver publishes
   a ``ResourceSlice`` for the node, with the attribute
   ``resource.kubernetes.io/pciBusID`` on each device.

The installer puts the driver on the node. See `Install the DRA driver`_.

Some facts about the driver that Spur uses:

- The device name is ``gpu-<card>-<renderD>``, for example ``gpu-9-136``. The
  pool name is the node name.
- ``pciBusID`` is the PCI address with the domain, for example
  ``0000:2f:00.0``.
- The partitions of a CPX GPU have the ``pciBusID`` of their parent GPU. Only
  the device name is different.

A shared node uses the DRA driver only. It does not use the AMD device plugin.
With DRA, kube-scheduler names the device when it schedules a pod. Thus Spur
knows each hold before the pod starts.

Mark a node as shared
---------------------

You can mark a node as shared when you enrol it, or later. Only a node with the
k0s role ``worker`` or ``single`` can share its GPUs. A control-plane-only node
cannot share.

.. warning::

   Do not enable or disable sharing while GPU workloads run on the node.
   Spur does not check that both schedulers have released their GPUs, and
   does not transfer existing allocations between the device plugin and DRA.
   These are current implementation limits, not automatic drain behavior.

Before enrolment with sharing, or before either mode change:

1. Block new placements on the target node in both Spur and Kubernetes.
   Keep these blocks in place throughout the driver change.
2. Let existing native GPU jobs and Kubernetes GPU pods finish. Verify that
   their processes have stopped and their GPU allocations are released.
   For opt-out, include all Kubernetes DRA consumers.
3. Change the sharing flag and reconcile the driver installation. Follow
   `The node label`_ when the gpu-operator manages the driver.
4. Before you resume placement, verify that only the intended GPU driver
   runs on the node. For opt-in, verify the DeviceClass, ResourceSlice and
   a fresh valid Spur hold report. For opt-out, verify that no old DRA
   consumer remains and that the device plugin is ready.

A running pod allocated by the device plugin has no DRA claim, and removing that
plugin does not remove the pod's GPU access. Publishing those GPUs through
DRA before the old workloads finish can allocate the same GPU twice.

At enrolment, after these prerequisites:

.. code-block:: bash

   spur k8s up --nodes "gpu[01-08]" --gpu-sharing-nodes "gpu[01-02]"
   spur k8s add-nodes --nodes gpu09 --gpu-sharing

``--gpu-sharing-nodes`` is a hostlist. The nodes in it must also be in the
cluster scope. ``--gpu-sharing`` on ``add-nodes`` marks all the added nodes.

On an enrolled node, use one of these commands:

.. code-block:: bash

   spur node gpu-sharing gpu03 on
   spur node gpu-sharing "gpu[03-04]" off
   scontrol update NodeName=gpu03 GpuSharing=yes
   scontrol update NodeName=gpu03 GpuSharing=no

The two commands send the same request. ``spur node gpu-sharing`` also accepts
``yes``/``no`` and ``true``/``false``. These commands need the Administrator
role (see :doc:`../admin-guide/configuration`).

The controller keeps the flag in the Raft log, beside the k0s role. Each
heartbeat response gives the flag to ``spurd``, and ``spurd`` changes the node
to agree with it.

What opt-in does
~~~~~~~~~~~~~~~~

When a node becomes shared, ``spurd`` does these steps on the node, in this
sequence:

1. It makes two kubelet links (see `The kubelet links`_).
2. It sets the Kubernetes node label ``spur.amd.com/gpu-sharing=true`` on its
   own ``Node`` object.
3. It checks that a ``ResourceSlice`` from the driver ``gpu.amd.com`` exists
   for the node.

Until the ``ResourceSlice`` exists, the node is unshareable.
``scontrol show node`` shows the reason. Spur does not place GPU jobs on the
node in this state.

When the node is shared, a GPU job that waits for the node shows a resources
reason. It does not show ``Reserved for Kubernetes cluster``.

What opt-out does
~~~~~~~~~~~~~~~~~

Opt-out changes admission immediately; it does not safely drain the driver:

- The node is reserved for Kubernetes again. Spur places no new jobs on it.
- Spur does not stop existing jobs or pods.
- ``spurd`` sets the node label to ``false`` immediately, stops the claim
  watch and removes the kubelet links.

With the disjoint selectors below, the label change selects the device plugin
instead of DRA. Finish all native GPU jobs and Kubernetes DRA consumers before
opt-out, as described above.

The node label
~~~~~~~~~~~~~~

``spurd`` sets the label ``spur.amd.com/gpu-sharing`` on each GPU node that k0s
enrolls with the role ``worker`` or ``single``. A node that is not shared gets
the label too, but ``spurd`` does not watch its claims:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Node
     - Label value
   * - Shared
     - ``spur.amd.com/gpu-sharing=true``
   * - Enrolled, not shared
     - ``spur.amd.com/gpu-sharing=false``

``spurd`` does not remove the label while the node is enrolled. The label
always has a value, because the node selectors of the AMD gpu-operator 1.5.x
``DeviceConfig`` can only compare for equality. Ordinary GPU nodes select
``"false"``, and shared nodes select ``"true"``.

.. important::

   The AMD gpu-operator does not see a change of the node label on a node
   that it already manages. After you mark a node as shared or not shared,
   restart the gpu-operator controller. First complete the idle-workload
   prerequisites in `Mark a node as shared`_. A delayed operator reconcile
   does not protect live allocations from the driver switch.

   Restart command:

   .. code-block:: bash

      kubectl -n <operator-namespace> rollout restart deployment <operator-controller>

   This applies only when the gpu-operator installs the driver. The Helm chart
   of the driver uses a usual ``nodeSelector``, and Kubernetes applies a label
   change to it automatically.

.. _gpu-sharing-credential:

The Kubernetes credential
~~~~~~~~~~~~~~~~~~~~~~~~~

``spurd`` uses the Kubernetes API to set the label, and to watch claims and
``ResourceSlice`` objects. The credential depends on the k0s role of the node:

- On a node with the role ``single``, ``spurd`` uses the local admin
  kubeconfig (``k0s kubeconfig admin``).
- A node with the role ``worker`` has no admin kubeconfig. ``spurd`` sends the
  controller RPC ``GetGpuSharingKubeconfig`` with its hostname and node token.

The controller uses the heartbeat identity check for this RPC. That check
verifies the node token only when token admission and a signing key or signer
are both available. Token admission alone does not establish this protection.
See :doc:`../admin-guide/configuration` for admission and signing settings.

The requested node must be registered, have GPUs, and have the k0s role
``worker`` or ``single``. It does not have to be shared, because a node that
is not shared also sets its label. The controller refuses a node whose
Kubernetes node name (the lowercase hostname) is not a DNS-1123 label.

The controller then sends ``GetKubeconfig`` with ``gpu_sharing_node`` to a
control-plane ``spurd``. That ``spurd`` applies these objects and then makes a
bound token of 24 hours for the ServiceAccount:

.. list-table::
   :header-rows: 1
   :widths: 35 65

   * - Object
     - Contents
   * - Namespace ``spur-system``
     - Holds the ServiceAccounts.
   * - ServiceAccount ``spur-system/spurd-gpu-sharing-<node>``
     - The identity of the worker.
   * - ClusterRole and ClusterRoleBinding ``spurd-gpu-sharing-<node>``
     - ``get``, ``list`` and ``watch`` on ``resourceclaims`` and
       ``resourceslices`` (``resource.k8s.io``). ``get`` and ``patch`` on the
       ``Node`` of this node only.

``spurd`` does not log the token or the kubeconfig. It makes a new client
every 12 hours, and immediately when the API server answers HTTP 401.

.. warning::

   Credential issuance does not currently fail closed when node identity
   enforcement is unavailable. With ``admission.mode = "open"``, each client
   that can connect to the controller can obtain a credential for an eligible
   GPU node. The same check is bypassed when no signing key or signer is
   available. Use token admission with a working signer before deployment.

   This credential can read all claims and slices of the cluster, and can
   change the ``Node`` object of its node, which includes its taints. A
   firewall can limit access, but does not correct the credential issuance
   defect.

Opt-out and node removal do not delete the ServiceAccount or its RBAC objects.
An administrator must remove these objects when the node no longer needs
Kubernetes access. Do not remove them while the agent still needs them for
node-label updates. Token rotation is not revocation.

The kubelet links
~~~~~~~~~~~~~~~~~

The kubelet of k0s uses the root directory ``/var/lib/k0s/kubelet``. Most
Kubernetes components, and the DRA DaemonSet of the gpu-operator, expect
``/var/lib/kubelet``. On a shared node ``spurd`` makes two symbolic links:

.. code-block:: text

   /var/lib/kubelet/plugins_registry -> /var/lib/k0s/kubelet/plugins_registry
   /var/lib/kubelet/plugins          -> /var/lib/k0s/kubelet/plugins

The kubelet finds a DRA plugin through the registration socket in
``plugins_registry``. Then it connects to the socket path that the plugin
gives. With the links, the plugin registers at the default paths.

``spurd`` does not link the full ``/var/lib/kubelet`` directory. On k0s the
kubelet makes a real ``/var/lib/kubelet/device-plugins`` directory, so
``/var/lib/kubelet`` is always a real directory.

The guard: ``spurd`` makes a link only when the path does not exist, or when
the path is already the same link from Spur. When a different file, directory
or link is at the path, ``spurd`` does not change it. The node then stays
unshareable, and the reason names the path.

``spur k8s down --reset`` removes the two links. It removes only the links from
Spur, and nothing else in ``/var/lib/kubelet``.

Install the DRA driver
----------------------

Spur does not install the DRA driver. Use one of the two procedures that
follow. In each procedure, the driver must run only on shared nodes. The node
needs a loaded ``amdgpu`` kernel driver and a CDI-enabled container runtime.
Complete the workload transition prerequisites before replacing a device
plugin with DRA.

Also install the DeviceClass ``gpu.amd.com`` with the extended resource
mapping (see `The DeviceClass`_). Then a pod that requests ``amd.com/gpu``
gets a GPU on a shared node, with no change to the pod.

The DeviceClass
~~~~~~~~~~~~~~~

.. code-block:: yaml

   apiVersion: resource.k8s.io/v1
   kind: DeviceClass
   metadata:
     name: gpu.amd.com
   spec:
     extendedResourceName: amd.com/gpu
     selectors:
       - cel:
           expression: "device.driver == 'gpu.amd.com'"

The chart of the driver v1.0.1 and the gpu-operator chart 1.5.1 make the same
class without ``extendedResourceName``. Give the class one owner: set
``deviceClass.create=false`` in the driver chart, or
``draDriver.deviceClass.create=false`` in the gpu-operator chart, and apply
the class above from your own chart or GitOps repository. A manual patch of a
class that a chart owns can go away at the next sync of the chart.

The mapping needs the Kubernetes feature gate ``DRAExtendedResource`` in the
API server, the scheduler, the controller manager and the kubelet. It is beta
and on by default in Kubernetes 1.36. In 1.34 and 1.35 it is alpha and off by
default. The k0s v1.36.2 of Spur has the gate on. To check a component, read
its metric ``kubernetes_feature_enabled{name="DRAExtendedResource"}``.

With the Helm chart of the driver
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The chart is a release asset of
`ROCm/k8s-gpu-dra-driver <https://github.com/ROCm/k8s-gpu-dra-driver>`_,
release ``v1.0.1``. Set ``deviceClass.create=false``, because the class of
the chart has no extended resource mapping (see `The DeviceClass`_).

.. code-block:: bash

   helm install amd-gpu-dra \
       https://github.com/ROCm/k8s-gpu-dra-driver/releases/download/v1.0.1/k8s-gpu-dra-driver-v1.0.1.tgz \
       --namespace kube-amd-gpu --create-namespace \
       --set image.tag=v1.0.1 \
       --set deviceClass.create=false \
       --set-string 'kubeletPlugin.nodeSelector.spur\.amd\.com/gpu-sharing=true'

Set ``image.tag`` to pin the image. The release chart uses its ``appVersion`` when the tag is empty.

With the kubelet links, the default kubelet paths of the chart are correct. If
you do not want to use the links, set the k0s paths. These values are
optional:

.. code-block:: bash

   --set kubeletPlugin.kubeletRegistrarDirectoryPath=/var/lib/k0s/kubelet/plugins_registry \
   --set kubeletPlugin.kubeletPluginsDirectoryPath=/var/lib/k0s/kubelet/plugins \
   --set cdi.dynamicPath=/var/run/cdi

With the AMD gpu-operator 1.5.x
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The gpu-operator 1.5.0 is the first release with ``spec.draDriver``. The
operator does not accept one ``DeviceConfig`` that enables both the device
plugin and the DRA driver. Thus, use two ``DeviceConfig`` objects with node
selectors that do not overlap:

- One ``DeviceConfig`` with the device plugin, for ordinary GPU nodes
  (``spur.amd.com/gpu-sharing: "false"``).
- One ``DeviceConfig`` with the DRA driver, for shared nodes
  (``spur.amd.com/gpu-sharing: "true"``).

The examples also require ``feature.node.kubernetes.io/amd-gpu=true`` on
each GPU node. Verify this label; virtual-function hosts can lack automatic
GPU discovery. Confirm the hardware before an administrator adds the label.
A node without the required labels matches neither DeviceConfig.

The default image tag of the DRA driver in the operator is ``latest``. Always
set a fixed tag.

.. code-block:: yaml

   apiVersion: amd.com/v1alpha1
   kind: DeviceConfig
   metadata:
     name: gpu-device-plugin
     namespace: kube-amd-gpu
   spec:
     selector:
       feature.node.kubernetes.io/amd-gpu: "true"
       spur.amd.com/gpu-sharing: "false"
     devicePlugin:
       enableDevicePlugin: true
     # Other fields as in your current DeviceConfig.
   ---
   apiVersion: amd.com/v1alpha1
   kind: DeviceConfig
   metadata:
     name: gpu-dra-driver
     namespace: kube-amd-gpu
   spec:
     selector:
       feature.node.kubernetes.io/amd-gpu: "true"
       spur.amd.com/gpu-sharing: "true"
     devicePlugin:
       enableDevicePlugin: false
     draDriver:
       enable: true
       image: docker.io/rocm/k8s-gpu-dra-driver:v1.0.1
       selector:
         spur.amd.com/gpu-sharing: "true"

The DRA DaemonSet of the operator uses the fixed paths ``/var/lib/kubelet/...``.
It works on k0s only because of `The kubelet links`_.

Remember to restart the operator after a label change (see `The node label`_).

CDI files
~~~~~~~~~

The CDI specifications do not collide. ``spurd`` writes the kind
``amd.com/gpu`` into ``/etc/cdi/amd.json``. The DRA driver writes the kind
``k8s.gpu.amd.com/gpu`` into ``/var/run/cdi``. The containerd of k0s reads both
directories.

``spurd`` also reads ``/var/run/cdi``, but it ignores each CDI kind whose
vendor starts with ``k8s.``. DRA drivers use such a vendor for the
specifications that they write for each allocated claim. These devices belong
to a pod, and are not in the inventory of the node.

Bridge contract
---------------

A pod gets GPUs of a shared node through a bridge. The bridge is not part of
Spur. It uses only the public gRPC API of Spur: ``SubmitJob``, ``GetJobs``,
``GetJob``, ``GetNode`` and ``CancelJob``. Spur expects this sequence from it:

1. The bridge submits one Spur job for the pod. The job only waits, for
   example ``sleep infinity``. The job runs on node X with the GPUs S. The
   stable ids of S are in ``JobInfo.resources.devices["gpu"]``. ``GetNode``
   gives the DRA device name of each GPU in ``gpu_holds.gpus[].dra_device``.
2. The bridge writes the allocation of the pod's ``ResourceClaim`` for the
   devices of S. It sets the claim label ``spur.amd.com/job-id`` to the id of
   the job.
3. The bridge waits until ``GetNode`` for X shows each GPU of S as held by the
   claim, with the id of the job.
4. The bridge reads the job again. If the job does not run, the bridge deletes
   the claim and stops. If the job runs, the bridge lets the pod start, for
   example with the removal of a scheduling gate.
5. When the job ends, the bridge deletes the pod. When the pod ends, the
   bridge cancels the job.

If the job ends before step 3, the bridge sees it in step 4, and the pod does
not start. If the job ends after step 3, the hold keeps the GPUs busy until
the pod stops and DRA releases the claim.

The controller uses the claim label as follows:

- While the job holds the allocation of a GPU, the hold of the pod on that GPU
  is part of the job. The scheduler counts the GPU once, as the allocation of
  the job. Thus preemption and idle-fill reclaim can stop the job to free the
  GPU.
- After the job ends, the hold counts again, until the claim goes. This is the
  fence between the stopping pod and the next job. It does not depend on how
  the job ended: cancel, preemption, requeue, time limit, a node that goes
  down, a ``spurd`` restart and a controller leader change all keep the GPU
  held.
- A pending job that preempted the job does not preempt more jobs while the
  hold of the pod is on a node that the pending job can use.

A claim without the label is a hold for as long as it is allocated. On a GPU
that a Spur job uses, a claim that does not name that job is a conflict.
Spur does not check who wrote the label. Only the bridge must write claim
allocations and the label on shared nodes (see `Limits`_).

How Spur sees pod GPUs: holds
-----------------------------

``spurd`` watches the ``ResourceClaim`` objects and the ``ResourceSlice`` of
its node. Each GPU that an allocated claim of a pod names is a **hold**. A
hold starts when the claim is allocated, before the pod starts. The claim
label ``spur.amd.com/job-id`` names the Spur job of the hold (see
`Bridge contract`_).

``spurd`` sends the full hold state of the node in each heartbeat, every 30
seconds. When the state changes, it sends one more heartbeat immediately. The
controller keeps the holds in memory only, not in the Raft log. After a
controller leader change or an agent restart, the next heartbeat sends the
holds again.

The controller gives no GPU of a shared node to a Spur job when:

- It has no report from the node yet.
- The last report is older than ``controller.heartbeat_timeout_secs``
  (90 seconds if not set).
- The report is for an old GPU inventory of the node.
- The report says that the node is unshareable.

Jobs that use only CPUs can go to a shared node in these conditions.

When the report is valid, the controller does not give a GPU that a pod holds,
that is in conflict, or that is unshareable. Backfill thinks that a held GPU
is busy for all time, because a pod has no end time. A hold that is part of a
running job is busy until the end of the job.

See the GPU owners
------------------

``scontrol show node`` (and ``spur show node``) shows ``GpuSharing=yes`` or
``GpuSharing=no``. On a shared node it also shows one line for each GPU:

.. code-block:: text

   NodeName=gpu01
      State=MIXED Reason=
      ...
      Gres=gpu:mi300x:1,gpu:mi300x:1,gpu:mi300x:1,gpu:mi300x:1
      GpuSharing=yes
      Gpu=0x1b0000 Device=gpu-1-128 State=free
      Gpu=0x2f0000 Device=gpu-9-136 State=job 1042
      Gpu=0x3a0000 Device=gpu-13-140 State=job 1044 held aim/llama-2 (claim llama-2-gpu)
      Gpu=0x4e0000 Device=gpu-17-144 State=held aim/llama-0 (claim llama-0-gpu)
      Gpu=0x620000 Device=gpu-25-152 State=conflict job 1043 vs aim/llama-1

``Gpu`` is the stable id of the GPU in Spur, in hexadecimal. ``Device`` is the
DRA device name, or ``-`` when the GPU has none. ``State`` is one of:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - State
     - Meaning
   * - ``free``
     - No allocated claim holds the GPU, and no Spur job uses it. This is not
       proof that no process uses it: an old device-plugin pod is not in this
       ledger.
   * - ``job <id>``
     - Spur job ``<id>`` uses the GPU. No claim holds it.
   * - ``job <id> held <ns>/<pod> (claim <name>)``
     - The claim ``<name>`` of the pod ``<ns>/<pod>`` holds the GPU for Spur
       job ``<id>``. While the job holds the GPU, the GPU is part of the job.
       After the job ends, the GPU stays held until the claim goes.
   * - ``held <ns>/<pod> (claim <name>)``
     - The claim ``<name>`` of the pod ``<ns>/<pod>`` holds the GPU, and names
       no Spur job.
   * - ``conflict job <id> vs <ns>/<pod>``
     - Spur job ``<id>`` uses the GPU, but the claim of the pod holds the GPU
       and does not name job ``<id>``. See `Failure cases`_.
   * - ``unshareable (<reason>)``
     - The GPU is not in the Kubernetes ledger, for example
       ``no DRM card for renderD130``. Spur does not place jobs on it.

Other lines:

- ``GpuHolds=(no report)``: the node is shared, but the controller has no hold
  report from it yet.
- ``GpuUnshareable=<reason>``: the full node cannot share, for example when no
  ``ResourceSlice`` from ``gpu.amd.com`` exists for the node.

``sinfo`` shows only GPU counts in its GRES columns. Hold telemetry is stored
on the controller leader; a node query answered by a follower can omit it.
Do not treat missing telemetry as proof that GPUs are free.

Run a pod on a shared node
--------------------------

A pod gets GPUs of a shared node through the bridge (see `Bridge contract`_).
The bridge decides which pods and which claims it serves. A pod that it
serves waits in the Spur queue as a Spur job, and starts when Spur gives GPUs
to that job.

A pod that kube-scheduler allocates without the bridge is a hold with no Spur
job: ``scontrol show node`` shows ``held <ns>/<pod> (claim <name>)``, and
Spur does not use its GPUs while the claim is allocated. But kube-scheduler
does not see the GPUs of native Spur jobs. Thus such a pod can get a GPU that
a Spur job uses, which is a conflict. On shared nodes, let only the bridge
allocate GPUs (see `Limits`_).

Validation scope
----------------

Single-node SPX testing with eight MI325X virtual-function GPUs, k0s 1.36.2,
DRA driver 1.0.1 and a bridge covered:

- The GPU in the pod has the PCI address of the GPU that Spur allocated to the
  bridge job.
- Spur jobs and bridge pods on different GPUs, in both start orders, and
  waiting for a free GPU.
- Preemption, ``scancel`` and ``scontrol requeue`` of a bridge job, and
  ``scancel`` while ``spurd`` was stopped, with a pod grace period of 60
  seconds. In each case, the next Spur job got a GPU of the pod only after
  DRA released the claim.

The test pods had the scheduling gate and the claim in their manifest. A
mutating admission policy that adds them made kube-apiserver 1.36.2 panic.
These results cover this configuration, not every supported topology.

The following checks remain pending:

- Pods that request ``amd.com/gpu``, and AIM inference, through a bridge.
- CPX partition identity and allocation on hardware.
- Multi-node operation and the worker-credential path.
- A real ArgoCD deployment and migration of an existing chart-owned
  DeviceClass to the new owner.
- Kubernetes 1.34 and 1.35 with the alpha extended-resource gate enabled.
- Idle-fill reclaim with GPU holds on hardware. Reclaim counts the held GPUs
  of a shared node as allocated; unit tests cover this.

Limits
------

- Holds apply to GPUs only. Spur jobs and pods can use more CPU and memory
  than the node has. Spur does not prevent this.
- You cannot make an advance reservation on a shared node. The request fails
  with an error.
- Do not change the partition mode (for example SPX to CPX) of a GPU while the
  node is shared. The unit of sharing is one KFD device.
- Use one Spur agent for each node. The ``spur-k8s`` operator mode must not
  also register a shared node.
- Kubernetes does not see the GPUs of native Spur jobs: the ``ResourceSlice``
  shows them as free. Spur makes no Kubernetes object for them. Use admission
  on the Kubernetes side, for example a taint on shared nodes and a
  ``ValidatingAdmissionPolicy`` on claim allocations, so that only the bridge
  allocates GPUs of a shared node. Spur shows a pod that gets past it as a
  conflict, but does not stop it.
- Spur preemption and idle-fill reclaim can stop the Spur job of a bridge pod.
  The bridge then deletes the pod. Spur does not stop a pod that the bridge
  does not serve.

Failure cases
-------------

.. list-table::
   :header-rows: 1
   :widths: 35 65

   * - Case
     - Result
   * - A GPU is in conflict, for example because a pod got the GPU of a native
       Spur job without the bridge.
     - ``scontrol show node`` shows ``conflict`` and the reason. The job gets
       no comment. The controller gives the GPU to no new job. Spur stops no job and no
       pod. The administrator decides what to do.
   * - ``spurd`` cannot connect to the API server.
     - ``spurd`` sends a report that says that the node is unshareable, or no
       report. The controller gives no GPU of the node to a new job: at once
       for an unshareable node, and else after the heartbeat timeout.
   * - A worker cannot get its credential, for example because no
       control-plane ``spurd`` answers.
     - The same as when ``spurd`` cannot connect to the API server. On a
       node that is not shared, the label stays unset until the next try.
   * - ``spurd`` restarts.
     - ``spurd`` gets the holds again from the claim watch, then sends a
       report.
   * - A claim is allocated, but its pod does not start.
     - The hold starts at allocation, so Spur does not use the GPU.
   * - No ``ResourceSlice`` from ``gpu.amd.com`` exists for the node.
     - The node is unshareable, with the reason in ``scontrol show node``.
   * - The node has a DRM card that is not AMD, for example the virtual
       display of a cloud VM.
     - The DRA driver v1.0.1 reads the driver version of the first card in
       ``/sys/class/drm``. If that card is not AMD, the version can be a
       value such as ``1``, which is not semantic versioning. The API server
       then refuses each ``ResourceSlice``, and the driver log shows
       ``must be a string compatible with semver.org``. The node stays
       unshareable. Remove the other card from DRM, for example with
       ``modprobe -r virtio_gpu``.
   * - A node had the AMD device plugin before it became shared.
     - The kubelet keeps ``amd.com/gpu`` in the node status, first with
       allocatable ``0``, and after approximately 5 minutes with capacity
       ``0``. kube-scheduler ignores it, because the DeviceClass maps
       ``amd.com/gpu``. aim-engine v0.2.6 does not: it finds no supported
       AIM profile for the node, and makes no InferenceService. After the
       5 minutes, remove the two fields with
       ``kubectl patch node <node> --subresource=status --type=json``
       and the operations ``remove`` on ``/status/capacity/amd.com~1gpu``
       and ``/status/allocatable/amd.com~1gpu``. The kubelet does not add
       them again while no device plugin registers ``amd.com/gpu``. This
       corrects AIM discovery only; it does not transfer live GPU allocations.
   * - ``spur k8s down --reset`` reports ``down``.
     - The agents can still be resetting k0s. Verify that reset has completed
       on each node before stopping the Spur daemons. The reported phase alone
       is not a completion check.
   * - A foreign file or directory is at a kubelet link path.
     - ``spurd`` does not change it. The node is unshareable, and the reason
       names the path.
