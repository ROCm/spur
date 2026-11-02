// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
pub(crate) const DRAIN_REASON: &str = "job cgroup cleanup incomplete";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingCleanup {
    pub job_id: u32,
    pub run_attempt: u32,
    pub step_id: u32,
    pub cgroup: PathBuf,
    pub deadline_unix_ms: u64,
    #[serde(default)]
    pub quarantine_acknowledged: bool,
}

impl PendingCleanup {
    pub fn new(
        job_id: u32,
        run_attempt: u32,
        step_id: u32,
        cgroup: PathBuf,
        timeout_secs: u32,
    ) -> io::Result<Self> {
        Ok(Self {
            job_id,
            run_attempt,
            step_id,
            cgroup,
            quarantine_acknowledged: false,
            deadline_unix_ms: unix_ms()?.saturating_add(u64::from(timeout_secs) * 1000),
        })
    }
}

fn unix_ms() -> io::Result<u64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?;
    u64::try_from(elapsed.as_millis()).map_err(io::Error::other)
}

#[derive(Clone)]
pub(crate) struct CleanupStore {
    directory: PathBuf,
}

impl CleanupStore {
    pub fn new(runtime: &Path) -> Self {
        Self {
            directory: runtime.join("cleanup"),
        }
    }

    fn path(&self, record: &PendingCleanup) -> PathBuf {
        self.directory.join(format!(
            "{}.{}.{}.json",
            record.job_id, record.run_attempt, record.step_id
        ))
    }

