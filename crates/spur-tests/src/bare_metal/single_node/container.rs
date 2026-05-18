#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;
    use tokio::sync::OnceCell;

    use crate::bare_metal::fixture::parse_job_id;
    use crate::bare_metal::single_node::fixture;
    use crate::bare_metal::wait_final_state;

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
    async fn container_launch_and_exit() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script(
                "c1.sh",
                "#!/bin/bash\nhostname >/dev/null || exit 1\nid >/dev/null || exit 1\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c1-launch",
                "-N",
                "1",
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
            "container job must complete, got {state}\n{diag}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_exit_code_propagation() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script("c2.sh", "#!/bin/bash\nexit 42\n")
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c2-exitcode",
                "-N",
                "1",
                &format!("--container-image={img}"),
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        let state = wait_final_state(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");
        assert_eq!(state, "F", "exit 42 should mark job failed");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_cancel() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script("c3.sh", "#!/bin/bash\nsleep 3600\n")
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c3-cancel",
                "-N",
                "1",
                &format!("--container-image={img}"),
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        // Wait for it to start running
        for _ in 0..15 {
            let sq = f.squeue_all().await.unwrap_or_default();
            if crate::bare_metal::job_state(&sq, job_id).as_deref() == Some("R") {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        f.scancel(&job_id.to_string()).await.expect("scancel");
        tokio::time::sleep(Duration::from_secs(3)).await;

        let state = wait_final_state(f, job_id, Duration::from_secs(30))
            .await
            .expect("wait");
        assert!(
            state == "CA" || state == "F" || state == "GONE",
            "cancelled container job should be CA/F, got {state}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_dns_resolution() {
        let f = fixture().await;
        let img = container_image().await;
        let out_path = format!("{}/c4.out", f.remote_dir);
        let script = f
            .write_script(
                "c4.sh",
                "#!/bin/bash\n\
                 # Fail if resolv.conf has loopback stub (should be stripped)\n\
                 grep -q '127.0.0.53' /etc/resolv.conf && exit 1\n\
                 # Fail if DNS doesn't work\n\
                 getent hosts google.com >/dev/null 2>&1 || exit 2\n\
                 echo DNS_OK\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c4-dns",
                "-N",
                "1",
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
            "DNS test failed (exit 1=loopback in resolv.conf, 2=getent failed), state={state}\n{diag}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_dev_shm() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script(
                "c5.sh",
                "#!/bin/bash\n\
                 echo shm_test > /dev/shm/spur_ctest || exit 1\n\
                 rm -f /dev/shm/spur_ctest\n\
                 df /dev/shm >/dev/null 2>&1 || exit 2\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c5-shm",
                "-N",
                "1",
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
            "/dev/shm test failed (1=write failed, 2=not mounted), state={state}\n{diag}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_pid_namespace() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script(
                "c6.sh",
                "#!/bin/bash\n\
                 [ \"$$\" = \"1\" ] || exit 1\n\
                 [ -r /proc/self/status ] || exit 2\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c6-pidns",
                "-N",
                "1",
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
            "PID namespace test failed (1=not PID 1, 2=/proc missing), state={state}\n{diag}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_env_vars() {
        let f = fixture().await;
        let img = container_image().await;
        let script = f
            .write_script(
                "c7.sh",
                "#!/bin/bash\n\
                 [ -n \"$SPUR_JOB_ID\" ] || exit 1\n\
                 [ -n \"$OMP_NUM_THREADS\" ] || exit 2\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "c7-env",
                "-N",
                "1",
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
            "env var test failed (1=no SPUR_JOB_ID, 2=no OMP_NUM_THREADS), state={state}\n{diag}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn container_bind_mount_readonly() {
        let f = fixture().await;
        let img = container_image().await;

        let bind_dir = format!("{}/bind-test", f.remote_dir);
        for node in &f.nodes {
            node.exec(&format!(
                "mkdir -p '{bind_dir}' && echo bind_mount_ci_test > '{bind_dir}/data.txt'"
            ))
            .await
            .expect("setup bind dir");
        }

        let script = f
            .write_script(
                "c8.sh",
                "#!/bin/bash\n\
                 [ \"$(cat /mnt/data/data.txt 2>/dev/null)\" = \"bind_mount_ci_test\" ] || exit 1\n\
                 # Write must fail on :ro mount\n\
                 touch /mnt/data/write_test 2>/dev/null && exit 2\n\
                 exit 0\n",
            )
            .await
            .expect("script");

        let mount_spec = format!("{bind_dir}:/mnt/data:ro");
        let sb = f
            .sbatch(&[
                "-J",
                "c8-bind",
                "-N",
                "1",
                &format!("--container-image={img}"),
                &format!("--container-mounts={mount_spec}"),
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
            "bind mount test failed (1=content wrong, 2=ro not enforced), state={state}\n{diag}"
        );
    }
}
