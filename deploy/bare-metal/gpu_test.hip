// Spur GPU Test — vector add across all GPUs on a node
// Compile: hipcc -o gpu_test gpu_test.hip

#include <hip/hip_runtime.h>
#include <cstdio>
#include <cstdlib>
#include <cmath>
#include <unistd.h>

#define HIP_CHECK(cmd)                                                         \
  do {                                                                         \
    hipError_t err = cmd;                                                      \
    if (err != hipSuccess) {                                                   \
      fprintf(stderr, "HIP error %d: %s at %s:%d\n", err,                     \
              hipGetErrorString(err), __FILE__, __LINE__);                     \
      exit(1);                                                                 \
    }                                                                          \
  } while (0)

__global__ void vector_add(const float *a, const float *b, float *c, int n) {
  int i = blockDim.x * blockIdx.x + threadIdx.x;
  if (i < n) {
    c[i] = a[i] + b[i];
  }
}

int test_gpu(int device_id, int n) {
  HIP_CHECK(hipSetDevice(device_id));

  hipDeviceProp_t props;
  HIP_CHECK(hipGetDeviceProperties(&props, device_id));

  size_t bytes = n * sizeof(float);

  // Host arrays
  float *h_a = (float *)malloc(bytes);
  float *h_b = (float *)malloc(bytes);
  float *h_c = (float *)malloc(bytes);

  for (int i = 0; i < n; i++) {
    h_a[i] = sinf(i) * sinf(i);
    h_b[i] = cosf(i) * cosf(i);
  }

  // Device arrays
  float *d_a, *d_b, *d_c;
  HIP_CHECK(hipMalloc(&d_a, bytes));
  HIP_CHECK(hipMalloc(&d_b, bytes));
  HIP_CHECK(hipMalloc(&d_c, bytes));

  HIP_CHECK(hipMemcpy(d_a, h_a, bytes, hipMemcpyHostToDevice));
  HIP_CHECK(hipMemcpy(d_b, h_b, bytes, hipMemcpyHostToDevice));

  int threads = 256;
  int blocks = (n + threads - 1) / threads;

  // Warm up
  vector_add<<<blocks, threads>>>(d_a, d_b, d_c, n);
  HIP_CHECK(hipDeviceSynchronize());

  // Timed run
  hipEvent_t start, stop;
  HIP_CHECK(hipEventCreate(&start));
  HIP_CHECK(hipEventCreate(&stop));

  HIP_CHECK(hipEventRecord(start));
  for (int iter = 0; iter < 100; iter++) {
    vector_add<<<blocks, threads>>>(d_a, d_b, d_c, n);
  }
  HIP_CHECK(hipEventRecord(stop));
  HIP_CHECK(hipEventSynchronize(stop));

  float ms = 0;
  HIP_CHECK(hipEventElapsedTime(&ms, start, stop));

  // Verify
  HIP_CHECK(hipMemcpy(h_c, d_c, bytes, hipMemcpyDeviceToHost));

  int errors = 0;
  for (int i = 0; i < n; i++) {
    float expected = h_a[i] + h_b[i];
    if (fabs(h_c[i] - expected) > 1e-5) {
      errors++;
    }
  }

  double gb = (3.0 * bytes * 100) / (1e9);
  double bw = gb / (ms / 1000.0);

  printf("  GPU %d [%s]: %s | %.2f ms (100 iters) | %.1f GB/s",
         device_id, props.name, errors == 0 ? "PASS" : "FAIL", ms, bw);
  if (props.totalGlobalMem > 0) {
    printf(" | VRAM %.1f GB", props.totalGlobalMem / (1024.0 * 1024 * 1024));
  }
  printf("\n");

  HIP_CHECK(hipEventDestroy(start));
  HIP_CHECK(hipEventDestroy(stop));
  HIP_CHECK(hipFree(d_a));
  HIP_CHECK(hipFree(d_b));
  HIP_CHECK(hipFree(d_c));
  free(h_a);
  free(h_b);
  free(h_c);

  return errors;
}

int main() {
  int device_count = 0;
  HIP_CHECK(hipGetDeviceCount(&device_count));

  char hostname[256];
  gethostname(hostname, sizeof(hostname));

  const char *job_id = getenv("SPUR_JOB_ID");
  const char *task_offset = getenv("SPUR_TASK_OFFSET");
  const char *peer_nodes = getenv("SPUR_PEER_NODES");

  printf("=== Spur GPU Test ===\n");
  printf("Node: %s | Job: %s | Task offset: %s\n",
         hostname, job_id ? job_id : "N/A", task_offset ? task_offset : "N/A");
  printf("Peer nodes: %s\n", peer_nodes ? peer_nodes : "N/A");
  printf("GPU count: %d\n", device_count);

  int n = 1 << 20; // 1M elements
  int total_errors = 0;

  for (int i = 0; i < device_count; i++) {
    total_errors += test_gpu(i, n);
  }

  printf("Result: %s (%d/%d GPUs passed)\n",
         total_errors == 0 ? "ALL PASS" : "SOME FAILED",
         device_count - total_errors, device_count);

  return total_errors > 0 ? 1 : 0;
}
