// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::native_host::fixture::parse_job_id;
    use crate::native_host::gpu::fixture;
    use crate::native_host::wait_job;

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn hip_gpu_test_two_node() {
        let f = fixture().await;
        f.gpu_preflight(2).await;
        f.ship_gpu_assets().await;

        let gpu_bin = format!("{}/gpu_test", f.remote_dir);
        let script = f
            .write_script("gpu-2n.sh", &format!("#!/bin/bash\n'{gpu_bin}'\n"))
            .await
            .expect("script");
        let out_path = format!("{}/hip-2n.out", f.remote_dir);

        let sb = f
            .sbatch(&["-J", "test-hip-2n", "-N", "2", "-o", &out_path, &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(120))
            .await
            .expect("wait");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        let diag = f.debug_job(job_id).await;
        assert!(
            content.contains("ALL PASS"),
            "2-node HIP gpu_test must report ALL PASS.\n{diag}\noutput:\n{content}"
        );
        assert!(
            content.contains("GPU count:"),
            "HIP gpu_test must report GPU count.\noutput:\n{content}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn pytorch_distributed() {
        let f = fixture().await;
        f.gpu_preflight(2).await;
        f.ship_gpu_assets().await;

        let job_sh = format!("{}/distributed_job.sh", f.remote_dir);
        let out_path = format!("{}/pt-dist.out", f.remote_dir);
        let sb = f
            .sbatch(&["-J", "test-pt", "-N", "2", "-o", &out_path, &job_sh])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(600))
            .await
            .expect("wait");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        let diag = f.debug_job(job_id).await;
        assert!(
            content.contains("DONE"),
            "PyTorch distributed must report DONE.\n{diag}\noutput:\n{content}"
        );
        assert!(
            content.contains("TFLOPS") || content.contains("GPUs:"),
            "PyTorch output must contain TFLOPS or GPUs count.\noutput:\n{content}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn inference_two_node() {
        let f = fixture().await;
        f.gpu_preflight(2).await;
        f.ship_gpu_assets().await;

        let job_sh = format!("{}/inference_job.sh", f.remote_dir);
        let out_path = format!("{}/infer.out", f.remote_dir);
        let sb = f
            .sbatch(&["-J", "test-infer", "-N", "2", "-o", &out_path, &job_sh])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(600))
            .await
            .expect("wait");

        let all = f.read_output_all_nodes(&out_path).await;
        let diag = f.debug_job(job_id).await;
        assert!(
            all.contains("INFERENCE_OK"),
            "Inference must report INFERENCE_OK.\n{diag}\noutput:\n{all}"
        );
        assert!(
            all.contains("Throughput:"),
            "Inference must report throughput.\noutput:\n{all}"
        );
        assert!(
            !all.to_lowercase().contains("nan") && !all.to_lowercase().contains("non-finite"),
            "Inference output contains NaN/non-finite values.\noutput:\n{all}"
        );
    }
}
