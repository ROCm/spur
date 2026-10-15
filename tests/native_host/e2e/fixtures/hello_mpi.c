// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#include <mpi.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    int rank = 0;
    int size = 0;
    char host[256] = {0};

    MPI_Init(&argc, &argv);
    MPI_Comm_rank(MPI_COMM_WORLD, &rank);
    MPI_Comm_size(MPI_COMM_WORLD, &size);
    gethostname(host, sizeof(host) - 1);

    printf("rank=%d size=%d host=%s pid=%d\n", rank, size, host, (int)getpid());
    fflush(stdout);

    MPI_Finalize();
    return 0;
}
