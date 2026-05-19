// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serial_test::serial;

    use crate::bare_metal::fixture::parse_job_id;
    use crate::bare_metal::single_node::fixture;
    use crate::bare_metal::{assert_eventually, job_state, wait_final_state, wait_job};

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn job_cancel() {
        let f = fixture().await;
        let script = f
            .write_script("test-long.sh", "#!/bin/bash\nsleep 300\n")
            .await
            .expect("script");

        let sb = f
            .sbatch(&["-J", "test-cancel", "-N", "1", &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        tokio::time::sleep(Duration::from_secs(3)).await;
        f.scancel(&job_id.to_string()).await.expect("scancel");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sq = f.squeue_all().await.expect("squeue");
        let state = job_state(&sq, job_id);
        assert!(
            matches!(state.as_deref(), Some("CA") | Some("F") | None),
            "expected cancelled, got {:?}",
            state
        );
    }

    /// After cancellation, the node must return to idle (resources released).
    /// Regression test: the force-kill path previously removed the job from
    /// the agent's tracking map without releasing allocations.
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn job_cancel_releases_resources() {
        let f = fixture().await;
        let script = f
            .write_script(
                "test-cancel-res.sh",
                "#!/bin/bash\ntrap '' TERM\nsleep 300\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&["-J", "cancel-res", "-N", "1", &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        tokio::time::sleep(Duration::from_secs(4)).await;
        f.scancel(&job_id.to_string()).await.expect("scancel");

        // Wait for SIGTERM grace (5s) + SIGKILL + monitor reap
        assert_eventually(
            Duration::from_secs(15),
            Duration::from_secs(2),
            "node should return to idle after cancel",
            || async {
                let info = f.sinfo().await.unwrap_or_default();
                info.contains("idle") && !info.contains("mix") && !info.contains("alloc")
            },
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn job_hold_and_release() {
        let f = fixture().await;
        let out_path = format!("{}/spur-hold.out", f.remote_dir);
        let script = f
            .write_script("test-hold.sh", "#!/bin/bash\necho HOLD_OK\n")
            .await
            .expect("script");

        let sb = f
            .sbatch(&["-J", "test-hold", "-N", "1", "-H", "-o", &out_path, &script])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        tokio::time::sleep(Duration::from_secs(2)).await;
        let sq = f.squeue_all().await.expect("squeue");
        assert_eq!(job_state(&sq, job_id).as_deref(), Some("PD"));

        f.scontrol_release(&job_id.to_string())
            .await
            .expect("release");
        wait_job(f, job_id, Duration::from_secs(60))
            .await
            .expect("wait after release");

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(content.contains("HOLD_OK"));
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn job_dependency_afterok() {
        let f = fixture().await;
        let out_a = format!("{}/dep-a.out", f.remote_dir);
        let out_b = format!("{}/dep-b.out", f.remote_dir);
        let script_a = f
            .write_script(
                "dep-a.sh",
                "#!/bin/bash\necho DEP_A_START\nsleep 6\necho DEP_A_DONE\n",
            )
            .await
            .expect("script a");
        let script_b = f
            .write_script("dep-b.sh", "#!/bin/bash\necho DEP_B_RAN\n")
            .await
            .expect("script b");

        let sb_a = f
            .sbatch(&["-J", "test-dep-a", "-N", "1", "-o", &out_a, &script_a])
            .await
            .expect("sbatch a");
        let job_a = parse_job_id(&sb_a).expect("job a");

        let sb_b = f
            .sbatch(&[
                "-J",
                "test-dep-b",
                "-N",
                "1",
                "-o",
                &out_b,
                &format!("--dependency=afterok:{job_a}"),
                &script_b,
            ])
            .await
            .expect("sbatch b");
        let job_b = parse_job_id(&sb_b).expect("job b");

        tokio::time::sleep(Duration::from_secs(3)).await;
        let sq = f.squeue_all().await.expect("squeue");
        assert_eq!(job_state(&sq, job_a).as_deref(), Some("R"));
        assert_eq!(job_state(&sq, job_b).as_deref(), Some("PD"));

        wait_job(f, job_a, Duration::from_secs(60))
            .await
            .expect("wait a");
        tokio::time::sleep(Duration::from_secs(3)).await;
        wait_job(f, job_b, Duration::from_secs(60))
            .await
            .expect("wait b");

        let content = f.read_output_on_any_node(&out_b).await.expect("out b");
        assert!(content.contains("DEP_B_RAN"));
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn time_limit_enforced() {
        let f = fixture().await;
        let out_path = format!("{}/walltime.out", f.remote_dir);
        let script = f
            .write_script(
                "walltime.sh",
                "#!/bin/bash\necho WALLTIME_STARTED\nsleep 300\necho WALLTIME_SHOULD_NOT_REACH\n",
            )
            .await
            .expect("script");

        let sb = f
            .sbatch(&[
                "-J",
                "test-walltime",
                "-N",
                "1",
                "-o",
                &out_path,
                "-t",
                "0:00:10",
                &script,
            ])
            .await
            .expect("sbatch");
        let job_id = parse_job_id(&sb).expect("job id");

        let state = wait_final_state(f, job_id, Duration::from_secs(45))
            .await
            .expect("wait terminal");
        assert!(
            matches!(state.as_str(), "CA" | "F" | "TO" | "GONE"),
            "job should be killed, got {state}"
        );

        let content = f.read_output_on_any_node(&out_path).await.expect("out");
        assert!(content.contains("WALLTIME_STARTED"));
        assert!(!content.contains("WALLTIME_SHOULD_NOT_REACH"));
    }
}
