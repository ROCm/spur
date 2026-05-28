// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;
    use tokio::sync::OnceCell;

    use crate::native_host::fixture::parse_job_id;
    use crate::native_host::multi_node::fixture;
    use crate::native_host::{wait_final_state, wait_job};

    static CONTAINER_IMG: OnceCell<String> = OnceCell::const_new();

    async fn container_image() -> &'static str {
        CONTAINER_IMG
            .get_or_init(|| async {
                let f = fixture().await;
                f.container_preflight().await;
                f.build_container_image()
                    .await
                    .expect("failed to build container image")
            })
            .await
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn two_node_container_job() {
        let f = fixture().await;
        let img = container_image().await;
        let out_path = format!("{}/ct-2n.out", f.remote_dir);
        let script = f
            .write_script(
                "ct-2n.sh",
                "#!/bin/bash\n\
                 echo \"CONTAINER_NODE=$(hostname)\"\n\
                 echo \"SPUR_NODE_RANK=${SPUR_NODE_RANK}\"\n\
                 echo \"SPUR_NUM_NODES=${SPUR_NUM_NODES}\"\n\
                 echo CONTAINER_2N_OK\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "ct-2node",
                "-N",
                "2",
                "-o",
                &out_path,
                &format!("--container-image={img}"),
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(90))
            .await
            .expect("wait");

        let all = f.read_output_all_nodes(&out_path).await;
        let diag = f.debug_job(job_id).await;
        assert!(
            all.contains("CONTAINER_2N_OK"),
            "2-node container job must report CONTAINER_2N_OK\n{diag}\noutput:\n{all}"
        );
        assert!(
            all.contains("SPUR_NUM_NODES=2"),
            "must see SPUR_NUM_NODES=2\noutput:\n{all}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn two_node_container_env_vars() {
        let f = fixture().await;
        let img = container_image().await;
        let out_path = format!("{}/ct-2n-env.out", f.remote_dir);
        let script = f
            .write_script(
                "ct-2n-env.sh",
                "#!/bin/bash\n\
                 echo \"RANK=${RANK}\"\n\
                 echo \"WORLD_SIZE=${WORLD_SIZE}\"\n\
                 echo \"MASTER_ADDR=${MASTER_ADDR}\"\n\
                 echo \"SPUR_JOB_ID=${SPUR_JOB_ID}\"\n\
                 echo CT_ENV_OK\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "ct-2n-env",
                "-N",
                "2",
                "-o",
                &out_path,
                &format!("--container-image={img}"),
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(90))
            .await
            .expect("wait");

        let all = f.read_output_all_nodes(&out_path).await;
        assert!(all.contains("CT_ENV_OK"), "missing CT_ENV_OK:\n{all}");
        assert!(all.contains("WORLD_SIZE=2"), "missing WORLD_SIZE=2:\n{all}");
        assert!(all.contains("RANK=0"), "missing RANK=0:\n{all}");
        assert!(all.contains("RANK=1"), "missing RANK=1:\n{all}");
        assert!(
            all.lines()
                .any(|l| l.starts_with("MASTER_ADDR=") && l.len() > "MASTER_ADDR=".len()),
            "MASTER_ADDR should be non-empty:\n{all}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn two_node_container_dns_both_nodes() {
        let f = fixture().await;
        let img = container_image().await;
        let out_path = format!("{}/ct-2n-dns.out", f.remote_dir);
        let script = f
            .write_script(
                "ct-2n-dns.sh",
                "#!/bin/bash\n\
                 getent hosts google.com >/dev/null 2>&1 || exit 1\n\
                 echo \"DNS_OK_$(hostname)\"\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "ct-2n-dns",
                "-N",
                "2",
                "-o",
                &out_path,
                &format!("--container-image={img}"),
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        let state = wait_final_state(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");
        let diag = f.debug_job(job_id).await;
        assert!(
            state == "CD" || state == "GONE",
            "2-node container DNS failed, state={state}\n{diag}"
        );
    }
}
