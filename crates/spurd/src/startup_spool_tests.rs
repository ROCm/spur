// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::startup_spool::{ActivityPaths, CleanupSummary, STARTUP_CLEANUP_BUDGET};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

struct SpoolFixture {
    directory: tempfile::TempDir,
    agent: AgentService,
    root: PathBuf,
}

impl SpoolFixture {
    fn new(controller: &ControllerFixture, ids: &[u32]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("spool");
        for path in [
            &root,
            &directory.path().join("proc"),
            &directory.path().join("cgroup/spur"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(
            directory.path().join("cgroup/cgroup.controllers"),
            "cpu memory",
        )
        .unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        for id in ids {
            let job = root.join(format!("job{id}"));
            fs::create_dir(&job).unwrap();
            fs::write(job.join("output"), "retained output").unwrap();
        }
        let mut agent = agent_for_directory(controller, directory.path())
            .with_job_spool_roots(vec![root.clone(), root.clone()]);
        agent.cgroup.enabled = true;
        Self {
            directory,
            agent,
            root,
        }
    }

    fn paths(&self) -> ActivityPaths {
        ActivityPaths {
            proc_root: self.directory.path().join("proc"),
            cgroup_root: self.directory.path().join("cgroup/spur"),
            runtime_root: self.directory.path().join("runtime"),
            cgroups_enabled: self.agent.cgroup.enabled,
            supervisor_uid: nix::unistd::geteuid().as_raw(),
            budget: STARTUP_CLEANUP_BUDGET,
            worker_barriers: None,
        }
    }

    async fn run(&self) -> CleanupSummary {
        self.agent
            .cleanup_startup_job_spools_with_inventory(self.paths())
            .await
    }

    fn exists(&self, id: u32) -> bool {
        self.root.join(format!("job{id}")).is_dir()
    }

    fn process(&self, pid: u32, command: &[&str], environment: &[u8]) -> PathBuf {
        let path = self.directory.path().join("proc").join(pid.to_string());
        fs::create_dir_all(path.join("fd")).unwrap();
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "1234";
        fs::write(
            path.join("stat"),
            format!("{pid} (process) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(path.join("cmdline"), command.join("\0").as_bytes()).unwrap();
        fs::write(
            path.join("comm"),
            format!("{}\n", command.first().unwrap_or(&"process")),
        )
        .unwrap();
        fs::write(path.join("environ"), environment).unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        fs::write(
            path.join("status"),
            format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        symlink(self.directory.path(), path.join("cwd")).unwrap();
        if let Some(command) = command.first() {
            symlink(command, path.join("exe")).unwrap();
        }
        path
    }
}

fn spool_response(controller: &ControllerFixture, unrestricted: bool, jobs: &[(u32, i32)]) {
    *controller.state.spool_response.lock().unwrap() = spur_proto::proto::GetJobsResponse {
        jobs: jobs
            .iter()
            .map(|&(job_id, state)| spur_proto::proto::JobInfo {
                job_id,
                state,
                ..Default::default()
            })
            .collect(),
        unrestricted_visibility: unrestricted,
        authoritative: true,
        next_job_id: u32::MAX,
    };
}

#[tokio::test]
async fn startup_spool_deletes_terminal_and_purged_jobs_once() {
    use spur_proto::proto::JobState;
    let controller = ControllerFixture::new(true);
    spool_response(
        &controller,
        true,
        &[
            (1, JobState::JobCompleted as i32),
            (2, JobState::JobRunning as i32),
            (4, i32::MAX),
        ],
    );
    let fixture = SpoolFixture::new(&controller, &[1, 2, 3, 4]);
    for name in [
        "job0",
        "job01",
        "job-1",
        "job4294967296",
        "jobs",
        "containers",
    ] {
        fs::create_dir(fixture.root.join(name)).unwrap();
    }
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 2);
    assert_eq!(summary.retained, 2);
    assert!(!fixture.exists(1));
    assert!(!fixture.exists(3));
    assert!(fixture.exists(2));
    assert!(fixture.exists(4));
    for name in [
        "job0",
        "job01",
        "job-1",
        "job4294967296",
        "jobs",
        "containers",
    ] {
        assert!(fixture.root.join(name).is_dir());
    }
    let queries = controller.state.spool_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0],
        spur_proto::proto::GetJobsRequest {
            job_ids: vec![1, 2, 3, 4],
            require_authoritative: true,
            ..Default::default()
        }
    );
    assert_eq!(fixture.run().await.deleted, 0);
}

#[tokio::test]
async fn startup_spool_preserves_missing_jobs_without_unrestricted_visibility() {
    let controller = ControllerFixture::new(true);
    spool_response(
        &controller,
        false,
        &[(1, spur_proto::proto::JobState::JobFailed as i32)],
    );
    let fixture = SpoolFixture::new(&controller, &[1, 2]);
    assert_eq!(fixture.run().await.deleted, 1);
    assert!(fixture.exists(2));
    // A legacy empty response has the same conservative absence semantics.
    *controller.state.spool_response.lock().unwrap() = Default::default();
    assert_eq!(fixture.run().await.deleted, 0);
    assert!(fixture.exists(2));
}

#[tokio::test]
async fn startup_spool_preserves_stale_terminal_and_missing_jobs_without_authority() {
    let controller = ControllerFixture::new(true);
    let fixture = SpoolFixture::new(&controller, &[1, 2]);
    for unrestricted in [false, true] {
        spool_response(
            &controller,
            unrestricted,
            &[(1, spur_proto::proto::JobState::JobCancelled as i32)],
        );
        controller
            .state
            .spool_response
            .lock()
            .unwrap()
            .authoritative = false;
        let summary = fixture.run().await;
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.retained, 2);
        assert_eq!(
            fs::read_to_string(fixture.root.join("job1/output")).unwrap(),
            "retained output"
        );
        assert!(fixture.exists(2));
    }
    assert!(controller
        .state
        .spool_queries
        .lock()
        .unwrap()
        .iter()
        .all(|request| request.require_authoritative));
    controller
        .state
        .spool_response
        .lock()
        .unwrap()
        .authoritative = true;
    assert_eq!(fixture.run().await.deleted, 2);
    assert!(!fixture.exists(1));
    assert!(!fixture.exists(2));
}

#[tokio::test]
async fn startup_spool_stops_after_a_failed_sorted_batch_and_retries_next_startup() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    controller.state.spool_fail_at.store(2, Ordering::SeqCst);
    let fixture = SpoolFixture::new(&controller, &(1..=300).rev().collect::<Vec<_>>());
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 128);
    assert_eq!(summary.retained, 172);
    let queries = controller.state.spool_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].job_ids, (1..=128).collect::<Vec<_>>());
    assert_eq!(queries[1].job_ids, (129..=256).collect::<Vec<_>>());
    controller.state.spool_fail_at.store(0, Ordering::SeqCst);
    assert_eq!(fixture.run().await.deleted, 172);
    assert_eq!(fixture.run().await.deleted, 0);
}

