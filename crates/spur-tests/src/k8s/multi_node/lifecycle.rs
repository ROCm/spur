// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    use kube::api::{Api, ListParams, PostParams};
    use serial_test::serial;

    use crate::k8s::fixture::{ClusterFixture, FixtureConfig};
    use crate::k8s::{
        assert_eventually, cleanup_spurjob, count_ready_pods, delete_pod, exec_in_pod,
        read_file_from_pod, simple_spurjob, suite_context, wait_pod_ready, DEFAULT_TIMEOUT,
    };

    const HA_TIMEOUT: Duration = Duration::from_secs(90);
    const POLL: Duration = Duration::from_secs(3);

    async fn assert_leader_elected(client: &kube::Client, ns: &str) {
        assert_eventually(HA_TIMEOUT, POLL, "no committed leader vote", || {
            let client = client.clone();
            let ns = ns.to_string();
            async move {
                for i in 0..3 {
                    let vote = read_file_from_pod(
                        &client,
                        &ns,
                        &format!("spurctld-{i}"),
                        "/var/spool/spur/raft/vote.json",
                    )
                    .await
                    .unwrap_or_default();
                    if vote.contains("\"committed\":true") {
                        return true;
                    }
                }
                false
            }
        })
        .await;
    }

    async fn assert_all_pods_ready(client: &kube::Client, ns: &str, count: usize) {
        assert_eventually(
            HA_TIMEOUT,
            POLL,
            &format!("expected {count} ready controller pods"),
            || {
                let client = client.clone();
                let ns = ns.to_string();
                async move {
                    count_ready_pods(&client, &ns, "app=spurctld")
                        .await
                        .map(|n| n >= count)
                        .unwrap_or(false)
                }
            },
        )
        .await;
    }

    /// Tests 9–15 from k8s_test.sh as a single sequential lifecycle.
    ///
    /// Deploys a 3-replica Raft cluster, validates leader election,
    /// state replication, failover recovery, state persistence across
    /// leader kill, post-failover writes, and log replication.
    #[tokio::test]
    #[ignore]
    #[serial]
    async fn raft_ha_full_lifecycle() {
        // === Phase 1: Deploy 3-replica Raft cluster (test 9) ===
        let suite = suite_context().await;
        let fixture = ClusterFixture::deploy(suite, FixtureConfig::raft_ha())
            .await
            .expect("failed to deploy 3-replica Raft cluster");

        let client = fixture.client();
        let ns = fixture.namespace();

        // With podManagementPolicy: Parallel, all 3 pods start
        // simultaneously and discover each other via symmetric bootstrap.
        assert_all_pods_ready(&client, ns, 3).await;
        assert_leader_elected(&client, ns).await;

        // === Phase 3: State replication (test 11) ===
        let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
        let pvcs = pvc_api
            .list(&ListParams::default())
            .await
            .expect("failed to list PVCs");
        let bound = pvcs
            .items
            .iter()
            .filter(|pvc| {
                pvc.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .map(|p| p == "Bound")
                    .unwrap_or(false)
            })
            .count();
        assert!(bound >= 3, "expected >= 3 Bound PVCs, got {bound}");

        assert_eventually(
            HA_TIMEOUT,
            POLL,
            "some nodes missing Raft log entries",
            || {
                let client = client.clone();
                let ns = ns.to_string();
                async move {
                    for i in 0..3 {
                        let output = exec_in_pod(
                            &client,
                            &ns,
                            &format!("spurctld-{i}"),
                            vec!["ls", "/var/spool/spur/raft/log/"],
                        )
                        .await
                        .unwrap_or_default();
                        if output.trim().is_empty() {
                            return false;
                        }
                    }
                    true
                }
            },
        )
        .await;

        // === Phase 4: Failover recovery (test 12) ===
        delete_pod(&client, ns, "spurctld-0")
            .await
            .expect("failed to kill spurctld-0");
        wait_pod_ready(&client, ns, "spurctld-0", HA_TIMEOUT)
            .await
            .expect("spurctld-0 not ready after failover");
        assert_all_pods_ready(&client, ns, 3).await;

        assert_eventually(
            HA_TIMEOUT,
            POLL,
            "restarted pod has no committed vote",
            || {
                let client = client.clone();
                let ns = ns.to_string();
                async move {
                    read_file_from_pod(&client, &ns, "spurctld-0", "/var/spool/spur/raft/vote.json")
                        .await
                        .map(|v| v.contains("\"committed\":true"))
                        .unwrap_or(false)
                }
            },
        )
        .await;

        // === Phase 5: State survives leader failover (test 13) ===
        let api = fixture.spurjob_api(ns);
        let job = simple_spurjob(
            "it-failover-state",
            vec![
                "sh".into(),
                "-c".into(),
                "echo FAILOVER_STATE_OK && sleep 5".into(),
            ],
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        assert_eventually(
            DEFAULT_TIMEOUT,
            POLL,
            "no job ID assigned before failover",
            || {
                let api = api.clone();
                async move {
                    api.get("it-failover-state")
                        .await
                        .ok()
                        .and_then(|j| j.status.as_ref().and_then(|s| s.spur_job_id))
                        .is_some()
                }
            },
        )
        .await;
        let job_id_before = api
            .get("it-failover-state")
            .await
            .ok()
            .and_then(|j| j.status.as_ref().and_then(|s| s.spur_job_id))
            .expect("job ID must exist after assert_eventually");

        // Find and kill the leader
        let leader_pod = find_leader_pod(&client, ns)
            .await
            .unwrap_or("spurctld-0".into());
        delete_pod(&client, ns, &leader_pod)
            .await
            .expect("failed to kill leader pod");

        for i in 0..3 {
            wait_pod_ready(&client, ns, &format!("spurctld-{i}"), HA_TIMEOUT)
                .await
                .ok();
        }

        let job_id_after = api
            .get("it-failover-state")
            .await
            .ok()
            .and_then(|j| j.status.as_ref().and_then(|s| s.spur_job_id));
        assert_eq!(
            Some(job_id_before),
            job_id_after,
            "job ID changed across failover: before={job_id_before}, after={job_id_after:?}"
        );

        assert_leader_elected(&client, ns).await;

        cleanup_spurjob(&api, "it-failover-state").await;

        // === Phase 6: New leader accepts writes (test 14) ===
        let job = simple_spurjob(
            "it-post-failover",
            vec![
                "sh".into(),
                "-c".into(),
                "echo POST_FAILOVER && sleep 2".into(),
            ],
        );
        api.create(&PostParams::default(), &job).await.unwrap();

        assert_eventually(
            DEFAULT_TIMEOUT,
            POLL,
            "new job not accepted after failover",
            || {
                let api = api.clone();
                async move {
                    api.get("it-post-failover")
                        .await
                        .ok()
                        .and_then(|j| j.status.as_ref().and_then(|s| s.spur_job_id))
                        .is_some()
                }
            },
        )
        .await;

        let post_job_id = api
            .get("it-post-failover")
            .await
            .ok()
            .and_then(|j| j.status.as_ref().and_then(|s| s.spur_job_id))
            .unwrap();
        assert!(
            post_job_id > job_id_before,
            "job ID sequence broken: {post_job_id} <= {job_id_before}"
        );

        cleanup_spurjob(&api, "it-post-failover").await;

        // === Phase 7: Log replication after node recovery (test 15) ===
        let mut min_logs: usize = usize::MAX;
        for i in 0..3 {
            let output = exec_in_pod(
                &client,
                ns,
                &format!("spurctld-{i}"),
                vec!["ls", "/var/spool/spur/raft/log/"],
            )
            .await
            .unwrap_or_default();
            let count = output.lines().filter(|l| !l.is_empty()).count();
            min_logs = min_logs.min(count);
        }
        assert!(
            min_logs > 0,
            "some nodes have no log entries (min={min_logs})"
        );

        delete_pod(&client, ns, "spurctld-2")
            .await
            .expect("failed to kill spurctld-2");
        wait_pod_ready(&client, ns, "spurctld-2", HA_TIMEOUT)
            .await
            .expect("spurctld-2 not ready after recovery");

        assert_eventually(
            HA_TIMEOUT,
            POLL,
            "recovered node has no log entries",
            || {
                let client = client.clone();
                let ns = ns.to_string();
                async move {
                    exec_in_pod(
                        &client,
                        &ns,
                        "spurctld-2",
                        vec!["ls", "/var/spool/spur/raft/log/"],
                    )
                    .await
                    .map(|o| !o.trim().is_empty())
                    .unwrap_or(false)
                }
            },
        )
        .await;

        // Cleanup
        fixture
            .teardown_workloads()
            .await
            .expect("workload teardown failed");
    }

    /// Find the Raft leader pod by checking vote.json on each controller.
    async fn find_leader_pod(client: &kube::Client, ns: &str) -> Option<String> {
        for i in 0..3u32 {
            let pod_name = format!("spurctld-{i}");
            let vote = read_file_from_pod(client, ns, &pod_name, "/var/spool/spur/raft/vote.json")
                .await
                .unwrap_or_default();

            if !vote.contains("\"committed\":true") {
                continue;
            }

            // Parse the voted-for node_id and check if it matches this pod
            let my_id = i + 1; // Raft node IDs are 1-indexed
            if let Some(node_id) = parse_voted_node_id(&vote) {
                if node_id == my_id {
                    return Some(pod_name);
                }
            }
        }
        None
    }

    fn parse_voted_node_id(vote_json: &str) -> Option<u32> {
        let v: serde_json::Value = serde_json::from_str(vote_json).ok()?;
        v.get("node_id").and_then(|n| n.as_u64()).map(|n| n as u32)
    }
}
