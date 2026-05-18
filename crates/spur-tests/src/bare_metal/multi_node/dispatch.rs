#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::bare_metal::fixture::parse_job_id;
    use crate::bare_metal::multi_node::fixture;
    use crate::bare_metal::wait_job;

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn two_node_job_completes() {
        let f = fixture().await;
        assert!(f.node_names.len() >= 2);

        let out_path = format!("{}/two-node.out", f.remote_dir);
        let script = f
            .write_script(
                "two-node.sh",
                "#!/bin/bash\n\
                 echo \"node=$(hostname)\"\n\
                 echo \"SPUR_JOB_ID=${SPUR_JOB_ID}\"\n\
                 echo \"SPUR_NODE_RANK=${SPUR_NODE_RANK}\"\n\
                 echo \"SPUR_NUM_NODES=${SPUR_NUM_NODES}\"\n\
                 echo \"SPUR_PEER_NODES=${SPUR_PEER_NODES}\"\n\
                 echo TWO_NODE_OK\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&["-J", "test-2node", "-N", "2", "-o", &out_path, &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(90))
            .await
            .expect("wait");

        let all = f.read_output_all_nodes(&out_path).await;
        assert!(all.contains("TWO_NODE_OK"), "missing TWO_NODE_OK:\n{all}");
        assert!(
            all.contains("SPUR_NUM_NODES=2"),
            "missing SPUR_NUM_NODES=2:\n{all}"
        );
        assert!(
            all.contains("SPUR_NODE_RANK="),
            "missing SPUR_NODE_RANK:\n{all}"
        );
        assert!(
            all.lines()
                .any(|l| l.starts_with("SPUR_PEER_NODES=") && l.len() > "SPUR_PEER_NODES=".len()),
            "SPUR_PEER_NODES should be set and non-empty:\n{all}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn distributed_env_vars() {
        let f = fixture().await;
        let out_path = format!("{}/dist-env.out", f.remote_dir);
        let script = f
            .write_script(
                "dist-env.sh",
                "#!/bin/bash\n\
                 echo \"RANK=${RANK}\"\n\
                 echo \"WORLD_SIZE=${WORLD_SIZE}\"\n\
                 echo \"MASTER_ADDR=${MASTER_ADDR}\"\n\
                 echo \"MASTER_PORT=${MASTER_PORT}\"\n\
                 echo DIST_ENV_OK\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&["-J", "test-dist-env", "-N", "2", "-o", &out_path, &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(90))
            .await
            .expect("wait");

        let mut all = f
            .read_output_on_any_node(&out_path)
            .await
            .expect("local out");
        for node in f.nodes.iter().skip(1) {
            if let Ok(more) = node.read_remote_file(&out_path).await {
                all.push('\n');
                all.push_str(&more);
            }
        }

        assert!(all.contains("WORLD_SIZE=2"));
        assert!(all.contains("RANK=0"));
        assert!(all.contains("RANK=1"));
        assert!(all.contains("MASTER_PORT=29500"));
        assert!(
            all.lines()
                .filter(|l| l.starts_with("MASTER_ADDR=") && *l != "MASTER_ADDR=")
                .count()
                >= 2,
            "MASTER_ADDR should be set on both ranks:\n{all}"
        );
    }
}
