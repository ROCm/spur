# Docker containers and Spur job cleanup

On Spur compute nodes, **prolog** and **epilog** hooks manage Docker containers so jobs do not leave stale containers or block co-tenant workloads. Multiple jobs can run on the same node when resources allow.

To prevent your containers from being removed while your job is running—or from being mistaken for orphans—**label every container you start** with the Spur job ID.

---

## Required: label containers with your job ID

When you run Docker inside a Spur job, always pass:

```bash
--label spur_job_id=${SPUR_JOB_ID}
```

`SPUR_JOB_ID` is set automatically in the job environment. (Spur also exports `SLURM_JOB_ID` with the same value for compatibility, but the **Docker label key must be `spur_job_id`**.)

### Example

```bash
docker run -d \
  --label spur_job_id=${SPUR_JOB_ID} \
  --network host \
  --device /dev/kfd \
  --device /dev/dri \
  my-image:tag \
  /path/to/entrypoint.sh
```

### Recommended job script pattern

```bash
#!/bin/bash
#SBATCH --job-name=my-training
# ... other SBATCH directives ...

set -euo pipefail

: "${SPUR_JOB_ID:?SPUR_JOB_ID is not set — are you inside a Spur job?}"

DOCKER_JOB_LABEL=(--label "spur_job_id=${SPUR_JOB_ID}")

docker run "${DOCKER_JOB_LABEL[@]}" \
  --rm \
  --network host \
  my-image:tag \
  "$@"
```

---

## What the hooks do

### Prolog (before a job starts on a node)

Runs on the node when **any** job is about to launch. It may remove **stale** containers left behind by **finished** jobs. It does **not** remove containers that belong to **active** jobs on that node.

Your container is **kept** during prolog if any of the following is true:

| Condition | Description |
|-----------|-------------|
| **Labeled with an active job ID** | `spur_job_id` matches a job that still has processes on the node |
| **Labeled with the launching job** | `spur_job_id` matches the job currently starting |
| **Younger than 10 minutes** | Container age &lt; 600 seconds (grace period) |
| **Unlabeled, no GPU conflict** | No label, but container does not use GPUs allocated to the launching job |

Your container may be **removed** during prolog if:

- It has `spur_job_id` (or legacy `slurm_job_id`) set to a **job that is no longer active**, or
- It has **no label**, uses GPUs that another job needs, and is older than 10 minutes.

### Epilog (after your job ends)

Runs when **your** job finishes (success, failure, or cancel). It removes:

- Containers labeled `spur_job_id=<your job ID>`
- Containers whose processes are still in your job’s `job_<id>` or
  `job_<id>_<step>` cgroup

**Expect your labeled containers to be stopped and removed when the job completes.**
If any job-owned process, container, mount, rootfs, scratch path, or cgroup
survives verification, epilog exits non-zero so Spur drains the node.

---

## Multi-tenant nodes (shared nodes)

Several jobs may run on the same node at once. The hooks are designed so that:

- **Your labeled containers are not killed** while your job is active, even if another job starts on the same node.
- **Other users’ containers are not killed** by your job’s prolog/epilog if they are labeled with their own active job ID.

**Do not rely on unlabeled containers** on shared nodes. Without a label, ownership is guessed from cgroups and GPU overlap, which is unreliable—especially for detached (`-d`) or long-running containers.

---

## Fallback: cgroup ownership (no label)

If you **do not** label a container, the hooks may still associate it with your job when the container’s main process runs inside the Spur job cgroup:

```text
/sys/fs/cgroup/spur/job_<id>/
```

Deployments that create step-scoped leaves may instead use
`/sys/fs/cgroup/spur/job_<id>_<step>/`; both exact forms are recognized.

This can work when:

- You run `docker run` **directly from your job script** (foreground or background), and
- The container process stays in the job cgroup.

This is **not reliable** for:

- Detached containers (`-d`) that outlive the job step
- `--privileged` or unusual network/cgroup setups
- Containers started outside the job process tree

**Use labels.** Cgroup detection is a fallback, not a substitute.

---

## Common mistakes

| Mistake | Result |
|---------|--------|
| No `--label spur_job_id=...` | Container may be killed as an orphan when another job starts |
| Wrong job ID in the label | Not protected; may be killed as a stale orphan |
| Using `slurm_job_id` as the label key | Recognized for compatibility, but deprecated — use `spur_job_id` |
| Container keeps running after job ends | Epilog removes labeled containers; if it survives, prolog removes it after ~10 minutes |
| Assuming `-d` alone is safe | Detached unlabeled containers are high risk on shared nodes |

---

## Checklist before submitting jobs

- [ ] Every `docker run` includes `--label spur_job_id=${SPUR_JOB_ID}`
- [ ] Job script uses `set -euo pipefail` and verifies `SPUR_JOB_ID` is set
- [ ] Long-running services are expected to stop when the job ends (epilog cleanup)
- [ ] Multi-node jobs: every rank uses the allocation’s `SPUR_JOB_ID`; steps may additionally have `SPUR_STEP_ID`

---

## Verify labels on a running container

From inside your job on the compute node:

```bash
# Show your job ID
echo "SPUR_JOB_ID=${SPUR_JOB_ID}"

# List containers for this job
docker ps --filter "label=spur_job_id=${SPUR_JOB_ID}"

# Inspect a specific container
docker inspect --format '{{json .Config.Labels}}' <container_id>
```

You should see:

```json
{"spur_job_id":"899"}
```

---

## Hook logs (for debugging)

If a container disappeared unexpectedly, admins can check on the compute node:

| Log | Purpose |
|-----|---------|
| `/var/log/spur/prolog.log` | Pre-job cleanup decisions |
| `/var/log/spur/epilog.log` | Post-job cleanup for your job ID |

Look for lines mentioning your container ID or `spur_job_id=<your job ID>`.

---

## Summary

```bash
# Always include this when starting containers inside a Spur job:
--label spur_job_id=${SPUR_JOB_ID}
```

- **While your job is running:** labeled containers are protected from other jobs’ prolog cleanup.
- **When your job ends:** epilog removes containers labeled with your job ID.
- **On shared nodes:** labeling is required for predictable, safe behavior.

For questions or cleanup incidents, contact your cluster admins with your **Spur job ID**, **node name**, and **container ID** (from `docker ps -a`).
