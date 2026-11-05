// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fs;

#[tokio::test]
async fn fencing_discovers_every_attempt_with_reused_step_ids() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::stepd::StepdStore::new(root.path().join("state"));
    let cgroups = root.path().join("cgroups");
    fs::create_dir(&cgroups).unwrap();
    let sessions = Arc::new(Mutex::new(StepdMap::new()));
    let completions = crate::step_completion::StepCompletions::new();
    for attempt in 1..=3 {
        for step in [spur_core::step::STEP_BATCH, 0] {
            let path = cgroups
                .join(format!("job_79_{attempt}"))
                .join(spur_core::step::step_dir_name(step));
            fs::create_dir_all(&path).unwrap();
            let descriptor = crate::stepd::StepdDescriptor::new(
                79,
                attempt,
                step,
                0,
                0,
                store.session_dir(79, attempt, step).join("runtime.sock"),
                path,
            );
            store.publish(&descriptor).unwrap();
            sessions
                .lock()
                .await
                .insert(stepd_key(&descriptor), descriptor);
        }
    }
    for name in ["job_79_4", "job_79_5", "job_790_1"] {
        fs::create_dir(cgroups.join(name)).unwrap();
    }
    assert_eq!(sessions.lock().await.len(), 2);

    fence_older_runs(&sessions, &completions, &store, &cgroups, 79, 4)
        .await
        .unwrap();

    for attempt in 1..=3 {
        assert!(!cgroups.join(format!("job_79_{attempt}")).exists());
        for step in [spur_core::step::STEP_BATCH, 0] {
            assert!(completions.settled(79, attempt, step).await.is_some());
        }
    }
    assert!(sessions.lock().await.is_empty());
    for name in ["job_79_4", "job_79_5", "job_790_1"] {
        assert!(cgroups.join(name).is_dir());
    }
}

#[tokio::test]
async fn orphan_parents_are_reaped_without_a_session_record() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::stepd::StepdStore::new(root.path().join("state"));
    for attempt in 1..=3 {
        fs::create_dir(root.path().join(format!("job_79_{attempt}"))).unwrap();
    }
    fence_older_runs(
        &Arc::new(Mutex::new(StepdMap::new())),
        &crate::step_completion::StepCompletions::new(),
        &store,
        root.path(),
        79,
        4,
    )
    .await
    .unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn an_unremovable_parent_refuses_the_fence_even_without_steps() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::stepd::StepdStore::new(root.path().join("state"));
    let parent = root.path().join("job_79_1");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("cgroup.kill"), "").unwrap();
    fs::write(parent.join("cgroup.events"), "populated 0\n").unwrap();
    assert!(fence_older_runs(
        &Arc::new(Mutex::new(StepdMap::new())),
        &crate::step_completion::StepCompletions::new(),
        &store,
        root.path(),
        79,
        4,
    )
    .await
    .is_err());
    assert!(parent.exists());
}

#[test]
fn unreadable_old_sessions_are_not_silently_skipped() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::stepd::StepdStore::new(root.path());
    let session = store.session_dir(79, 1, 0);
    fs::create_dir_all(&session).unwrap();
    fs::write(session.join("descriptor.json"), "broken").unwrap();
    assert!(store.older_sessions(79, 4).is_err());
    assert!(store.older_sessions(79, 1).unwrap().is_empty());
    assert!(store.older_sessions(790, 4).unwrap().is_empty());
}

#[tokio::test]
async fn an_unreadable_exit_refuses_the_fence_without_inventing_an_outcome() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::stepd::StepdStore::new(root.path().join("state"));
    let cgroups = root.path().join("cgroups");
    let cgroup = cgroups.join("job_79_1/step_0");
    fs::create_dir_all(&cgroup).unwrap();
    let descriptor = crate::stepd::StepdDescriptor::new(
        79,
        1,
        0,
        0,
        0,
        store.session_dir(79, 1, 0).join("runtime.sock"),
        cgroup.clone(),
    );
    store.publish(&descriptor).unwrap();
    fs::write(
        store.session_dir(79, 1, 0).join("obligations.jsonl"),
        "broken\n",
    )
    .unwrap();
    let completions = crate::step_completion::StepCompletions::new();

    let error = fence_older_runs(
        &Arc::new(Mutex::new(StepdMap::new())),
        &completions,
        &store,
        &cgroups,
        79,
        2,
    )
    .await
    .expect_err("an unreadable exit cannot be replaced with a synthetic exit");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(completions.settled(79, 1, 0).await, None);
    assert!(!cgroup.exists());
}

#[test]
fn cgroup_discovery_does_not_follow_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("job_79_1")).unwrap();
    assert!(older_run_cgroups(root.path(), 79, 4).is_err());
    assert!(outside.path().is_dir());
}
