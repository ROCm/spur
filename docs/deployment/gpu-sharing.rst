GPU Sharing with Kubernetes
===========================

On a :doc:`Spur-managed k0s cluster <managed-kubernetes>`, an enrolled node is
usually reserved for Kubernetes as a whole. Spur does not place jobs on it.

A **shared node** is different. Spur jobs and Kubernetes pods use the GPUs of
the same node at the same time. The split is per GPU and changes with the load.
For example, a node with 8 GPUs can run 3 GPUs of Spur jobs and 5 GPUs of pods.
Claims coordinate new allocations so that a Spur job and a pod use different
GPUs. This protection depends on all GPU consumers having claims and on the
claims remaining allocated while their workloads run. Live mode changes and
lost placeholders can break that protection; see `Mark a node as shared`_
and `Failure cases`_.

Kubernetes is the ledger of record for the GPUs of a shared node. Before a
new GPU job starts, Spur reserves its GPUs with a placeholder pod and claim.
Spur also reads allocated application claims and reports their GPUs as holds.

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
   For opt-out, include Kubernetes DRA consumers, not only Spur placeholders.
3. Change the sharing flag and reconcile the driver installation. Follow
   `The node label`_ when the gpu-operator manages the driver.
4. Before you resume placement, verify that only the intended GPU driver
   runs on the node. For opt-in, verify the DeviceClass, ResourceSlice and
   a fresh valid Spur hold report. For opt-out, verify that no old DRA
   consumer remains and that the device plugin is ready.

An existing native job gets no placeholder retroactively. A running pod
allocated by the device plugin also has no DRA claim, and removing that
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
- ``spurd`` sets the node label to ``false`` immediately and stops the claim
  watch and live-placeholder presence checks.
- Cleanup of tracked placeholders continues as jobs finish. The kubelet links
  stay while tracked placeholders remain.

With the disjoint selectors below, the label change selects the device plugin
instead of DRA. Keeping the links does not keep the DRA driver running, and a
missing placeholder is not repaired during opt-out. Finish all native GPU jobs
and Kubernetes DRA consumers before opt-out, as described above.

The node label
~~~~~~~~~~~~~~

``spurd`` sets the label ``spur.amd.com/gpu-sharing`` on each GPU node that k0s
enrolls with the role ``worker`` or ``single``. A node that is not shared gets
the label too, but ``spurd`` does not watch its claims or make placeholders:

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

``spurd`` uses the Kubernetes API to set the label, to watch claims and
``ResourceSlice`` objects, and to make placeholders. The credential depends on
the k0s role of the node:

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
     - Label ``pod-security.kubernetes.io/enforce=baseline``. Thus a pod in it
       cannot be privileged and cannot mount a host path.
   * - ServiceAccount ``spur-system/spurd-gpu-sharing-<node>``
     - The identity of the worker.
   * - ClusterRole and ClusterRoleBinding ``spurd-gpu-sharing-<node>``
     - ``get``, ``list`` and ``watch`` on ``resourceclaims`` and
       ``resourceslices`` (``resource.k8s.io``). ``get`` and ``patch`` on the
       ``Node`` of this node only. ``get`` on the namespace ``spur-system``
       only.
   * - Role and RoleBinding ``spur-system/spurd-gpu-sharing-<node>``
     - ``create``, ``get``, ``list``, ``watch``, ``delete`` and ``patch`` on
       all ``resourceclaims`` and ``pods`` in ``spur-system``, including
       placeholders of other nodes. These permissions are not node-scoped.

``spurd`` does not log the token or the kubeconfig. It makes a new client
every 12 hours, and immediately when the API server answers HTTP 401.

.. warning::

   Credential issuance does not currently fail closed when node identity
   enforcement is unavailable. With ``admission.mode = "open"``, each client
   that can connect to the controller can obtain a credential for an eligible
   GPU node. The same check is bypassed when no signing key or signer is
   available. Use token admission with a working signer before deployment.

   This credential can modify other nodes' pods and claims in ``spur-system``.
   Pod Security baseline does not prevent these API operations. A firewall
   can limit access, but does not correct the credential issuance defect.

Opt-out and node removal do not delete the ServiceAccount or its RBAC objects.
An administrator must remove these objects when the node no longer needs
Kubernetes access. Do not remove them while the agent still needs them for
placeholder cleanup or node-label updates. Token rotation is not revocation.

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

