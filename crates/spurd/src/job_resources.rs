// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-job CPU, memory, and disk readings from the job cgroup spurd already
//! creates. A job with no cgroup directory is omitted: there is nothing to
//! attribute, and a zero series would look like a real idle sample.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use spur_core::job_usage::JobUsage;

/// Samples held jobs and keeps their disk byte totals monotonic.
///
/// The per-process disk sum falls when a process is reaped outside the job,
/// and the controller exports it as a counter, so only growth is added.
#[derive(Debug, Default)]
pub struct JobUsageSampler {
    disk: HashMap<(u32, u32), DiskTotal>,
}

#[derive(Debug, Default, Clone, Copy)]
struct DiskTotal {
    last_read: u64,
    last_write: u64,
    read: u64,
    write: u64,
}

impl DiskTotal {
    fn advance(&mut self, read: u64, write: u64) {
        self.read = self
            .read
            .saturating_add(read.saturating_sub(self.last_read));
        self.write = self
            .write
            .saturating_add(write.saturating_sub(self.last_write));
        self.last_read = read;
        self.last_write = write;
    }
}

impl JobUsageSampler {
    /// Readings for the held jobs that have a readable cgroup under
    /// `cgroup_root`. `proc_root` is `/proc` in the agent and a fixture in tests.
    pub fn sample(
        &mut self,
        cgroup_root: &Path,
        proc_root: &Path,
        held_ids: &[u32],
    ) -> Vec<JobUsage> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for &job_id in held_ids {
            let Some((attempt, dir)) = newest_attempt_dir(cgroup_root, job_id) else {
                continue;
            };
            let Some(mut usage) = read_cgroup_usage(job_id, &dir, proc_root) else {
                continue;
            };
            let total = self.disk.entry((job_id, attempt)).or_default();
            total.advance(usage.disk_read_bytes, usage.disk_write_bytes);
            usage.disk_read_bytes = total.read;
            usage.disk_write_bytes = total.write;
            seen.push((job_id, attempt));
            out.push(usage);
        }
        self.disk.retain(|key, _| seen.contains(key));
        out.sort_by_key(|usage| usage.job_id);
        out
    }
}

/// Highest `job_<id>_<attempt>` directory. Attempt numbers are not fixed width,
/// so a lexical max would pick attempt 9 over attempt 10.
fn newest_attempt_dir(root: &Path, job_id: u32) -> Option<(u32, PathBuf)> {
    let prefix = format!("job_{job_id}_");
    let entries = std::fs::read_dir(root).ok()?;
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(attempt) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(attempt) = attempt.parse::<u32>() else {
            continue;
        };
        let replace = best.as_ref().is_none_or(|(current, _)| attempt > *current);
        if replace {
            best = Some((attempt, entry.path()));
        }
    }
    best
}

/// `None` when `cpu.stat` cannot be read, e.g. the cgroup is being removed as
/// the job ends: a zero would read as a real sample and a counter reset.
fn read_cgroup_usage(job_id: u32, dir: &Path, proc_root: &Path) -> Option<JobUsage> {
    // cpu.stat and memory.swap.current on the job cgroup already include every
    // step leaf. memory.current counts page cache, so RSS and virtual size are
    // summed from each process instead. The io controller is not delegated, so
    // disk bytes come from /proc/<pid>/io rather than io.stat.
    let cpu_usage_usec = usage_usec(&file_text(&dir.join("cpu.stat"))?)?;
    let swap_bytes = file_text(&dir.join("memory.swap.current"))
        .as_deref()
        .map(first_u64)
        .unwrap_or(0);

    let mut pids = BTreeSet::new();
    pids.extend(cgroup_pids(dir));
    for child in child_dirs(dir) {
        pids.extend(cgroup_pids(&child));
    }

    let mut rss_bytes = 0u64;
    let mut virtual_bytes = 0u64;
    let mut disk_read_bytes = 0u64;
    let mut disk_write_bytes = 0u64;
    for pid in pids {
        if let Some(status) = file_text(&proc_root.join(pid.to_string()).join("status")) {
            rss_bytes = rss_bytes.saturating_add(status_kb(&status, "VmRSS"));
            virtual_bytes = virtual_bytes.saturating_add(status_kb(&status, "VmSize"));
        }
        if let Some(io) = file_text(&proc_root.join(pid.to_string()).join("io")) {
            disk_read_bytes = disk_read_bytes.saturating_add(io_counter(&io, "read_bytes"));
            disk_write_bytes = disk_write_bytes.saturating_add(io_counter(&io, "write_bytes"));
        }
    }

    Some(JobUsage {
        job_id,
        cpu_usage_usec,
        rss_bytes,
        virtual_bytes,
        swap_bytes,
        disk_read_bytes,
        disk_write_bytes,
    })
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs
}

