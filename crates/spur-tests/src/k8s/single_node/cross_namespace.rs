#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams, PostParams};
    use serial_test::serial;
    use spur_k8s::crd::SpurJob;

    use crate::k8s::single_node::fixture;
    use crate::k8s::{
        cleanup_spurjob, delete_namespace, ensure_namespace, simple_spurjob, wait_spurjob_state,
        DEFAULT_TIMEOUT,
    };

    #[tokio::test]
    #[ignore]
    #[serial]
    async fn cross_namespace_no_pod_leakage() {
        let f = fixture().await;
        let client = f.fresh_client().await.expect("fresh client");
        let cross_ns = format!("{}-user1", f.namespace());

        ensure_namespace(&client, &cross_ns).await.unwrap();

        let api: Api<SpurJob> = Api::namespaced(client.clone(), &cross_ns);
        let job = simple_spurjob(
            "it-cross-ns",
            vec![
                "sh".into(),
                "-c".into(),
                "echo CROSS_NS_OK && sleep 1".into(),
            ],
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        let completed = wait_spurjob_state(&api, "it-cross-ns", "Completed", DEFAULT_TIMEOUT)
            .await
            .unwrap();

        if let Some(job_id) = completed.status.as_ref().and_then(|s| s.spur_job_id) {
            let spur_pods: Api<Pod> = Api::namespaced(client.clone(), f.namespace());
            let leaked = spur_pods
                .list(&ListParams::default().labels(&format!("spur.ai/job-id={job_id}")))
                .await
                .unwrap();
            assert!(
                leaked.items.is_empty(),
                "pods leaked into spur namespace for cross-ns job (found {})",
                leaked.items.len()
            );
        }

        cleanup_spurjob(&api, "it-cross-ns").await;
        delete_namespace(&client, &cross_ns).await;
    }
}
