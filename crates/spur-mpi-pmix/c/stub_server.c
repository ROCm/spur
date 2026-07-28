// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include <stdio.h>
#include <string.h>

#include "spur_mpi_plugin.h"

int spur_mpi_pmix_version(void) {
    return SPUR_MPI_PLUGIN_API_VERSION;
}

int spur_mpi_pmix_runtime_version(char *buf, size_t buflen) {
    if (buf != NULL && buflen > 0) {
        buf[0] = '\0';
    }
    return -1;
}

int spur_mpi_pmix_server_start(const spur_mpi_launch_plan_t *plan, char *errbuf, size_t errlen) {
    (void)plan;
    if (errbuf != NULL && errlen > 0) {
        snprintf(
            errbuf,
            errlen,
            "spur_mpi_pmix.so was built without libpmix; rebuild with libpmix development packages"
        );
    }
    return -1;
}

int spur_mpi_pmix_server_stop(const char *namespace_, char *errbuf, size_t errlen) {
    (void)namespace_;
    if (errbuf != NULL && errlen > 0) {
        errbuf[0] = '\0';
    }
    return 0;
}

int spur_mpi_pmix_env(
    const spur_mpi_launch_plan_t *plan,
    uint32_t rank,
    const char *key,
    char *val,
    size_t vallen
) {
    (void)plan;
    (void)rank;
    (void)key;
    if (val != NULL && vallen > 0) {
        val[0] = '\0';
    }
    return -1;
}