How Spur holds a GPU: the placeholder pod
-----------------------------------------

Before ``spurd`` starts a Spur job on a shared node, it makes one
``ResourceClaim`` and one placeholder pod for the job:

- Namespace: ``spur-system``. ``spurd`` makes the namespace.
- Name of the pod and of the claim: ``spur-job-<jobid>-<run_attempt>-<node>``,
  for example ``spur-job-1042-0-gpu01``. A job on many nodes has one
  placeholder on each node.
- Labels: ``spur.amd.com/job-id``, ``spur.amd.com/run-attempt``,
  ``spur.amd.com/node``, ``spur.amd.com/user``, ``spur.amd.com/account`` and
  ``app.kubernetes.io/managed-by=spurd``. A user or account name that is not a
  valid label value is left out.
- The pod runs the pause image of k0s. It has no CPU or memory request.
- The pod selects the node with a node selector on the hostname. It does not
  use ``spec.nodeName``, because then kube-scheduler does not allocate the
  claim.

The claim uses ``deviceClassName: gpu.amd.com``. It has one request for each
whole GPU, with a CEL selector on the ``pciBusID``. For a partitioned (CPX) GPU
it has one request with ``count`` *k* on the ``pciBusID`` of the parent GPU.
For example:

.. code-block:: yaml

   apiVersion: resource.k8s.io/v1
   kind: ResourceClaim
   metadata:
     name: spur-job-1042-0-gpu01
     namespace: spur-system
     labels:
       spur.amd.com/job-id: "1042"
       spur.amd.com/run-attempt: "0"
       spur.amd.com/node: gpu01
       spur.amd.com/user: alice
       spur.amd.com/account: research
       app.kubernetes.io/managed-by: spurd
   spec:
     devices:
       requests:
         - name: g0
           exactly:
             deviceClassName: gpu.amd.com
             allocationMode: ExactCount
             count: 1
             selectors:
               - cel:
                   expression: device.attributes["resource.kubernetes.io"].pciBusID == "0000:2f:00.0"

``spurd`` waits until kube-scheduler allocates the claim and schedules the pod.
Then it starts the job. The launch deadline is the only limit of this wait.
``spurd`` reads ``controller.dispatch_timeout_secs`` (default 300 seconds) from
its own configuration file and stops the wait 10 seconds before this time
passes, counted from when the launch request arrives. Thus the controller
gets the answer before its own deadline. Use the same value on the controller
and on the agents. A slow API server does not cause a failure: ``spurd`` tries
a timed out API call again, in the deadline.

Two conditions stop the wait:

- The placeholder gets ``PodScheduled=False`` with the reason
  ``Unschedulable``. The DRA scheduler plugin sets this when it cannot allocate
  the claim, for example because a pod holds the GPU.
- The deadline passes.

In the two conditions, ``spurd`` deletes the placeholder and the claim, and
refuses the launch. The job goes back to the queue.

On a CPX GPU, kube-scheduler selects which sibling partitions to give. The
job then runs on those partitions. The controller accepts a different
partition only when the number of devices is the same and each device has the
same parent GPU as a device in the request.

When the job ends, or when its launch fails after ``spurd`` made the
placeholder, ``spurd`` deletes the pod and the claim. Every 30 seconds
``spurd`` also does a check of its placeholders:

- It deletes each placeholder of the node that has no live job. A job is live
  from the start of its launch until its resources are released.
- While sharing is enabled, it tries to recreate a missing placeholder of a
  tracked running job.
- While sharing is enabled, it reports a conflict if the claim of a tracked
  running job holds different GPUs from the job.

After a restart, ``spurd`` also does these two last steps for the GPU jobs that
it recovers from before the restart. Recreation is not an atomic reservation:
another pod can acquire the GPU before the replacement claim.
Do not delete a live job's placeholder or claim.

How Spur sees pod GPUs: holds
-----------------------------