    pub fn track(&self, record: &PendingCleanup) -> io::Result<()> {
        validate_cgroup(&record.cgroup)?;
        fs::create_dir_all(&self.directory)?;
        if let Some(parent) = self.directory.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        let path = self.path(record);
        match fs::read(&path) {
            Ok(bytes) => {
                return check_identity(&bytes, record);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.write_record(record, true)
    }

    fn write_record(&self, record: &PendingCleanup, create_new: bool) -> io::Result<()> {
        let path = self.path(record);
        let temporary = self
            .directory
            .join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec(record)?)?;
            file.sync_all()?;
            if create_new {
                // Concurrent supervisors and replay must keep the first persisted deadline.
                match fs::hard_link(&temporary, &path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        check_identity(&fs::read(&path)?, record)?;
                    }
                    Err(error) => return Err(error),
                }
                fs::remove_file(&temporary)?;
            } else {
                fs::rename(&temporary, &path)?;
            }
            fs::File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn quarantine_acknowledged(&self, record: &PendingCleanup) -> io::Result<bool> {
        match fs::read(self.path(record)) {
            Ok(bytes) => {
                Ok(serde_json::from_slice::<PendingCleanup>(&bytes)?.quarantine_acknowledged)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn acknowledge_quarantine(&self, record: &PendingCleanup) -> io::Result<()> {
        let bytes = match fs::read(self.path(record)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        check_identity(&bytes, record)?;
        let mut stored: PendingCleanup = serde_json::from_slice(&bytes)?;
        if !stored.quarantine_acknowledged {
            stored.quarantine_acknowledged = true;
            self.write_record(&stored, false)?;
        }
        Ok(())
    }

    pub fn finish(&self, record: &PendingCleanup) -> io::Result<()> {
        match fs::remove_file(self.path(record)) {
            Ok(()) => fs::File::open(&self.directory)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn entries(&self) -> io::Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(".tmp-") {
                paths.push(entry.path());
            }
        }
        Ok(paths)
    }

    pub fn is_pending(&self) -> io::Result<bool> {
        Ok(!self.entries()?.is_empty())
    }

    pub fn is_pending_for(&self, job_id: u32, run_attempt: u32) -> io::Result<bool> {
        Ok(self
            .records()?
            .iter()
            .any(|record| record.job_id == job_id && record.run_attempt == run_attempt))
    }

    pub fn expired(&self, record: &PendingCleanup) -> io::Result<bool> {
        let bytes = match fs::read(self.path(record)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let stored: PendingCleanup = serde_json::from_slice(&bytes)?;
        Ok(unix_ms()? >= stored.deadline_unix_ms)
    }

    pub fn remaining(&self, record: &PendingCleanup) -> io::Result<Duration> {
        let bytes = match fs::read(self.path(record)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Duration::ZERO),
            Err(error) => return Err(error),
        };
        let stored: PendingCleanup = serde_json::from_slice(&bytes)?;
        Ok(Duration::from_millis(
            stored.deadline_unix_ms.saturating_sub(unix_ms()?),
        ))
    }

    pub fn prepare(&self, record: &PendingCleanup) -> io::Result<bool> {
        match reap_once(&record.cgroup) {
            Ok(true) => {
                self.finish(record)?;
                Ok(true)
            }
            remaining => {
                self.track(record)?;
                if let Err(error) = remaining {
                    tracing::warn!(%error, "cgroup cleanup remains unresolved");
                }
                Ok(false)
            }
        }
    }

    fn reap(&self, record: &PendingCleanup) -> io::Result<bool> {
        if !reap_once(&record.cgroup)? {
            return Ok(false);
        }
        self.finish(record)?;
        Ok(true)
    }

    pub fn records(&self) -> io::Result<Vec<PendingCleanup>> {
        self.entries()?
            .into_iter()
            .map(|path| serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other))
            .collect()
    }

    pub fn retry(&self) -> io::Result<()> {
        for path in self.entries()? {
            let result = (|| {
                let record: PendingCleanup = serde_json::from_slice(&fs::read(path)?)?;
                self.reap(&record)
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "cgroup cleanup remains pending");
            }
        }
        Ok(())
    }
}

fn check_identity(bytes: &[u8], record: &PendingCleanup) -> io::Result<()> {
    let existing: PendingCleanup = serde_json::from_slice(bytes)?;
    if existing.job_id == record.job_id
        && existing.run_attempt == record.run_attempt
        && existing.step_id == record.step_id
        && existing.cgroup == record.cgroup
    {
        return Ok(());
    }
    Err(io::Error::other("cleanup identity changed"))
}

pub(crate) fn owned_cgroup(path: &Path, step_id: u32) -> &Path {
    if spur_core::step::owns_job_lifetime(step_id)
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("step_"))
    {
        if let Some(parent) = path.parent() {
            return parent;
        }
    }
    path
}

fn validate_cgroup(path: &Path) -> io::Result<()> {
    if path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with("job_") || name.starts_with("step_")
    }) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "not an owned cgroup",
    ))
}

fn kill_tree(path: &Path) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.kill"))
    {
        Ok(mut file) => return file.write_all(b"1"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            kill_tree(&entry.path())?;
        }
    }
    crate::executor::cgroup_signal(path, nix::sys::signal::Signal::SIGKILL)?;
    Ok(())
}

pub(crate) fn reap_once(path: &Path) -> io::Result<bool> {
    validate_cgroup(path)?;
    reap_tree(path)
}

