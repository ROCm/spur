// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#ifndef SPUR_MPI_PLUGIN_H
#define SPUR_MPI_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SPUR_MPI_PLUGIN_API_VERSION 3
#define SPUR_MPI_MAX_PEER_HOSTS 64

/* Use when the controller did not supply job credentials; plugin falls back to spurd. */
#define SPUR_MPI_JOB_CRED_UNSET UINT32_MAX

typedef struct spur_mpi_proc {
    uint32_t rank;
    uint32_t local_rank;
} spur_mpi_proc_t;

typedef struct spur_mpi_launch_plan {
    uint32_t job_id;
    char namespace_[256];
    uint32_t universe_size;
    uint32_t task_offset;
    uint32_t num_local_procs;
    spur_mpi_proc_t local_procs[256];
    char tmpdir[512];
    uint32_t job_uid;
    uint32_t job_gid;
    uint32_t num_nodes;
    uint32_t node_index;
    uint32_t num_peer_hosts;
    char peer_hosts[SPUR_MPI_MAX_PEER_HOSTS][256];
    /* 0 = use plugin defaults (see modex_exchange.h). */
    uint32_t modex_connect_timeout_sec;
    uint32_t modex_fence_timeout_sec;
    uint32_t modex_verify_timeout_sec;
} spur_mpi_launch_plan_t;

int spur_mpi_pmix_version(void);
int spur_mpi_pmix_runtime_version(char *buf, size_t buflen);
int spur_mpi_pmix_server_start(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen);
int spur_mpi_pmix_server_stop(const char *namespace_, char *errbuf, size_t errlen);
int spur_mpi_pmix_verify_peers(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen);
/* Bulk PMIx_server_setup_fork env (KEY=VALUE strings). Caller frees via setup_fork_env_free. */
int spur_mpi_pmix_setup_fork_env(
    const spur_mpi_launch_plan_t *plan,
    uint32_t rank,
    char ***env_out
);
void spur_mpi_pmix_setup_fork_env_free(char **env);

#ifdef __cplusplus
}
#endif

#endif
