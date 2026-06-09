// Spur GPU allocation test — exercises only ROCR-visible devices.
// Compile: hipcc -o gpu_alloc_test gpu_alloc_test.hip

#include <hip/hip_runtime.h>
#include <cstdio>
#include <cstdlib>
#include <cmath>

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

int test_gpu(int device_id) {
  HIP_CHECK(hipSetDevice(device_id));
  const int n = 1 << 16;
  size_t bytes = n * sizeof(float);

  float *h_a = (float *)malloc(bytes);
  float *h_b = (float *)malloc(bytes);
  float *h_c = (float *)malloc(bytes);
  for (int i = 0; i < n; i++) {
    h_a[i] = 1.0f;
    h_b[i] = 2.0f;
  }

  float *d_a, *d_b, *d_c;
  HIP_CHECK(hipMalloc(&d_a, bytes));
  HIP_CHECK(hipMalloc(&d_b, bytes));
  HIP_CHECK(hipMalloc(&d_c, bytes));
  HIP_CHECK(hipMemcpy(d_a, h_a, bytes, hipMemcpyHostToDevice));
  HIP_CHECK(hipMemcpy(d_b, h_b, bytes, hipMemcpyHostToDevice));

  int threads = 256;
  int blocks = (n + threads - 1) / threads;
  vector_add<<<blocks, threads>>>(d_a, d_b, d_c, n);
  HIP_CHECK(hipDeviceSynchronize());
  HIP_CHECK(hipMemcpy(h_c, d_c, bytes, hipMemcpyDeviceToHost));

  int errors = 0;
  for (int i = 0; i < n; i++) {
    if (fabs(h_c[i] - 3.0f) > 1e-5f) {
      errors++;
      break;
    }
  }

  HIP_CHECK(hipFree(d_a));
  HIP_CHECK(hipFree(d_b));
  HIP_CHECK(hipFree(d_c));
  free(h_a);
  free(h_b);
  free(h_c);
  return errors;
}

int main() {
  const char *rocr = getenv("ROCR_VISIBLE_DEVICES");
  const char *spur = getenv("SPUR_JOB_GPUS");
  printf("ROCR_VISIBLE_DEVICES=%s\n", rocr ? rocr : "");
  printf("SPUR_JOB_GPUS=%s\n", spur ? spur : "");

  int device_count = 0;
  HIP_CHECK(hipGetDeviceCount(&device_count));
  printf("GPU count: %d\n", device_count);

  int passed = 0;
  for (int i = 0; i < device_count; i++) {
    if (test_gpu(i) == 0) {
      passed++;
    }
  }

  printf("ALLOC_OK %d/%d\n", passed, device_count);
  return passed == device_count ? 0 : 1;
}
