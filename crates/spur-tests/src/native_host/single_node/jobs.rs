// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::native_host::fixture::parse_job_id;
    use crate::native_host::single_node::fixture;
    use crate::native_host::{wait_final_state, wait_job};

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn sinfo_returns_output() {
        let f = fixture().await;
        let out = f.sinfo().await.expect("sinfo");
        assert!(!out.trim().is_empty(), "sinfo produced no output");
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn all_nodes_registered_and_idle() {
        let f = fixture().await;
        let out = f.sinfo().await.expect("sinfo");
        for name in &f.node_names {
            assert!(out.contains(name), "node {name} not in sinfo:\n{out}");
        }
        assert!(
            f.cluster_is_ready(&out),
            "expected {} idle nodes, sinfo:\n{out}",
            f.node_names.len()
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn single_node_job_completes_with_output() {
        let f = fixture().await;
        let script = f
            .write_script(
                "test-basic.sh",
                "#!/bin/bash\necho \"hostname=$(hostname)\"\necho \"SPUR_JOB_ID=${SPUR_JOB_ID}\"\necho SUCCESS\n",
            )
            .await
            .expect("write script");

        let out = f
            .sbatch(&["-J", "test-basic", "-N", "1", &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&out).expect("job id from sbatch");

        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait job");

        let state = crate::native_host::wait_final_state(f, job_id, Duration::from_secs(30))
            .await
            .expect("final state");
        assert!(
            state == "CD" || state == "GONE",
            "expected completed job, got {state}"
        );

        let content = f.read_job_output(job_id).await.expect("read out");
        assert!(content.contains("SUCCESS"), "output:\n{content}");
        assert!(
            content.contains(&format!("SPUR_JOB_ID={job_id}")),
            "output:\n{content}"
        );
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn failed_job_state_f() {
        let f = fixture().await;
        let out_path = format!("{}/spur-fail.out", f.remote_dir);
        let script = f
            .write_script(
                "test-exitfail.sh",
                "#!/bin/bash\necho before-failure\nexit 42\n",
            )
            .await
            .expect("script");

        let node = &f.node_names[0];
        let sb = f
            .sbatch(&[
                "-J",
                "test-exitfail",
                "-N",
                "1",
                "-w",
                node,
                "-o",
                &out_path,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        let state = wait_final_state(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");
        assert_eq!(state, "F", "expected failed state");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(content.contains("before-failure"));
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn custom_output_and_error_paths() {
        let f = fixture().await;
        let out_path = format!("{}/custom-out.txt", f.remote_dir);
        let err_path = format!("{}/custom-err.txt", f.remote_dir);
        let script = f
            .write_script(
                "test-io.sh",
                "#!/bin/bash\necho stdout-line\necho stderr-line >&2\necho CUSTOM_IO_OK\n",
            )
            .await
            .expect("script");

        let node = &f.node_names[0];
        let sb = f
            .sbatch(&[
                "-J",
                "test-custom-io",
                "-N",
                "1",
                "-w",
                node,
                "-o",
                &out_path,
                "-e",
                &err_path,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");

        let stdout = f.read_output_on_any_node(&out_path).await.expect("stdout");
        assert!(stdout.contains("CUSTOM_IO_OK"));
        let stderr = f.read_output_on_any_node(&err_path).await.expect("stderr");
        assert!(stderr.contains("stderr-line"));
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn percent_j_output_substitution() {
        let f = fixture().await;
        let script = f
            .write_script("test-basic-j.sh", "#!/bin/bash\necho J_OK\n")
            .await
            .expect("script");
        let pattern = format!("{}/spur-subst-%j.out", f.remote_dir);
        let node = &f.node_names[0];

        let sb = f
            .sbatch(&[
                "-J",
                "test-subst",
                "-N",
                "1",
                "-w",
                node,
                "-o",
                &pattern,
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");

        let path = format!("{}/spur-subst-{job_id}.out", f.remote_dir);
        let content = f.read_output_on_any_node(&path).await.expect("out");
        assert!(content.contains("J_OK"));
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn env_passthrough_export() {
        let f = fixture().await;
        let out_path = format!("{}/spur-env.out", f.remote_dir);
        let script = f
            .write_script(
                "test-env.sh",
                "#!/bin/bash\necho \"MYVAR=${MYVAR}\"\necho \"MULTIVAR=${MULTIVAR}\"\necho ENV_OK\n",
            )
            .await
            .expect("script");
        let node = &f.node_names[0];

        let cmd = format!(
            "SPUR_CONTROLLER_ADDR='{}' PATH='{}':$PATH MYVAR=hello123 MULTIVAR=world456 \
             '{}/sbatch' -J test-env -N 1 -w {node} -o '{out_path}' --export=MYVAR,MULTIVAR '{script}'",
            f.controller_addr, f.bin_dir, f.bin_dir, node = node, out_path = out_path, script = script
        );
        let sb = f.controller().exec(&cmd).await.expect("sbatch with env");
        let job_id = parse_job_id(&sb).expect("job id from sbatch");

        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait");
        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        let output = if content.trim().is_empty() {
            f.read_job_output(job_id).await.expect("job output")
        } else {
            content
        };
        assert!(
            output.contains("MYVAR=hello123"),
            "missing MYVAR=hello123:\n{output}"
        );
        assert!(
            output.contains("MULTIVAR=world456"),
            "missing MULTIVAR=world456:\n{output}"
        );
    }
}
