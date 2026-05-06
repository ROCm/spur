//! Test harness utilities.
//!
//! Provides helpers for standing up test clusters, submitting jobs,
//! and asserting state transitions.

use std::sync::atomic::{AtomicU32, Ordering};

use spur_core::config::SlurmConfig;
use spur_core::job::{Job, JobSpec, JobState};
use spur_core::node::{Node, NodeState};
use spur_core::partition::{Partition, PartitionState};
use spur_core::resource::ResourceSet;

static NEXT_JOB_ID: AtomicU32 = AtomicU32::new(1);

/// Reset the job ID counter (call at start of each test).
pub fn reset_job_ids() {
    NEXT_JOB_ID.store(1, Ordering::SeqCst);
}

/// Create a job with sensible defaults and auto-incrementing ID.
pub fn make_job(name: &str) -> Job {
    let id = NEXT_JOB_ID.fetch_add(1, Ordering::SeqCst);
    Job::new(
        id,
        JobSpec {
            name: name.into(),
            user: "testuser".into(),
            uid: 1000,
            gid: 1000,
            partition: Some("default".into()),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        },
    )
}

/// Create a job with specific resource requirements.
pub fn make_job_with_resources(
    name: &str,
    nodes: u32,
    tasks: u32,
    cpus_per_task: u32,
    time_limit_minutes: Option<i64>,
) -> Job {
    let id = NEXT_JOB_ID.fetch_add(1, Ordering::SeqCst);
    Job::new(
        id,
        JobSpec {
            name: name.into(),
            user: "testuser".into(),
            uid: 1000,
            gid: 1000,
            partition: Some("default".into()),
            num_nodes: nodes,
            num_tasks: tasks,
            cpus_per_task,
            time_limit: time_limit_minutes.map(|m| chrono::Duration::minutes(m)),
            work_dir: "/tmp".into(),
            ..Default::default()
        },
    )
}

/// Create a batch of idle compute nodes.
pub fn make_nodes(count: usize, cpus: u32, memory_mb: u64) -> Vec<Node> {
    (0..count)
        .map(|i| {
            let mut node = Node::new(
                format!("node{:03}", i + 1),
                ResourceSet {
                    cpus,
                    memory_mb,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["default".into()];
            node
        })
        .collect()
}

/// Create a default partition.
pub fn make_partition(name: &str, node_count: usize) -> Partition {
    let nodes = if node_count <= 1 {
        "node001".into()
    } else {
        format!("node[001-{:03}]", node_count)
    };
    Partition {
        name: name.into(),
        state: PartitionState::Up,
        is_default: name == "default",
        nodes,
        ..Default::default()
    }
}

/// Assert a job is in the expected state.
pub fn assert_job_state(job: &Job, expected: JobState) {
    assert_eq!(
        job.state, expected,
        "job {} expected state {:?}, got {:?}",
        job.job_id, expected, job.state
    );
}

/// Assert a job transitioned successfully.
pub fn assert_transition_ok(job: &mut Job, to: JobState) {
    let result = job.transition(to);
    assert!(
        result.is_ok(),
        "job {} transition to {:?} should succeed, got {:?}",
        job.job_id,
        to,
        result
    );
}

/// Assert a job transition is rejected.
pub fn assert_transition_err(job: &mut Job, to: JobState) {
    let result = job.transition(to);
    assert!(
        result.is_err(),
        "job {} transition to {:?} should fail",
        job.job_id,
        to
    );
}

/// Parse a test script with #SBATCH directives.
pub fn test_script(directives: &[&str], body: &str) -> String {
    let mut script = String::from("#!/bin/bash\n");
    for d in directives {
        script.push_str(&format!("#SBATCH {}\n", d));
    }
    script.push('\n');
    script.push_str(body);
    script.push('\n');
    script
}

/// Minimal TOML config for testing.
pub fn test_config() -> SlurmConfig {
    SlurmConfig::from_str(
        r#"
cluster_name = "test-cluster"

[[partitions]]
name = "default"
default = true
nodes = "node[001-008]"
max_time = "24:00:00"

[[partitions]]
name = "gpu"
nodes = "gpu[001-004]"
max_time = "72:00:00"

[[nodes]]
names = "node[001-008]"
cpus = 64
memory_mb = 256000

[[nodes]]
names = "gpu[001-004]"
cpus = 128
memory_mb = 512000
gres = ["gpu:mi300x:8"]
"#,
    )
    .expect("test config should parse")
}