#[tokio::test]
async fn startup_spool_preserves_all_attempts_steps_and_replay_sessions() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let mut tracked = TrackedJob::allocation_only(None);
    tracked.run_attempt = 9;
    fixture.agent.running.lock().await.insert(1, tracked);
    fixture
        .agent
        .allocation
        .lock()
        .await
        .allocate_for_job(2, 10, 1, 128, &[])
        .unwrap();
    fixture.agent.active_steps.lock().await.insert(
        (3, 42),
        ActiveStep {
            run_attempt: 12,
            ..Default::default()
        },
    );
    let descriptor = StepdDescriptor::new(
        4,
        13,
        7,
        0,
        0,
        fixture.root.join("socket"),
        fixture.root.join("cgroup"),
    );
    fixture.agent.stepds.lock().await.insert((4, 7), descriptor);
    let store = StepdStore::new(fixture.directory.path());
    for (id, attempt, step) in [(5, 15, spur_core::step::STEP_BATCH), (6, 16, 3)] {
        let session = store.prepare_session_dir(id, attempt, step).unwrap();
        let descriptor = StepdDescriptor::new(
            id,
            attempt,
            step,
            0,
            0,
            session.join("socket"),
            fixture.root.join("cgroup"),
        );
        store.publish(&descriptor).unwrap();
        store
            .obligations(id, attempt, step)
            .append(&StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 0,
                synthesized: false,
            })
            .unwrap();
    }
    fs::create_dir(store.root().join("7.bad.metadata")).unwrap();
    fs::write(
        store.root().join("7.bad.metadata/descriptor.json"),
        "invalid JSON",
    )
    .unwrap();
    let group = fixture.paths().cgroup_root.join("job_8_18");
    fs::create_dir(&group).unwrap();
    fs::write(group.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    fs::write(group.join("cgroup.kill"), "untouched").unwrap();
    fs::create_dir(group.join("step_99")).unwrap();
    fs::write(group.join("step_99/cgroup.events"), "populated 1\n").unwrap();
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 1);
    assert_eq!(summary.retained, 8);
    for id in 1..=8 {
        assert!(fixture.exists(id), "job {id} must be retained");
    }
    assert!(!fixture.exists(9));
    assert_eq!(
        fs::read_to_string(group.join("cgroup.kill")).unwrap(),
        "untouched"
    );
}

