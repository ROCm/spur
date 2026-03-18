#!/usr/bin/env python3
"""
Distributed PyTorch GPU test for Spur cluster.

Two modes:
1. Single-node: RCCL all-reduce across all local GPUs (torchrun-style)
2. Multi-node: Per-GPU GEMM benchmark on each node (no cross-node RCCL)

Spur dispatches to each node with SPUR_JOB_ID, SPUR_NODE_RANK, etc.
"""

import os
import socket
import time
import torch
import torch.distributed as dist
import torch.multiprocessing as mp


def get_spur_env():
    return {
        "job_id": os.environ.get("SPUR_JOB_ID", "0"),
        "node_rank": int(os.environ.get("SPUR_NODE_RANK", os.environ.get("RANK", "0"))),
        "num_nodes": int(os.environ.get("SPUR_NUM_NODES", os.environ.get("WORLD_SIZE", "1"))),
        "task_offset": int(os.environ.get("SPUR_TASK_OFFSET", "0")),
        "peer_nodes": os.environ.get("SPUR_PEER_NODES", ""),
    }


def allreduce_worker(local_rank, num_gpus, results_dict):
    """Worker for single-node multi-GPU all-reduce via RCCL."""
    os.environ["MASTER_ADDR"] = "127.0.0.1"
    os.environ["MASTER_PORT"] = "29500"

    dist.init_process_group(
        backend="nccl",
        rank=local_rank,
        world_size=num_gpus,
    )

    torch.cuda.set_device(local_rank)
    device = f"cuda:{local_rank}"

    sizes_mb = [1, 16, 64, 256]
    local_results = []

    for size_mb in sizes_mb:
        numel = size_mb * 1024 * 1024 // 4
        tensor = torch.randn(numel, device=device)

        # Warmup
        for _ in range(5):
            dist.all_reduce(tensor)
        torch.cuda.synchronize()

        # Timed
        iters = 20
        start = time.perf_counter()
        for _ in range(iters):
            dist.all_reduce(tensor)
        torch.cuda.synchronize()
        elapsed = time.perf_counter() - start

        avg_ms = (elapsed / iters) * 1000
        algo_bw = (2 * (num_gpus - 1) / num_gpus * size_mb) / (avg_ms / 1000)
        local_results.append((size_mb, avg_ms, algo_bw))

    results_dict[local_rank] = local_results
    dist.destroy_process_group()


def gemm_benchmark(device, sizes=((4096, 4096), (8192, 8192))):
    """FP16 matrix multiply benchmark on a single GPU."""
    results = []
    for m, n in sizes:
        k = m
        a = torch.randn(m, k, device=device, dtype=torch.float16)
        b = torch.randn(k, n, device=device, dtype=torch.float16)

        # Warmup
        for _ in range(10):
            torch.mm(a, b)
        torch.cuda.synchronize()

        # Timed
        iters = 50
        start = time.perf_counter()
        for _ in range(iters):
            torch.mm(a, b)
        torch.cuda.synchronize()
        elapsed = time.perf_counter() - start

        avg_ms = (elapsed / iters) * 1000
        tflops = (2 * m * n * k) / (avg_ms / 1000) / 1e12
        results.append((m, n, avg_ms, tflops))

    return results


def main():
    env = get_spur_env()
    hostname = socket.gethostname()
    num_gpus = torch.cuda.device_count()

    print(f"=== Spur Distributed PyTorch Test ===")
    print(f"Host: {hostname} | Job: {env['job_id']} | Node rank: {env['node_rank']}/{env['num_nodes']}")
    print(f"Task offset: {env['task_offset']} | Peers: {env['peer_nodes']}")
    print(f"PyTorch: {torch.__version__} | HIP: {torch.version.hip}")
    print(f"GPUs: {num_gpus}")
    for i in range(num_gpus):
        props = torch.cuda.get_device_properties(i)
        total_gb = getattr(props, 'total_memory', getattr(props, 'total_mem', 0)) // (1024**3)
        print(f"  GPU {i}: {props.name} | {total_gb} GB")
    print()

    # --- Part 1: Per-GPU GEMM benchmark ---
    print("--- FP16 GEMM Benchmark ---")
    for gpu_id in range(num_gpus):
        device = f"cuda:{gpu_id}"
        results = gemm_benchmark(device)
        for m, n, avg_ms, tflops in results:
            print(f"  GPU {gpu_id}: {m}x{n} : {avg_ms:6.2f} ms | {tflops:6.1f} TFLOPS")
    print()

    # --- Part 2: Single-node multi-GPU RCCL all-reduce ---
    if num_gpus > 1:
        print(f"--- RCCL All-Reduce ({num_gpus} GPUs, single-node) ---")

        manager = mp.Manager()
        results_dict = manager.dict()

        mp.spawn(
            allreduce_worker,
            args=(num_gpus, results_dict),
            nprocs=num_gpus,
            join=True,
        )

        # Print results from rank 0
        if 0 in results_dict:
            for size_mb, avg_ms, algo_bw in results_dict[0]:
                print(f"  {size_mb:>4} MB: {avg_ms:7.2f} ms | {algo_bw:7.1f} GB/s algo BW")
        print()
    else:
        print("Skipping all-reduce (single GPU)")
        print()

    print(f"=== DONE (node rank {env['node_rank']}) ===")


if __name__ == "__main__":
    mp.set_start_method("spawn", force=True)
    main()