``spurd`` watches the ``ResourceClaim`` objects and the ``ResourceSlice`` of
its node. Each GPU that an allocated claim of a pod names is a **hold**. A
hold starts when kube-scheduler allocates the claim, before the pod starts.
Placeholder pods of Spur are not holds.

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
is busy for all time, because a pod has no end time.

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
     - No allocated claim holds the GPU. This is not proof that no process
       uses it: a job without a placeholder or an old device-plugin pod is
       not protected by this ledger.
   * - ``job <id>``
     - The placeholder of Spur job ``<id>`` holds the GPU.
   * - ``held <ns>/<pod> (claim <name>)``
     - The claim ``<name>`` of the pod ``<ns>/<pod>`` holds the GPU.
   * - ``conflict job <id> vs <ns>/<pod>``
     - Spur job ``<id>`` uses the GPU, but its placeholder lost the GPU to the
       pod. See `Failure cases`_.
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

With `The DeviceClass`_, a pod can request ``amd.com/gpu`` as on an ordinary
GPU node:

.. code-block:: yaml

   resources:
     limits:
       amd.com/gpu: 1

kube-scheduler makes a ``ResourceClaim`` for the pod, in the namespace of the
pod, and allocates a ``gpu.amd.com`` device to it. The name of the claim is
``<pod>-extended-resources-<suffix>``, and is shorter when the pod name is
long. The pod field ``status.extendedResourceClaimStatus.resourceClaimName``
gives the name. ``spurd`` sees this claim as any other claim, and
``scontrol show node`` shows ``held <ns>/<pod> (claim <name>)``. AIM and
KServe pods use this path with no change.

A pod can also request a ``ResourceClaim`` directly:

.. code-block:: yaml

   apiVersion: resource.k8s.io/v1
   kind: ResourceClaim
   metadata:
     name: one-gpu
     namespace: demo
   spec:
     devices:
       requests:
         - name: gpu
           exactly:
             deviceClassName: gpu.amd.com
             allocationMode: ExactCount
             count: 1
   ---
   apiVersion: v1
   kind: Pod
   metadata:
     name: gpu-probe
     namespace: demo
   spec:
     restartPolicy: Never
     nodeSelector:
       spur.amd.com/gpu-sharing: "true"
     resourceClaims:
       - name: gpu
         resourceClaimName: one-gpu
     containers:
       - name: rocm
         image: rocm/dev-ubuntu-24.04:latest
         command: ["rocm-smi"]
         resources:
           claims:
             - name: gpu

The ``nodeSelector`` is optional. Without it, kube-scheduler can put the pod on
any node with a free ``gpu.amd.com`` device.

Validation scope
----------------

Single-node SPX testing with eight MI325X virtual-function GPUs, k0s 1.36.2,
gpu-operator 1.5.1 and DRA driver 1.0.1 covered:

- Explicit claims and generated claims from ``amd.com/gpu`` requests.
- AIM inference with unchanged GPU requests, using aim-engine 0.2.6.
- Concurrent Spur and AIM workloads on different GPUs, in both start orders.
- Exhaustion, waiting for a free GPU, and allocation release.

The device-plugin migration check required removal of stale node status
fields. It did not establish that switching drivers with live workloads is
safe. These results cover this configuration, not every supported topology.

The following checks remain pending:

- CPX partition identity and allocation on hardware.
- Multi-node operation and the worker-credential path.
- A real ArgoCD deployment and migration of an existing chart-owned
  DeviceClass to the new owner.
- Kubernetes 1.34 and 1.35 with the alpha extended-resource gate enabled.
- The interaction between idle-fill reclaim and GPU holds.

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
- Spur and Kubernetes keep their own queues. Spur does not stop a pod to start
  a job, and does not stop a job to start a pod.
- Each Spur job on a shared node needs one more kube-scheduler cycle before it
  starts. A busy API server makes this time longer, up to the launch deadline.

Failure cases
-------------

.. list-table::
   :header-rows: 1
   :widths: 35 65

   * - Case
     - Result
   * - A tracked running job loses its placeholder.
     - While sharing is enabled, ``spurd`` tries to recreate it at a presence
       check. This does not apply during opt-out. Another pod can
       acquire the GPU before repair; conflict reporting does not stop
       either workload.
   * - A GPU is in conflict.
     - ``scontrol show node`` shows ``conflict`` and the reason. The job gets
       no comment. The controller gives the GPU to no new job. Spur stops no job and no
       pod. The administrator decides what to do.
   * - ``spurd`` cannot connect to the API server.
     - ``spurd`` refuses new launches on the node. The last report becomes old,
       and after the heartbeat timeout the controller gives no GPU of the node
       to a job.
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