#[tokio::test]
async fn startup_spool_finds_recordless_supervisors_workloads_and_output_holders() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1, 2, 3, 4, 5, 6, 7]);
    fixture.process(
        101,
        &[
            "spurstepd",
            "/missing-state",
            "1",
            "5",
            "/missing-launch.json",
        ],
        b"",
    );
    fixture.process(102, &["workload"], b"SPUR_JOB_ID=2\0");
    fixture.process(103, &["sibling-step"], b"SLURM_JOB_ID=3\0");
    let holder = fixture.process(104, &["output-reader"], b"");
    symlink(fixture.root.join("job4/output"), holder.join("fd/1")).unwrap();
    fs::remove_file(holder.join("cwd")).unwrap();
    symlink(fixture.root.join("job5"), holder.join("cwd")).unwrap();
    let alias = fixture.directory.path().join("output-alias");
    fs::hard_link(fixture.root.join("job6/output"), &alias).unwrap();
    symlink(alias, holder.join("fd/2")).unwrap();
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 1);
    assert_eq!(summary.retained, 6);
    for id in 1..=6 {
        assert!(fixture.exists(id));
    }
    assert!(!fixture.exists(7));
}

#[tokio::test]
async fn startup_spool_ignores_job_markers_that_name_no_job() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1, 2]);
    fixture.process(100, &["workload"], b"SPUR_JOB_ID=bad\0SLURM_JOB_ID=\0");
    fixture.process(101, &["workload"], b"SLURM_JOB_ID=2\0");
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 1);
    assert!(!fixture.exists(1));
    assert!(fixture.exists(2));
}

#[tokio::test]
async fn startup_spool_trusts_supervisor_names_only_from_the_daemon_uid() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1]);
    let other = nix::unistd::geteuid().as_raw().wrapping_add(1);
    for (pid, status) in [
        (100, format!("Uid:\t{other}\t{other}\t{other}\t{other}\n")),
        (101, format!("Uid:\t{other}\t0\t0\t0\n")),
    ] {
        let process = fixture.process(pid, &["spurstepd", "--help"], b"");
        fs::write(process.join("status"), status).unwrap();
    }
    assert_eq!(fixture.run().await.deleted, 1);
    assert!(!fixture.exists(1));
}

