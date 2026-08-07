// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Driver for the Slurm-compatible FFI (libspur_compat.so).
//
// Declarations are hand-written because the library ships no slurm.h; they
// must stay in sync with crates/spur-ffi/src/types.rs. Each subcommand prints
// machine-readable `key=value` lines so the e2e test can assert on them
// without parsing prose.

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NO_VAL ((uint32_t)0xFFFFFFFF)

typedef struct {
    const char *name;
    const char *partition;
    const char *account;
    const char *script;
    const char *work_dir;
    uint32_t min_nodes;
    uint32_t max_nodes;
    uint32_t num_tasks;
    uint32_t cpus_per_task;
    uint32_t time_limit;
    uint32_t priority;
} job_desc_msg_t;

typedef struct {
    uint32_t job_id;
    char *name;
    char *user_name;
    char *partition;
    char *account;
    uint32_t job_state;
    uint32_t num_nodes;
    uint32_t num_tasks;
    int32_t exit_code;
    char *nodelist;
} job_info_t;

// The container structs are Rust-owned and carry trailing private storage;
// only these leading fields are part of the C-visible layout.
typedef struct {
    uint32_t record_count;
    job_info_t *job_array;
} job_info_msg_t;

typedef struct {
    char *name;
    uint32_t node_state;
    uint32_t cpus;
    uint64_t real_memory;
    char *reason;
} node_info_t;

typedef struct {
    uint32_t record_count;
    node_info_t *node_array;
} node_info_msg_t;

typedef struct {
    char *name;
    uint32_t total_nodes;
    uint32_t total_cpus;
    char *nodes;
} partition_info_t;

typedef struct {
    uint32_t record_count;
    partition_info_t *partition_array;
} partition_info_msg_t;

extern void slurm_init_job_desc_msg(job_desc_msg_t *desc);
extern int slurm_submit_batch_job(const job_desc_msg_t *desc, uint32_t *job_id);
extern int slurm_load_jobs(int64_t update_time, job_info_msg_t **resp, uint32_t flags);
extern void slurm_free_job_info_msg(job_info_msg_t *msg);
extern int slurm_load_node(int64_t update_time, node_info_msg_t **resp, uint32_t flags);
extern int slurm_load_partitions(int64_t update_time, partition_info_msg_t **resp,
                                 uint32_t flags);
extern int slurm_kill_job(uint32_t job_id, uint16_t signal, uint16_t flags);
extern const char *slurm_strerror(int errnum);

static const char *safe(const char *s)
{
    return s ? s : "";
}

static int cmd_defaults(void)
{
    job_desc_msg_t desc;
    memset(&desc, 0xAB, sizeof(desc));
    slurm_init_job_desc_msg(&desc);

    printf("name_null=%d\n", desc.name == NULL);
    printf("script_null=%d\n", desc.script == NULL);
    printf("min_nodes=%u\n", desc.min_nodes);
    printf("max_nodes=%u\n", desc.max_nodes);
    printf("num_tasks_is_no_val=%d\n", desc.num_tasks == NO_VAL);
    printf("cpus_per_task=%u\n", desc.cpus_per_task);
    printf("time_limit=%u\n", desc.time_limit);
    return 0;
}

static int cmd_submit(int argc, char **argv)
{
    if (argc < 4) {
        fprintf(stderr, "usage: submit <name> <partition> <script> [nodes] [tasks]\n");
        return 2;
    }

    job_desc_msg_t desc;
    slurm_init_job_desc_msg(&desc);
    desc.name = argv[1];
    desc.partition = argv[2][0] ? argv[2] : NULL;
    desc.script = argv[3];
    desc.min_nodes = (argc > 4) ? (uint32_t)strtoul(argv[4], NULL, 10) : 1;
    desc.max_nodes = desc.min_nodes;
    desc.num_tasks = (argc > 5) ? (uint32_t)strtoul(argv[5], NULL, 10) : 1;

    uint32_t job_id = 0;
    int rc = slurm_submit_batch_job(&desc, &job_id);
    printf("rc=%d\n", rc);
    printf("job_id=%u\n", job_id);
    return rc == 0 ? 0 : 1;
}

