// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// MPI + RCCL all-reduce: ranks bootstrapped by Spur's PMIx plugin, then a GPU
// collective over RCCL.
//
// Output contract, parsed by test_rccl.py:
//   rank=<r> size=<n> host=<h> device=<d> bus=<pci>
//   allreduce rank=<r> expected=<e> actual=<a> status=<OK|MISMATCH>
//   comm_size=<n>

#include <mpi.h>
#include <rccl/rccl.h>
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

    // Spur narrows the visible set per task, so rank-modulo is right here.
    int devices = 0;
    HIP_CHECK(hipGetDeviceCount(&devices));
    if (devices < 1) {
        fprintf(stderr, "rank=%d host=%s has no visible GPU\n", rank, host);
        MPI_Abort(MPI_COMM_WORLD, 1);
    }
    int device = rank % devices;
    HIP_CHECK(hipSetDevice(device));

    // Ordinals are all 0 after narrowing; the bus id is what differs.
    char bus[64] = {0};
    HIP_CHECK(hipDeviceGetPCIBusId(bus, sizeof(bus), device));

    printf("rank=%d size=%d host=%s device=%d bus=%s\n", rank, size, host, device,
           bus);
    fflush(stdout);

    // A wrong rank map from PMIx hangs ncclCommInitRank right here.
    ncclUniqueId id;
    if (rank == 0) {
        RCCL_CHECK(ncclGetUniqueId(&id));
    }
    MPI_Bcast(&id, sizeof(id), MPI_BYTE, 0, MPI_COMM_WORLD);

    ncclComm_t comm;
    RCCL_CHECK(ncclCommInitRank(&comm, size, id, rank));

    hipStream_t stream;
    HIP_CHECK(hipStreamCreate(&stream));

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