#[tokio::test]
async fn startup_spool_preserves_uncertain_activity_inventories() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    for fault in ["proc", "supervisor", "cgroup", "runtime"] {
        let fixture = SpoolFixture::new(&controller, &[1]);
        match fault {
            "proc" => {
                fs::remove_dir(fixture.paths().proc_root.clone()).unwrap();
                fs::write(fixture.paths().proc_root, "unreadable directory").unwrap();
            }
            "supervisor" => {
                fixture.process(100, &["spurstepd"], b"");
            }
            "cgroup" => {
                let group = fixture.paths().cgroup_root.join("job_1_1");
                fs::create_dir(&group).unwrap();
                fs::write(group.join("cgroup.events"), "populated unknown\n").unwrap();
            }
            "runtime" => {
                fs::create_dir_all(fixture.paths().runtime_root.join("unidentified")).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(fixture.run().await.deleted, 0, "{fault}");
        assert!(fixture.exists(1), "{fault}");
    }
}

#[tokio::test]
async fn startup_spool_skips_unsafe_roots_and_disabled_cgroups() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let mut fixture = SpoolFixture::new(&controller, &[1]);
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(fixture.run().await.deleted, 0);
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.agent.cgroup.enabled = false;
    assert_eq!(fixture.run().await.deleted, 0);
    assert!(fixture.exists(1));
    assert!(controller.state.spool_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn startup_spool_follows_root_symlinks_but_nothing_beneath_the_root() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let mut fixture = SpoolFixture::new(&controller, &[1, 3, 4]);
    let target = fixture.directory.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), "safe").unwrap();
    symlink(&target, fixture.root.join("job2")).unwrap();
    symlink(&target, fixture.root.join("job1/link")).unwrap();
    let alias = fixture.directory.path().join("root-link");
    symlink(&fixture.root, &alias).unwrap();
    fixture.agent.job_spool_roots = vec![alias.clone()];
    let holder = fixture.process(100, &["output-reader"], b"");
    symlink(fixture.root.join("job4/output"), holder.join("fd/1")).unwrap();
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 2);
    assert!(!fixture.exists(1));
    assert!(!fixture.exists(3));
    assert!(
        fixture.exists(4),
        "holders are matched against the resolved root"
    );
    assert_eq!(fs::read_to_string(target.join("sentinel")).unwrap(), "safe");
    assert!(fixture.root.join("job2").is_symlink());

    let writable = fixture.directory.path().join("writable");
    fs::create_dir_all(writable.join("job5")).unwrap();
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
    let planted = fixture.directory.path().join("planted-link");
    symlink(&writable, &planted).unwrap();
    fixture.agent.job_spool_roots = vec![planted];
    assert_eq!(fixture.run().await.deleted, 0);
    assert!(writable.join("job5").is_dir());
}

#[tokio::test]
async fn startup_spool_treats_only_issued_ids_as_confirmed_absent() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    controller.state.spool_response.lock().unwrap().next_job_id = 3;
    let fixture = SpoolFixture::new(&controller, &[1, 2, 3, 4]);
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 2);
    assert!(!fixture.exists(1));
    assert!(!fixture.exists(2));
    assert!(fixture.exists(3));
    assert!(fixture.exists(4));
    controller.state.spool_response.lock().unwrap().next_job_id = 0;
    assert_eq!(fixture.run().await.deleted, 0);
}

#[tokio::test]
async fn startup_spool_requires_every_duplicate_record_to_be_terminal() {
    use spur_proto::proto::JobState;
    let controller = ControllerFixture::new(true);
    spool_response(
        &controller,
        true,
        &[
            (1, JobState::JobCompleted as i32),
            (1, JobState::JobPending as i32),
            (2, JobState::JobCompleted as i32),
            (2, JobState::JobFailed as i32),
        ],
    );
    let fixture = SpoolFixture::new(&controller, &[1, 2]);
    assert_eq!(fixture.run().await.deleted, 1);
    assert!(fixture.exists(1));
    assert!(!fixture.exists(2));
}

#[tokio::test]
async fn startup_spool_retains_everything_once_its_budget_is_spent() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1, 2]);
    let mut paths = fixture.paths();
    paths.budget = std::time::Duration::ZERO;
    let summary = fixture
        .agent
        .cleanup_startup_job_spools_with_inventory(paths)
        .await;
    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.retained, 2);
    assert!(fixture.exists(1));
    assert!(fixture.exists(2));
}

#[tokio::test]
async fn startup_spool_rechecks_launch_ownership_after_controller_query() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = Arc::new(SpoolFixture::new(&controller, &[1]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *controller.state.spool_query_barrier.lock().unwrap() =
        Some((entered.clone(), release.clone()));
    let cleanup = tokio::spawn({
        let fixture = fixture.clone();
        async move { fixture.run().await }
    });
    entered.notified().await;
    let launch_guard = fixture.agent.lifecycle.acquire(1).await;
    fixture
        .agent
        .running
        .lock()
        .await
        .insert(1, TrackedJob::allocation_only(None));
    release.notify_one();
    drop(launch_guard);
    assert_eq!(cleanup.await.unwrap().deleted, 0);
    assert!(fixture.exists(1));
}

#[tokio::test]
async fn startup_spool_retries_a_deletion_error_on_the_next_startup() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = Arc::new(SpoolFixture::new(&controller, &[1]));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *controller.state.spool_query_barrier.lock().unwrap() =
        Some((entered.clone(), release.clone()));
    let cleanup = tokio::spawn({
        let fixture = fixture.clone();
        async move { fixture.run().await }
    });
    entered.notified().await;
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o777)).unwrap();
    release.notify_one();
    let summary = cleanup.await.unwrap();
    assert_eq!(summary.errors, 1);
    assert!(fixture.exists(1));
    *controller.state.spool_query_barrier.lock().unwrap() = None;
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fixture.run().await.deleted, 1);
    assert_eq!(fixture.run().await.deleted, 0);
}