static int cmd_jobs(int argc, char **argv)
{
    job_info_msg_t *msg = NULL;
    int rc = slurm_load_jobs(0, &msg, 0);
    printf("rc=%d\n", rc);
    if (rc != 0 || msg == NULL)
        return 1;

    uint32_t want = (argc > 1) ? (uint32_t)strtoul(argv[1], NULL, 10) : 0;
    printf("record_count=%u\n", msg->record_count);
    for (uint32_t i = 0; i < msg->record_count; i++) {
        job_info_t *j = &msg->job_array[i];
        if (want != 0 && j->job_id != want)
            continue;
        printf("job id=%u name=%s user=%s partition=%s account=%s state=%u "
               "nodes=%u tasks=%u exit=%d nodelist=%s\n",
               j->job_id, safe(j->name), safe(j->user_name), safe(j->partition),
               safe(j->account), j->job_state, j->num_nodes, j->num_tasks,
               j->exit_code, safe(j->nodelist));
    }

    slurm_free_job_info_msg(msg);
    return 0;
}

static int cmd_nodes(void)
{
    node_info_msg_t *msg = NULL;
    int rc = slurm_load_node(0, &msg, 0);
    printf("rc=%d\n", rc);
    if (rc != 0 || msg == NULL)
        return 1;

    printf("record_count=%u\n", msg->record_count);
    for (uint32_t i = 0; i < msg->record_count; i++) {
        node_info_t *n = &msg->node_array[i];
        printf("node name=%s state=%u cpus=%u memory=%llu reason=%s\n",
               safe(n->name), n->node_state, n->cpus,
               (unsigned long long)n->real_memory, safe(n->reason));
    }
    return 0;
}

static int cmd_partitions(void)
{
    partition_info_msg_t *msg = NULL;
    int rc = slurm_load_partitions(0, &msg, 0);
    printf("rc=%d\n", rc);
    if (rc != 0 || msg == NULL)
        return 1;

    printf("record_count=%u\n", msg->record_count);
    for (uint32_t i = 0; i < msg->record_count; i++) {
        partition_info_t *p = &msg->partition_array[i];
        printf("partition name=%s total_nodes=%u total_cpus=%u nodes=%s\n",
               safe(p->name), p->total_nodes, p->total_cpus, safe(p->nodes));
    }
    return 0;
}

static int cmd_kill(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: kill <job_id> [signal]\n");
        return 2;
    }
    uint32_t job_id = (uint32_t)strtoul(argv[1], NULL, 10);
    uint16_t signal = (argc > 2) ? (uint16_t)strtoul(argv[2], NULL, 10) : 9;
    int rc = slurm_kill_job(job_id, signal, 0);
    printf("rc=%d\n", rc);
    return rc == 0 ? 0 : 1;
}

static int cmd_strerror(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: strerror <errnum>\n");
        return 2;
    }
    printf("message=%s\n", safe(slurm_strerror((int)strtol(argv[1], NULL, 10))));
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <defaults|submit|jobs|nodes|partitions|kill|"
                        "strerror> [args...]\n",
                argv[0]);
        return 2;
    }

    const char *cmd = argv[1];
    int rc;

    if (strcmp(cmd, "defaults") == 0)
        rc = cmd_defaults();
    else if (strcmp(cmd, "submit") == 0)
        rc = cmd_submit(argc - 1, argv + 1);
    else if (strcmp(cmd, "jobs") == 0)
        rc = cmd_jobs(argc - 1, argv + 1);
    else if (strcmp(cmd, "nodes") == 0)
        rc = cmd_nodes();
    else if (strcmp(cmd, "partitions") == 0)
        rc = cmd_partitions();
    else if (strcmp(cmd, "kill") == 0)
        rc = cmd_kill(argc - 1, argv + 1);
    else if (strcmp(cmd, "strerror") == 0)
        rc = cmd_strerror(argc - 1, argv + 1);
    else {
        fprintf(stderr, "unknown command: %s\n", cmd);
        return 2;
    }

    fflush(stdout);
    return rc;
}
