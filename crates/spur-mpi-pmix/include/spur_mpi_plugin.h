// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#ifndef SPUR_MPI_PLUGIN_H
#define SPUR_MPI_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SPUR_MPI_PLUGIN_API_VERSION 1

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
} spur_mpi_launch_plan_t;

int spur_mpi_pmix_version(void);
int spur_mpi_pmix_runtime_version(char *buf, size_t buflen);
int spur_mpi_pmix_server_start(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen);
int spur_mpi_pmix_server_stop(const char *namespace_, char *errbuf, size_t errlen);
int spur_mpi_pmix_env(
    const spur_mpi_launch_plan_t *plan,
    uint32_t rank,
    const char *key,
    char *val,
    size_t vallen
);

#ifdef __cplusplus
}
#endif

#endif