#[tokio::test]
async fn startup_spool_preserves_replacement_directories() {
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    for replace_root in [false, true] {
        let fixture = Arc::new(SpoolFixture::new(&controller, &[1]));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *controller.state.spool_query_barrier.lock().unwrap() =
            Some((entered.clone(), release.clone()));
        let cleanup = tokio::spawn({
            let fixture = fixture.clone();
            async move { fixture.run().await }
        });
        entered.notified().await;
        let replaced = if replace_root {
            fixture.root.clone()
        } else {
            fixture.root.join("job1")
        };
        let saved = fixture.directory.path().join("saved");
        fs::rename(&replaced, &saved).unwrap();
        fs::create_dir(&replaced).unwrap();
        fs::set_permissions(&replaced, fs::Permissions::from_mode(0o755)).unwrap();
        let output = if replace_root {
            fs::create_dir(replaced.join("job1")).unwrap();
            replaced.join("job1/output")
        } else {
            replaced.join("output")
        };
        fs::write(&output, "new generation").unwrap();
        release.notify_one();
        let summary = cleanup.await.unwrap();
        assert_eq!(summary.deleted, 0);
        assert_eq!(fs::read_to_string(output).unwrap(), "new generation");
        let old_output = if replace_root {
            saved.join("job1/output")
        } else {
            saved.join("output")
        };
        assert_eq!(fs::read_to_string(old_output).unwrap(), "retained output");
    }
}

#[tokio::test]
async fn startup_spool_excludes_launch_even_if_the_startup_future_is_cancelled() {
    use std::future::Future;
    use std::task::Poll;
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = Arc::new(SpoolFixture::new(&controller, &[1]));
    let barriers = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
    let cleanup = tokio::spawn({
        let fixture = fixture.clone();
        let barriers = barriers.clone();
        async move {
            let mut paths = fixture.paths();
            paths.worker_barriers = Some(barriers);
            fixture
                .agent
                .cleanup_startup_job_spools_with_inventory(paths)
                .await
        }
    });
    tokio::task::spawn_blocking({
        let barriers = barriers.clone();
        move || barriers.0.wait()
    })
    .await
    .unwrap();
    let mut launch = Box::pin(fixture.agent.lifecycle.acquire(1));
    std::future::poll_fn(|context| {
        assert!(matches!(launch.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    cleanup.abort();
    assert!(cleanup.await.unwrap_err().is_cancelled());
    std::future::poll_fn(|context| {
        assert!(matches!(launch.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    barriers.1.wait();
    let _launch_guard = launch.await;
    assert!(!fixture.exists(1));
}

#[tokio::test]
#[ignore = "requires unshare with user and mount namespaces"]
async fn startup_spool_keeps_bind_mounts_and_their_targets_untouched() {
    const CHILD: &str = "SPUR_SPOOL_MOUNT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "agent_server::cleanup_lifecycle_tests::startup_spool_tests::startup_spool_keeps_bind_mounts_and_their_targets_untouched", "--ignored"])
            .env(CHILD, "1")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    let controller = ControllerFixture::new(true);
    spool_response(&controller, true, &[]);
    let fixture = SpoolFixture::new(&controller, &[1, 2, 3]);
    let target = fixture.directory.path().join("mount-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), "untouched").unwrap();
    let nested = fixture.root.join("job2/mounted");
    fs::create_dir(&nested).unwrap();
    for mountpoint in [fixture.root.join("job1"), nested.clone()] {
        nix::mount::mount(
            Some(target.as_path()),
            &mountpoint,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .unwrap();
    }
    let summary = fixture.run().await;
    assert_eq!(summary.deleted, 1);
    assert!(fixture.exists(1));
    assert!(fixture.exists(2));
    assert!(
        !fixture.exists(3),
        "a mount in one spool must not hold back the others"
    );
    assert_eq!(
        fs::read_to_string(target.join("sentinel")).unwrap(),
        "untouched"
    );
    for mountpoint in [fixture.root.join("job1"), nested] {
        nix::mount::umount(&mountpoint).unwrap();
    }
}
