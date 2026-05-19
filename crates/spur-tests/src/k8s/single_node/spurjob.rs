// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, DeleteParams, ListParams, PostParams};
    use serial_test::serial;
    use spur_k8s::crd::SpurJob;

    use crate::k8s::single_node::fixture;
    use crate::k8s::{
        assert_eventually, cleanup_spurjob, multinode_spurjob, simple_spurjob, spurjob_with_env,
        wait_spurjob_state, DEFAULT_TIMEOUT,
    };

    async fn fresh_api() -> (kube::Client, Api<SpurJob>) {
        let f = fixture().await;
        let client = f.fresh_client().await.expect("fresh client");
        let api = Api::namespaced(client.clone(), f.namespace());
        (client, api)
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn simple_spurjob_completes() {
        let (_client, api) = fresh_api().await;

        let job = simple_spurjob(
            "it-simple",
            vec![
                "sh".into(),
                "-c".into(),
                "echo SPUR_K8S_OK && sleep 1".into(),
            ],
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        let completed = wait_spurjob_state(&api, "it-simple", "Completed", DEFAULT_TIMEOUT)
            .await
            .unwrap();

        let status = completed.status.expect("should have status");
        assert!(status.spur_job_id.is_some(), "should have a Spur job ID");

        cleanup_spurjob(&api, "it-simple").await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn env_vars_passed_through() {
        let (_client, api) = fresh_api().await;

        let env = HashMap::from([("CUSTOM_VAR".to_string(), "spur-ci-test".to_string())]);
        let job = spurjob_with_env(
            "it-env",
            vec![
                "sh".into(),
                "-c".into(),
                "echo job=$SPUR_JOB_ID custom=$CUSTOM_VAR".into(),
            ],
            env,
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        wait_spurjob_state(&api, "it-env", "Completed", DEFAULT_TIMEOUT)
            .await
            .unwrap();

        cleanup_spurjob(&api, "it-env").await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn multinode_job_assigns_nodes() {
        let (_client, api) = fresh_api().await;

        let job = multinode_spurjob(
            "it-multi",
            vec![
                "sh".into(),
                "-c".into(),
                "echo rank=$SPUR_NODE_RANK nodes=$SPUR_NUM_NODES host=$(hostname)".into(),
            ],
            2,
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        let completed = wait_spurjob_state(&api, "it-multi", "Completed", Duration::from_secs(90))
            .await
            .unwrap();

        let status = completed.status.expect("should have status");
        assert!(
            !status.assigned_nodes.is_empty(),
            "multi-node job should have assigned nodes"
        );

        cleanup_spurjob(&api, "it-multi").await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn cancellation_cleans_up_pods() {
        let f = fixture().await;
        let (client, api) = fresh_api().await;
        let pods: Api<Pod> = Api::namespaced(client, f.namespace());

        let job = simple_spurjob("it-cancel", vec!["sleep".into(), "600".into()]);
        api.create(&PostParams::default(), &job).await.unwrap();

        tokio::time::sleep(Duration::from_secs(8)).await;

        api.delete("it-cancel", &DeleteParams::default())
            .await
            .unwrap();

        assert_eventually(
            DEFAULT_TIMEOUT,
            Duration::from_secs(2),
            "pods not cleaned up after SpurJob cancellation",
            || {
                let pods = pods.clone();
                async move {
                    pods.list(&ListParams::default().labels("spur.ai/job-name=it-cancel"))
                        .await
                        .map(|list| list.items.is_empty())
                        .unwrap_or(false)
                }
            },
        )
        .await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn failure_detected() {
        let (_client, api) = fresh_api().await;

        let job = simple_spurjob("it-fail", vec!["sh".into(), "-c".into(), "exit 42".into()]);
        api.create(&PostParams::default(), &job).await.unwrap();

        wait_spurjob_state(&api, "it-fail", "Failed", DEFAULT_TIMEOUT)
            .await
            .unwrap();

        cleanup_spurjob(&api, "it-fail").await;
    }

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn sequential_jobs_all_complete() {
        let (_client, api) = fresh_api().await;

        // Submit one at a time — the controller enforces a 1-CPU minimum per
        // job, so submitting all 3 simultaneously can exhaust node CPU on
        // small test clusters.
        for i in 1..=3 {
            let name = format!("it-seq-{i}");
            let job = simple_spurjob(
                &name,
                vec!["sh".into(), "-c".into(), format!("echo seq={i}")],
            );
            api.create(&PostParams::default(), &job).await.unwrap();
            wait_spurjob_state(&api, &name, "Completed", DEFAULT_TIMEOUT)
                .await
                .unwrap();
            cleanup_spurjob(&api, &name).await;
        }
    }
}