fn cgroup_pids(dir: &Path) -> Vec<u32> {
    let Some(text) = file_text(&dir.join("cgroup.procs")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

fn file_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn usage_usec(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|rest| rest.trim().parse().ok())
}

fn status_kb(text: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        return first_u64(rest).saturating_mul(1024);
    }
    0
}

/// `read_bytes` / `write_bytes` are storage traffic. `rchar` / `wchar` also
/// count pipes and cache, which is not disk I/O.
fn io_counter(text: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        return first_u64(rest);
    }
    0
}

fn first_u64(text: &str) -> u64 {
    text.split_whitespace()
        .next()
        .and_then(|token| token.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn a_held_job_without_a_cgroup_is_omitted() {
        let (cgroup, proc) = fixture();
        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[4]);
        assert!(usage.is_empty());
    }

    #[test]
    fn two_jobs_on_one_node_do_not_share_counters() {
        let (cgroup, proc) = fixture();
        write(
            &cgroup.path().join("job_7_1/cpu.stat"),
            "usage_usec 100\nuser_usec 80\nsystem_usec 20\n",
        );
        write(&cgroup.path().join("job_7_1/memory.swap.current"), "4096\n");
        write(
            &cgroup.path().join("job_7_1/step_batch/cgroup.procs"),
            "10\n",
        );
        write(
            &proc.path().join("10/status"),
            "VmSize:\t 4 kB\nVmRSS:\t 2 kB\n",
        );
        write(
            &proc.path().join("10/io"),
            "rchar: 999\nread_bytes: 100\nwrite_bytes: 50\n",
        );

        write(&cgroup.path().join("job_9_2/cpu.stat"), "usage_usec 7\n");
        write(&cgroup.path().join("job_9_2/memory.swap.current"), "0\n");
        write(&cgroup.path().join("job_9_2/step_0/cgroup.procs"), "11\n");
        write(
            &proc.path().join("11/status"),
            "VmSize:\t 8 kB\nVmRSS:\t 3 kB\n",
        );
        write(
            &proc.path().join("11/io"),
            "read_bytes: 1\nwrite_bytes: 2\n",
        );

        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[9, 7]);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].job_id, 7);
        assert_eq!(usage[0].cpu_usage_usec, 100);
        assert_eq!(usage[0].rss_bytes, 2 * 1024);
        assert_eq!(usage[0].virtual_bytes, 4 * 1024);
        assert_eq!(usage[0].swap_bytes, 4096);
        assert_eq!(usage[0].disk_read_bytes, 100);
        assert_eq!(usage[0].disk_write_bytes, 50);
        assert_eq!(usage[1].job_id, 9);
        assert_eq!(usage[1].cpu_usage_usec, 7);
        assert_eq!(usage[1].disk_write_bytes, 2);
    }

    #[test]
    fn cpu_time_is_the_job_cgroup_total_and_rss_sums_every_step() {
        let (cgroup, proc) = fixture();
        write(&cgroup.path().join("job_3_1/cpu.stat"), "usage_usec 50\n");
        // A step leaf's cpu.stat must not be added again: the parent already
        // includes it.
        write(
            &cgroup.path().join("job_3_1/step_0/cpu.stat"),
            "usage_usec 99999\n",
        );
        write(&cgroup.path().join("job_3_1/memory.swap.current"), "0\n");
        write(&cgroup.path().join("job_3_1/cgroup.procs"), "20\n");
        write(
            &cgroup.path().join("job_3_1/step_0/cgroup.procs"),
            "20\n21\n",
        );
        write(
            &proc.path().join("20/status"),
            "VmRSS:\t 1 kB\nVmSize:\t 10 kB\n",
        );
        write(
            &proc.path().join("20/io"),
            "read_bytes: 5\nwrite_bytes: 0\n",
        );
        write(
            &proc.path().join("21/status"),
            "VmRSS:\t 2 kB\nVmSize:\t 20 kB\n",
        );
        write(
            &proc.path().join("21/io"),
            "read_bytes: 7\nwrite_bytes: 8\n",
        );

        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[3]);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].cpu_usage_usec, 50);
        assert_eq!(usage[0].rss_bytes, 3 * 1024);
        assert_eq!(usage[0].virtual_bytes, 30 * 1024);
        assert_eq!(usage[0].disk_read_bytes, 12);
        assert_eq!(usage[0].disk_write_bytes, 8);
    }

    #[test]
    fn a_later_attempt_wins_and_a_longer_job_id_does_not_match() {
        let (cgroup, proc) = fixture();
        write(&cgroup.path().join("job_1_9/cpu.stat"), "usage_usec 9\n");
        write(&cgroup.path().join("job_1_10/cpu.stat"), "usage_usec 10\n");
        write(&cgroup.path().join("job_17_1/cpu.stat"), "usage_usec 17\n");

        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[1]);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].cpu_usage_usec, 10);
    }

    #[test]
    fn a_pid_that_exits_before_it_is_read_is_skipped() {
        let (cgroup, proc) = fixture();
        write(&cgroup.path().join("job_5_1/cpu.stat"), "usage_usec 1\n");
        write(
            &cgroup.path().join("job_5_1/step_batch/cgroup.procs"),
            "30\n31\n",
        );
        write(
            &proc.path().join("31/status"),
            "VmRSS:\t 4 kB\nVmSize:\t 4 kB\n",
        );
        write(
            &proc.path().join("31/io"),
            "read_bytes: 0\nwrite_bytes: 0\n",
        );

        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[5]);
        assert_eq!(usage[0].rss_bytes, 4 * 1024);
    }

    #[test]
    fn a_job_whose_cpu_stat_is_unreadable_is_omitted() {
        let (cgroup, proc) = fixture();
        write(&cgroup.path().join("job_6_1/cgroup.procs"), "40\n");
        write(&cgroup.path().join("job_8_1/cpu.stat"), "user_usec 3\n");

        let usage = JobUsageSampler::default().sample(cgroup.path(), proc.path(), &[6, 8]);
        assert!(usage.is_empty(), "{usage:?}");
    }

    fn write_disk_job(cgroup: &Path, proc: &Path, attempt: u32, pids: &[(u32, u64, u64)]) {
        let dir = cgroup.join(format!("job_2_{attempt}"));
        write(&dir.join("cpu.stat"), "usage_usec 1\n");
        let procs: String = pids.iter().map(|(pid, _, _)| format!("{pid}\n")).collect();
        write(&dir.join("step_batch/cgroup.procs"), &procs);
        for (pid, read, written) in pids {
            write(
                &proc.join(format!("{pid}/io")),
                &format!("read_bytes: {read}\nwrite_bytes: {written}\n"),
            );
        }
    }

    #[test]
    fn disk_totals_do_not_fall_when_a_process_leaves_the_job() {
        let (cgroup, proc) = fixture();
        let mut sampler = JobUsageSampler::default();

        write_disk_job(cgroup.path(), proc.path(), 1, &[(50, 100, 10), (51, 40, 4)]);
        let first = sampler.sample(cgroup.path(), proc.path(), &[2]);
        assert_eq!(
            (first[0].disk_read_bytes, first[0].disk_write_bytes),
            (140, 14)
        );

        write_disk_job(cgroup.path(), proc.path(), 1, &[(50, 110, 12)]);
        let after_exit = sampler.sample(cgroup.path(), proc.path(), &[2]);
        assert_eq!(
            (
                after_exit[0].disk_read_bytes,
                after_exit[0].disk_write_bytes
            ),
            (140, 14)
        );

        write_disk_job(cgroup.path(), proc.path(), 1, &[(50, 130, 20)]);
        let grown = sampler.sample(cgroup.path(), proc.path(), &[2]);
        assert_eq!(
            (grown[0].disk_read_bytes, grown[0].disk_write_bytes),
            (160, 22)
        );
    }

    #[test]
    fn a_new_attempt_starts_its_own_disk_total() {
        let (cgroup, proc) = fixture();
        let mut sampler = JobUsageSampler::default();

        write_disk_job(cgroup.path(), proc.path(), 1, &[(60, 500, 50)]);
        sampler.sample(cgroup.path(), proc.path(), &[2]);

        write_disk_job(cgroup.path(), proc.path(), 2, &[(61, 7, 3)]);
        let usage = sampler.sample(cgroup.path(), proc.path(), &[2]);
        assert_eq!(
            (usage[0].disk_read_bytes, usage[0].disk_write_bytes),
            (7, 3)
        );
        assert_eq!(sampler.disk.len(), 1);

        sampler.sample(cgroup.path(), proc.path(), &[]);
        assert!(sampler.disk.is_empty());
    }
}
