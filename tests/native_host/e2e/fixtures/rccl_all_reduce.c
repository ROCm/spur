// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// MPI + RCCL all-reduce. Exercises the one combination Spur's e2e never covered:
// ranks bootstrapped by Spur's PMIx plugin, then a GPU collective over RCCL.
//
// The existing distributed_test.py drives RCCL through PyTorch on a single node,
// and hello_mpi.c drives PMIx with no GPU. Neither proves that a rank map handed
// out by `srun --mpi=pmix` can bring up an RCCL communicator, which is what every
// real MPI-launched RCCL workload depends on.
//
// Output contract, parsed by test_rccl.py:
//   rank=<r> size=<n> host=<h> device=<d>
//   allreduce rank=<r> expected=<e> actual=<a> status=<OK|MISMATCH>
//   comm_size=<n>
//
// Build on each compute node (the controller may have no ROCm):
//   hipcc -o rccl_all_reduce rccl_all_reduce.c -lrccl -lmpi -I<mpi_include>

#include <mpi.h>
// Plain <rccl.h>, paired with a -I at whichever directory actually holds the
// header. ROCm moved it into an rccl/ subdirectory between versions, so the
// include path is resolved per node rather than assumed here.
#include <rccl.h>
#include <hip/hip_runtime.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define HIP_CHECK(cmd)                                                      \
    do {                                                                    \
        hipError_t e = (cmd);                                               \
        if (e != hipSuccess) {                                              \
            fprintf(stderr, "HIP error %s at %s:%d\n",                      \
                    hipGetErrorString(e), __FILE__, __LINE__);              \
            MPI_Abort(MPI_COMM_WORLD, 1);                                   \
        }                                                                   \
    } while (0)

#define RCCL_CHECK(cmd)                                                     \
    do {                                                                    \
        ncclResult_t r = (cmd);                                             \
        if (r != ncclSuccess) {                                             \
            fprintf(stderr, "RCCL error %s at %s:%d\n",                     \
                    ncclGetErrorString(r), __FILE__, __LINE__);             \
            MPI_Abort(MPI_COMM_WORLD, 1);                                   \
        }                                                                   \
    } while (0)

int main(int argc, char **argv) {
    MPI_Init(&argc, &argv);

    int rank = 0, size = 0;
    MPI_Comm_rank(MPI_COMM_WORLD, &rank);
    MPI_Comm_size(MPI_COMM_WORLD, &size);

    char host[256] = {0};
    gethostname(host, sizeof(host) - 1);

    // One GPU per rank, round-robin over the devices this rank can see. Spur's
    // cgroup device isolation already narrows that set per job, so rank-modulo
    // is correct rather than a guess about global device ids.
    int devices = 0;
    HIP_CHECK(hipGetDeviceCount(&devices));
    if (devices < 1) {
        fprintf(stderr, "rank=%d host=%s has no visible GPU\n", rank, host);
        MPI_Abort(MPI_COMM_WORLD, 1);
    }
    int device = rank % devices;
    HIP_CHECK(hipSetDevice(device));

    printf("rank=%d size=%d host=%s device=%d\n", rank, size, host, device);
    fflush(stdout);

    // Rank 0 mints the RCCL id and broadcasts it over MPI. This is the standard
    // bootstrap, and it is the part that fails if the PMIx-provided rank map is
    // wrong: a duplicate or missing rank hangs ncclCommInitRank.
    ncclUniqueId id;
    if (rank == 0) {
        RCCL_CHECK(ncclGetUniqueId(&id));
    }
    MPI_Bcast(&id, sizeof(id), MPI_BYTE, 0, MPI_COMM_WORLD);

    ncclComm_t comm;
    RCCL_CHECK(ncclCommInitRank(&comm, size, id, rank));

    hipStream_t stream;
    HIP_CHECK(hipStreamCreate(&stream));

    // Each rank contributes (rank + 1), so the sum is n(n+1)/2. A wrong result
    // means the collective ran but moved the wrong data, which is a different
    // failure from a hang and worth distinguishing.
    const int count = 1024;
    float *sendbuf = NULL, *recvbuf = NULL;
    HIP_CHECK(hipMalloc((void **)&sendbuf, count * sizeof(float)));
    HIP_CHECK(hipMalloc((void **)&recvbuf, count * sizeof(float)));

    float *host_send = (float *)malloc(count * sizeof(float));
    float *host_recv = (float *)malloc(count * sizeof(float));
    if (!host_send || !host_recv) {
        fprintf(stderr, "rank=%d host allocation failed\n", rank);
        MPI_Abort(MPI_COMM_WORLD, 1);
    }
    for (int i = 0; i < count; i++) {
        host_send[i] = (float)(rank + 1);
    }
    HIP_CHECK(hipMemcpy(sendbuf, host_send, count * sizeof(float), hipMemcpyHostToDevice));

    RCCL_CHECK(ncclAllReduce((const void *)sendbuf, (void *)recvbuf, count,
                             ncclFloat, ncclSum, comm, stream));
    HIP_CHECK(hipStreamSynchronize(stream));
    HIP_CHECK(hipMemcpy(host_recv, recvbuf, count * sizeof(float), hipMemcpyDeviceToHost));

    const float expected = (float)(size * (size + 1) / 2);
    int mismatched = 0;
    for (int i = 0; i < count; i++) {
        if (host_recv[i] != expected) {
            mismatched = 1;
            break;
        }
    }
    printf("allreduce rank=%d expected=%.1f actual=%.1f status=%s\n",
           rank, expected, host_recv[0], mismatched ? "MISMATCH" : "OK");

    // Reported once, after the collective, so the test can assert every rank
    // joined the same communicator rather than inferring it from stdout ordering.
    if (rank == 0) {
        printf("comm_size=%d\n", size);
    }
    fflush(stdout);

    free(host_send);
    free(host_recv);
    HIP_CHECK(hipFree(sendbuf));
    HIP_CHECK(hipFree(recvbuf));
    HIP_CHECK(hipStreamDestroy(stream));
    RCCL_CHECK(ncclCommDestroy(comm));

    MPI_Finalize();
    return mismatched;
}