fn reap_tree(path: &Path) -> io::Result<bool> {
    match fs::remove_dir(path) {
        Ok(()) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(_) => {}
    }
    kill_tree(path)?;
    let events = fs::read_to_string(path.join("cgroup.events"))?;
    let populated = events.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("populated"))
            .then(|| fields.next())
            .flatten()
    });
    match populated {
        Some("1") => return Ok(false),
        Some("0") => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing cgroup population",
            ))
        }
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !reap_tree(&entry.path())? {
            return Ok(false);
        }
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::EBUSY) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) async fn wait_until_reaped(path: &Path, budget: Duration) -> io::Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if reap_once(path)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + POLL_INTERVAL).min(deadline)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn populated_cgroup(root: &Path) -> PathBuf {
        let path = root.join("job_7_1");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("cgroup.kill"), "").unwrap();
        fs::write(path.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        path
    }

    #[tokio::test(start_paused = true)]
    async fn completion_stays_pending_until_the_cgroup_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated_cgroup(dir.path());
        let waiting = wait_until_reaped(&path, Duration::from_secs(20));
        tokio::pin!(waiting);
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
        fs::remove_dir_all(&path).unwrap();
        tokio::time::advance(POLL_INTERVAL).await;
        assert!(waiting.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_does_not_claim_that_a_populated_cgroup_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated_cgroup(dir.path());
        assert!(!wait_until_reaped(&path, Duration::ZERO).await.unwrap());
    }

    #[test]
    fn unreadable_population_retains_the_cleanup_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated_cgroup(dir.path());
        fs::remove_file(path.join("cgroup.events")).unwrap();
        let store = CleanupStore::new(&dir.path().join("runtime"));
        let record = PendingCleanup::new(7, 1, spur_core::step::STEP_BATCH, path, 20).unwrap();
        assert!(reap_once(&record.cgroup).is_err());
        assert!(!store.prepare(&record).unwrap());
        assert!(store.is_pending().unwrap());
        store.retry().unwrap();
        assert!(store.is_pending().unwrap());
    }

    #[test]
    fn restart_retries_cleanup_and_preserves_the_original_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated_cgroup(dir.path());
        let runtime = dir.path().join("runtime");
        let record = PendingCleanup {
            job_id: 7,
            run_attempt: 1,
            step_id: spur_core::step::STEP_BATCH,
            cgroup: path.clone(),
            quarantine_acknowledged: false,
            deadline_unix_ms: 0,
        };
        let store = CleanupStore::new(&runtime);
        assert!(!store.prepare(&record).unwrap());
        let restarted = CleanupStore::new(&runtime);
        let later = PendingCleanup {
            deadline_unix_ms: u64::MAX,
            ..record.clone()
        };
        restarted.track(&later).unwrap();
        assert!(restarted.expired(&later).unwrap());
        assert_eq!(restarted.remaining(&later).unwrap(), Duration::ZERO);
        assert!(restarted.is_pending().unwrap());
        fs::remove_dir_all(path).unwrap();
        restarted.retry().unwrap();
        assert!(!restarted.is_pending().unwrap());
    }

    #[test]
    fn a_corrupt_record_keeps_launches_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let store = CleanupStore::new(dir.path());
        fs::create_dir_all(&store.directory).unwrap();
        fs::write(store.directory.join("7.1.0.json"), "broken").unwrap();
        store.retry().unwrap();
        assert!(store.is_pending().unwrap());
    }

    #[test]
    fn quarantine_acknowledgment_preserves_old_records_and_survives_restart() {
        let old = r#"{"job_id":7,"run_attempt":1,"step_id":4294967291,"cgroup":"/fixture/job_7_1","deadline_unix_ms":0}"#;
        let record: PendingCleanup = serde_json::from_str(old).unwrap();
        assert!(!record.quarantine_acknowledged);
        let dir = tempfile::tempdir().unwrap();
        let store = CleanupStore::new(dir.path());
        store.track(&record).unwrap();
        store.acknowledge_quarantine(&record).unwrap();
        let restarted = CleanupStore::new(dir.path());
        assert!(restarted.quarantine_acknowledged(&record).unwrap());
        assert_eq!(restarted.records().unwrap()[0].deadline_unix_ms, 0);
        restarted.track(&record).unwrap();
        assert!(restarted.quarantine_acknowledged(&record).unwrap());
    }

    #[test]
    fn owning_step_accepts_both_job_and_step_paths() {
        let job = Path::new("/fixture/spur/job_7_1");
        let batch = job.join("step_batch");
        let numbered = job.join("step_0");
        assert_eq!(owned_cgroup(job, spur_core::step::STEP_BATCH), job);
        assert_eq!(owned_cgroup(&batch, spur_core::step::STEP_BATCH), job);
        assert_eq!(owned_cgroup(&numbered, 0), numbered);
        assert!(reap_once(Path::new("/fixture/spur")).is_err());
    }
}
