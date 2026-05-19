// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::bare_metal::fixture::parse_job_id;
    use crate::bare_metal::gpu::fixture;
    use crate::bare_metal::wait_job;

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn hip_gpu_test() {
        let f = fixture().await;
        f.gpu_preflight(1).await;
        f.ship_gpu_assets().await;

        let gpu_bin = format!("{}/gpu_test", f.remote_dir);
        let script = f
            .write_script("gpu-1n.sh", &format!("#!/bin/bash\n'{gpu_bin}'\n"))
            .await
            .expect("script");
        let out_path = format!("{}/hip-1n.out", f.remote_dir);

        let sb = f
            .sbatch(&["-J", "test-hip-1n", "-N", "1", "-o", &out_path, &script])
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
            "HIP gpu_test must report ALL PASS.\n{diag}\noutput:\n{content}"
        );
        assert!(
            content.contains("GPU count:"),
            "HIP gpu_test must report GPU count.\noutput:\n{content}"
        );
        assert!(
            content.contains("MI300X") || content.contains("MI300") || content.contains("GPU"),
            "HIP gpu_test must identify GPU model.\noutput:\n{content}"
        );
    }
}
