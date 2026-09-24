GPU Sharing with Kubernetes
===========================

On a :doc:`Spur-managed k0s cluster <managed-kubernetes>`, an enrolled node is
usually reserved for Kubernetes as a whole. Spur does not place jobs on it.

A **shared node** is different. Spur jobs and Kubernetes pods use the GPUs of
the same node at the same time. The split is per GPU and changes with the load.
For example, a node with 8 GPUs can run 3 GPUs of Spur jobs and 5 GPUs of pods.
One GPU is never given to a Spur job and a pod at the same time.

Kubernetes is the ledger of record for the GPUs of a shared node. Spur writes
its GPUs into that ledger before a job starts, and reads the GPUs that pods
hold from that ledger.

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

At enrolment:

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

Opt-out has an immediate effect and works like drain:

- The node is reserved for Kubernetes again. Spur places no new jobs on it.
- Spur jobs that run on the node continue until they end. Their placeholder
  pods go away when each job ends.
- ``spurd`` sets the node label to ``false`` immediately.
- The kubelet links stay until the last placeholder is gone.

Spur does not stop a pod or a job at opt-out.

The node label
~~~~~~~~~~~~~~

``spurd`` sets the label ``spur.amd.com/gpu-sharing`` on each node that k0s
enrolls:

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
   restart the gpu-operator controller:

   .. code-block:: bash

      kubectl -n <operator-namespace> rollout restart deployment <operator-controller>

   This applies only when the gpu-operator installs the driver. The Helm chart
   of the driver uses a usual ``nodeSelector``, and Kubernetes applies a label
   change to it automatically.

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
follow. In each procedure, the driver must run only on shared nodes.

With the Helm chart of the driver
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The chart is a release asset of
`ROCm/k8s-gpu-dra-driver <https://github.com/ROCm/k8s-gpu-dra-driver>`_,
release ``v1.0.1``. The chart makes the DeviceClass ``gpu.amd.com``.

.. code-block:: bash

   helm install amd-gpu-dra \
       https://github.com/ROCm/k8s-gpu-dra-driver/releases/download/v1.0.1/k8s-gpu-dra-driver-v1.0.1.tgz \
       --namespace kube-amd-gpu --create-namespace \
       --set image.tag=v1.0.1 \
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
- If an administrator deletes the placeholder of a running job, ``spurd`` makes
  it again.
- If the claim of a running job does not hold the GPUs of the job, ``spurd``
  reports these GPUs as ``conflict``.

After a restart, ``spurd`` does these two last steps only for the jobs that it
started after the restart. It keeps and deletes the placeholders of the older
jobs as usual.

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
     - No job and no pod holds the GPU.
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

``sinfo`` shows only GPU counts in its GRES columns.

Run a pod on a shared node
--------------------------

The AMD DRA driver v1.0.1 does not map the ``amd.com/gpu`` extended resource.
Thus a pod that requests ``amd.com/gpu`` cannot go to a shared node. It goes to
an ordinary GPU node. On a shared node, a pod must request a ``ResourceClaim``.
This rule applies until a release of the DRA driver supports
``extendedResourceName``.

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
   * - An administrator deletes the placeholder of a running job.
     - ``spurd`` makes the placeholder again. If a pod gets the GPU first,
       the GPU shows ``conflict``.
   * - A GPU is in conflict.
     - ``scontrol show node`` shows ``conflict`` and the reason. The job gets
       no comment. The controller gives the GPU to no new job. Spur stops no job and no
       pod. The administrator decides what to do.
   * - ``spurd`` cannot connect to the API server.
     - ``spurd`` refuses new launches on the node. The last report becomes old,
       and after the heartbeat timeout the controller gives no GPU of the node
       to a job.
   * - ``spurd`` restarts.
     - ``spurd`` gets the holds again from the claim watch, then sends a
       report.
   * - A claim is allocated, but its pod does not start.
     - The hold starts at allocation, so Spur does not use the GPU.
   * - No ``ResourceSlice`` from ``gpu.amd.com`` exists for the node.
     - The node is unshareable, with the reason in ``scontrol show node``.
   * - A foreign file or directory is at a kubelet link path.
     - ``spurd`` does not change it. The node is unshareable, and the reason
       names the path.
