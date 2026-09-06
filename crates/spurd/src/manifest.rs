// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Persisted job manifests for re-adoption across a spurd restart.
//!
//! A batch job's workload survives a spurd restart in its cgroup, but the agent
//! starts with an empty job map and loses the OS process handle. Without a
//! record the job is orphaned: the agent refuses to acknowledge it and the
//! controller keeps reporting it RUNNING forever (spur#803).
//!
//! On launch the agent writes a small manifest per job; on startup it reads them
//! back and re-adopts every job whose cgroup still holds live processes, and
//! discards the manifests of jobs that ended while the agent was down.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

/// Durable manifest directory (under the node-owned spool root, not `/tmp`).
const MANIFEST_DIR: &str = "/var/spool/spur/manifests";

/// Enough of a job's state to re-track and manage it through its cgroup after a
/// restart. The process handle and exit code cannot be recovered, so a
/// re-adopted job is managed via its cgroup and reports a zero exit when it ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobManifest {
    pub job_id: u32,
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub work_dir: String,
    pub partition: String,
    pub nodelist: String,
    pub gpu_devices: Vec<u32>,
    pub cpus: u32,
    pub memory_mb: u64,
    pub mpi: String,
    pub run_attempt: u32,
    pub cgroup_path: String,
    pub has_pid_namespace: bool,
    pub has_user_namespace: bool,
    pub has_mount_namespace: bool,
}

fn manifest_path(job_id: u32) -> PathBuf {
    PathBuf::from(MANIFEST_DIR).join(format!("{job_id}.json"))
}

/// Persist a job's manifest. Best-effort: a failure only costs re-adoption of
/// this one job after a restart, so it must never fail the launch.
pub fn write(manifest: &JobManifest) {
    if let Err(e) = std::fs::create_dir_all(MANIFEST_DIR) {
        warn!(error = %e, "could not create job manifest dir; re-adoption disabled for this job");
        return;
    }
    match serde_json::to_vec_pretty(manifest) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(manifest_path(manifest.job_id), bytes) {
                warn!(job_id = manifest.job_id, error = %e, "could not write job manifest");
            }
        }
        Err(e) => warn!(job_id = manifest.job_id, error = %e, "could not serialize job manifest"),
    }
}

/// Remove a job's manifest once it has ended.
pub fn remove(job_id: u32) {
    let _ = std::fs::remove_file(manifest_path(job_id));
}

/// Load all persisted manifests, skipping any that are unreadable or corrupt.
pub fn load_all() -> Vec<JobManifest> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(MANIFEST_DIR) {
        Ok(e) => e,
        Err(_) => return out, // no manifests (fresh node, or nothing was running)
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<JobManifest>(&b).ok())
        {
            Some(m) => out.push(m),
            None => warn!(path = %path.display(), "skipping unreadable job manifest"),
        }
    }
    out
}

/// Whether a cgroup currently holds any process, i.e. the job is still alive.
pub fn cgroup_has_live_procs(cgroup_path: &str) -> bool {
    std::fs::read_to_string(Path::new(cgroup_path).join("cgroup.procs"))
        .map(|s| s.lines().any(|l| !l.trim().is_empty()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JobManifest {
        JobManifest {
            job_id: 42,
            uid: 1000,
            gid: 1000,
            user: "alice".into(),
            work_dir: "/home/alice".into(),
            partition: "gpu".into(),
            nodelist: "node0".into(),
            gpu_devices: vec![0, 1],
            cpus: 8,
            memory_mb: 4096,
            mpi: "pmix".into(),
            run_attempt: 1,
            cgroup_path: "/sys/fs/cgroup/spur/job_42".into(),
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
        }
    }

    #[test]
    fn manifest_survives_a_json_roundtrip() {
        let m = sample();
        let bytes = serde_json::to_vec(&m).unwrap();
        let back: JobManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.job_id, m.job_id);
        assert_eq!(back.cgroup_path, m.cgroup_path);
        assert_eq!(back.gpu_devices, m.gpu_devices);
        assert_eq!(back.run_attempt, m.run_attempt);
        assert_eq!(back.has_pid_namespace, m.has_pid_namespace);
    }

    #[test]
    fn cgroup_liveness_reflects_procs_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let cg = dir.path().to_str().unwrap();

        // No cgroup.procs at all → treated as dead (nothing to re-adopt).
        assert!(!cgroup_has_live_procs(cg));

        // Empty file → dead.
        std::fs::write(dir.path().join("cgroup.procs"), b"").unwrap();
        assert!(!cgroup_has_live_procs(cg));

        // Whitespace only → dead (a trailing newline is not a live pid).
        std::fs::write(dir.path().join("cgroup.procs"), b"\n").unwrap();
        assert!(!cgroup_has_live_procs(cg));

        // A pid present → alive.
        std::fs::write(dir.path().join("cgroup.procs"), b"12345\n").unwrap();
        assert!(cgroup_has_live_procs(cg));
    }
}
