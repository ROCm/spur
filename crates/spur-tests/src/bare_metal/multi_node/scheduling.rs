#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::bare_metal::fixture::parse_job_id;
    use crate::bare_metal::multi_node::fixture;
    use crate::bare_metal::{job_state, wait_job};

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn nodelist_runs_on_requested_node() {
        let f = fixture().await;
        let target = &f.node_names[0];
        let out_path = format!("{}/nodelist-{}.out", f.remote_dir, target);
        let script = f
            .write_script(
                "nodename.sh",
                "#!/bin/bash\necho \"RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}\"\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "test-nodelist",
                "-N",
                "1",
                "-w",
                target,
                "-o",
                &out_path,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(
            content.contains(&format!("RAN_ON={target}")),
            "expected run on {target}, got:\n{content}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn nodelist_runs_on_second_node() {
        let f = fixture().await;
        let target = &f.node_names[1];
        let out_path = format!("{}/nodelist-{}.out", f.remote_dir, target);
        let script = f
            .write_script(
                "nodename2.sh",
                "#!/bin/bash\necho \"RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}\"\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "test-nodelist2",
                "-N",
                "1",
                "-w",
                target,
                "-o",
                &out_path,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(
            content.contains(&format!("RAN_ON={target}")),
            "expected run on {target}, got:\n{content}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn exclude_skips_node() {
        let f = fixture().await;
        assert!(
            f.node_names.len() >= 2,
            "exclude test requires at least 2 nodes, got {}",
            f.node_names.len()
        );
        let excluded = &f.node_names[0];
        let out_path = format!("{}/exclude.out", f.remote_dir);
        let script = f
            .write_script(
                "nodename.sh",
                "#!/bin/bash\necho \"RAN_ON=${SPUR_TARGET_NODE:-$(hostname)}\"\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "test-exclude",
                "-N",
                "1",
                "-x",
                excluded,
                "-o",
                &out_path,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(
            !content.contains(&format!("RAN_ON={excluded}")),
            "job must not run on excluded node {excluded}, got:\n{content}"
        );
        let allowed: Vec<_> = f.node_names.iter().skip(1).collect();
        assert!(
            allowed
                .iter()
                .any(|n| content.contains(&format!("RAN_ON={n}"))),
            "expected run on one of {:?} (excluded {excluded}), got:\n{content}",
            allowed
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn concurrent_jobs_on_two_nodes() {
        let f = fixture().await;
        assert!(
            f.node_names.len() >= 2,
            "concurrent test requires at least 2 nodes, got {}",
            f.node_names.len()
        );

        let out1 = format!("{}/con1.out", f.remote_dir);
        let out2 = format!("{}/con2.out", f.remote_dir);
        let script = f
            .write_script(
                "concurrent.sh",
                "#!/bin/bash\necho CONCURRENT_START\nsleep 5\necho CONCURRENT_DONE\n",
            )
            .await
            .expect("script");

        let sb1 = f
            .sbatch(&["-J", "con1", "-N", "1", "-o", &out1, &script])
            .await
            .expect("sbatch1");
        let sb2 = f
            .sbatch(&["-J", "con2", "-N", "1", "-o", &out2, &script])
            .await
            .expect("sbatch2");
        let j1 = parse_job_id(&sb1).expect("j1");
        let j2 = parse_job_id(&sb2).expect("j2");

        tokio::time::sleep(Duration::from_secs(3)).await;
        let sq = f.squeue_all().await.expect("squeue");
        assert_eq!(job_state(&sq, j1).as_deref(), Some("R"));
        assert_eq!(job_state(&sq, j2).as_deref(), Some("R"));

        wait_job(f, j1, Duration::from_secs(60))
            .await
            .expect("wait j1");
        wait_job(f, j2, Duration::from_secs(60))
            .await
            .expect("wait j2");

        let c1 = f.read_output_on_any_node(&out1).await.expect("c1");
        let c2 = f.read_output_on_any_node(&out2).await.expect("c2");
        assert!(
            c1.contains("CONCURRENT_DONE"),
            "job1 missing CONCURRENT_DONE:\n{c1}"
        );
        assert!(
            c2.contains("CONCURRENT_DONE"),
            "job2 missing CONCURRENT_DONE:\n{c2}"
        );
    }
}
