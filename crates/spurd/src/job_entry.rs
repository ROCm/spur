// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

/// Namespace, cgroup, and isolation metadata for a tracked job, used to build
/// the correct `nsenter` arguments for entering the job's execution context.
#[derive(Debug, Clone)]
pub struct JobEntry {
    pub pid: i32,
    pub has_pid_namespace: bool,
    pub has_user_namespace: bool,
    pub has_mount_namespace: bool,
    pub uid: u32,
    pub gid: u32,
    pub work_dir: String,
    /// The job's cgroup, when one exists. It carries the job's resource limits
    /// and device filter, so anything launched into the job belongs in it.
    pub cgroup_path: Option<PathBuf>,
}

impl JobEntry {
    /// Build `nsenter` arguments for entering this job's namespaces.
    ///
    /// Returns the arguments to pass between `nsenter` and `--`.
    /// The caller prepends `nsenter` and appends `-- <command>`.
    ///
    /// `self.pid` is the process that called `unshare(CLONE_NEWPID)` (the batch
    /// wrapper or container parent). That process stays in its *original* pid
    /// namespace — only its children, and the `/proc` mounted inside the job,
    /// live in the new pid namespace. Entering via `/proc/<pid>/ns/pid` would
    /// therefore land in the wrong (host) pid namespace, where `/proc` does not
    /// match: any tool that reads `/proc/self/*` (e.g. `setpriv` activating
    /// capabilities) then fails with ENOENT. Enter through `pid_for_children`,
    /// which points at the job's real pid namespace. (For a process already
    /// settled inside a namespace, `pid_for_children` equals its own pid ns, so
    /// this is correct in every case.)
    pub fn nsenter_args(&self) -> Vec<String> {
        let mut args = vec!["--target".to_string(), self.pid.to_string()];

        if self.has_user_namespace {
            args.push("--user".to_string());
        }
        if self.has_mount_namespace {
            args.push("--mount".to_string());
        }
        if self.has_pid_namespace {
            args.push(format!("--pid=/proc/{}/ns/pid_for_children", self.pid));
        }

        args
    }

    /// Whether nsenter is useful (at least one namespace to enter).
    pub fn has_namespaces(&self) -> bool {
        self.has_pid_namespace || self.has_user_namespace || self.has_mount_namespace
    }

    /// Build env vars that should be set in the entered context.
    pub fn env_vars(&self, job_id: u32) -> Vec<(String, String)> {
        vec![
            ("SPUR_JOB_ID".into(), job_id.to_string()),
            ("SLURM_JOB_ID".into(), job_id.to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nsenter_args_root_non_container() {
        let entry = JobEntry {
            pid: 1234,
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
            uid: 1000,
            gid: 1000,
            work_dir: "/home/user".into(),
            cgroup_path: None,
        };
        let args = entry.nsenter_args();
        assert_eq!(
            args,
            vec![
                "--target",
                "1234",
                "--mount",
                "--pid=/proc/1234/ns/pid_for_children"
            ]
        );
    }

    #[test]
    fn nsenter_args_root_container() {
        let entry = JobEntry {
            pid: 5678,
            has_pid_namespace: true,
            has_user_namespace: true,
            has_mount_namespace: true,
            uid: 0,
            gid: 0,
            work_dir: "/".into(),
            cgroup_path: None,
        };
        let args = entry.nsenter_args();
        assert_eq!(
            args,
            vec![
                "--target",
                "5678",
                "--user",
                "--mount",
                "--pid=/proc/5678/ns/pid_for_children"
            ]
        );
    }

    #[test]
    fn nsenter_args_non_root_no_namespaces() {
        let entry = JobEntry {
            pid: 9999,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            uid: 1000,
            gid: 1000,
            work_dir: "/tmp".into(),
            cgroup_path: None,
        };
        assert!(!entry.has_namespaces());
        let args = entry.nsenter_args();
        assert_eq!(args, vec!["--target", "9999"]);
    }

    #[test]
    fn nsenter_args_rootless_container() {
        let entry = JobEntry {
            pid: 4444,
            has_pid_namespace: true,
            has_user_namespace: true,
            has_mount_namespace: true,
            uid: 1000,
            gid: 1000,
            work_dir: "/".into(),
            cgroup_path: None,
        };
        let args = entry.nsenter_args();
        assert_eq!(
            args,
            vec![
                "--target",
                "4444",
                "--user",
                "--mount",
                "--pid=/proc/4444/ns/pid_for_children"
            ]
        );
        assert!(entry.has_namespaces());
    }
}
