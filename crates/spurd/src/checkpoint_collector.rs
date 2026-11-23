// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! spurd's per-node map of checkpoint records collected from live and
//! finished steps. `checkpoint.json` is mtime-driven, so a periodic
//! `refresh()` only re-reads a step whose file actually changed; an
//! acknowledged final record is dropped and marked so the step's session can
//! be pruned.

use std::collections::hash_map;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tokio::sync::Notify;

use spur_core::checkpoint::CheckpointRecord;
use spur_core::step::StepId;

/// `(job_id, run_attempt, step_id)` — identifies one step's checkpoint state.
pub type StepKey = (u32, u32, StepId);

/// Empty marker written into a session directory once its final record has
/// been acknowledged by the controller, so the pruning gate in `stepd.rs`
/// lets the (otherwise finalized) session go.
pub const ACKED_FILE: &str = "checkpoint.acked";

struct Entry {
    session_dir: PathBuf,
    record: CheckpointRecord,
    mtime: Option<SystemTime>,
    dirty: bool,
    /// Set once the step is known finished (a `StepdCompleted` notification or
    /// a stale-session settle) — never inferred from a missing file. Starts
    /// the orphan-retention clock an unacknowledged final record is evicted
    /// against.
    finished_at: Option<SystemTime>,
}

/// Never allowed to fail, block, or delay a step launch or completion: every
/// filesystem error here is logged and swallowed by the caller.
pub struct CheckpointCollector {
    entries: Mutex<HashMap<StepKey, Entry>>,
    wake: Notify,
}

impl Default for CheckpointCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckpointCollector {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<StepKey, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Starts following a step's session directory. A no-op if it is already
    /// tracked — `collect_now`/`refresh` own updating the record from there.
    pub fn track(&self, key: StepKey, session_dir: PathBuf) {
        self.lock().entry(key).or_insert_with(|| Entry {
            session_dir,
            record: CheckpointRecord::default(),
            mtime: None,
            dirty: false,
            finished_at: None,
        });
    }

    /// Marks a tracked step as finished — called once its outcome is known (a
    /// `StepdCompleted` notification, or settling a stale session), never
    /// inferred from a missing file. A no-op if the step is not tracked; the
    /// first call wins the clock if it is somehow called twice.
    pub fn mark_finished(&self, key: StepKey) {
        let mut entries = self.lock();
        if let Some(entry) = entries.get_mut(&key) {
            entry.finished_at.get_or_insert_with(SystemTime::now);
        }
    }

    /// Stats each tracked session's `checkpoint.json` and re-reads only the
    /// ones whose mtime moved since the last read. The map lock is held only
    /// to snapshot what to check and to apply what was read — never across
    /// the filesystem calls themselves.
    pub fn refresh(&self) {
        let snapshot: Vec<(StepKey, PathBuf, Option<SystemTime>)> = {
            let entries = self.lock();
            entries
                .iter()
                .map(|(key, entry)| (*key, entry.session_dir.clone(), entry.mtime))
                .collect()
        };
        for (key, session_dir, previous_mtime) in snapshot {
            let Some(mtime) = record_mtime(&session_dir) else {
                continue;
            };
            if Some(mtime) == previous_mtime {
                continue;
            }
            match crate::checkpoint_listener::load_record(&session_dir) {
                Ok(Some(record)) => self.apply_if_tracked(key, mtime, record),
                // The file vanished between the stat above and this read;
                // the next refresh retries once it reappears.
                Ok(None) => {}
                // The step is still live — keep the last known record rather
                // than fabricating a final one over a transient read/parse
                // failure; the next refresh tries again.
                Err(error) => {
                    tracing::debug!(
                        job_id = key.0,
                        run_attempt = key.1,
                        step_id = key.2,
                        session_dir = %session_dir.display(),
                        %error,
                        "failed to re-read checkpoint.json; keeping the last known record"
                    );
                }
            }
        }
    }

    /// Reads a session's record immediately, for the two events that cannot
    /// wait for the next `refresh()` tick: a `StepdCompleted` notification
    /// and re-adoption at startup. Inserts a tracking entry if one is not
    /// already there. A missing `checkpoint.json` (old spurstepd, or a step
    /// that never ran a listener) leaves no entry and no record.
    pub fn collect_now(&self, key: StepKey, session_dir: &Path) {
        let mtime = record_mtime(session_dir);
        let path = session_dir.join(crate::checkpoint_listener::RECORD_FILE);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CheckpointRecord>(&bytes) {
                Ok(record) => self.upsert(key, session_dir, mtime, record),
                Err(error) => {
                    tracing::warn!(
                        job_id = key.0,
                        run_attempt = key.1,
                        step_id = key.2,
                        path = %path.display(),
                        %error,
                        "unreadable checkpoint.json; recording a final empty record so cleanup proceeds"
                    );
                    self.upsert(
                        key,
                        session_dir,
                        None,
                        CheckpointRecord {
                            is_final: true,
                            ..CheckpointRecord::default()
                        },
                    );
                }
            },
            // No listener ever ran (old spurstepd, or the feature was off at
            // launch): no entry, no record — this is not an error.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    job_id = key.0,
                    run_attempt = key.1,
                    step_id = key.2,
                    path = %path.display(),
                    %error,
                    "failed to read checkpoint.json"
                );
            }
        }
    }

    /// Up to `cap` dirty entries, final records first so a bounded batch
    /// never starves the reports that let a session be pruned.
    pub fn dirty(&self, cap: usize) -> Vec<(StepKey, CheckpointRecord)> {
        let entries = self.lock();
        let mut dirty: Vec<(StepKey, CheckpointRecord)> = entries
            .iter()
            .filter(|(_, entry)| entry.dirty)
            .map(|(key, entry)| (*key, entry.record.clone()))
            .collect();
        dirty.sort_by_key(|(_, record)| !record.is_final);
        dirty.truncate(cap);
        dirty
    }

    /// Clears dirty for exactly the versions sent — an entry whose record has
    /// since moved on stays dirty, so it is reported again. A final entry
    /// that matched is dropped and its session marked acknowledged on disk.
    pub fn acknowledge(&self, sent: &[(StepKey, CheckpointRecord)]) {
        let mut finalized_dirs = Vec::new();
        {
            let mut entries = self.lock();
            for (key, sent_record) in sent {
                let Some(entry) = entries.get_mut(key) else {
                    continue;
                };
                if entry.record != *sent_record {
                    continue;
                }
                entry.dirty = false;
                if sent_record.is_final {
                    if let Some(entry) = entries.remove(key) {
                        finalized_dirs.push(entry.session_dir);
                    }
                }
            }
        }
        for session_dir in finalized_dirs {
            if let Err(error) = crate::stepd::publish_private(&session_dir, ACKED_FILE, b"") {
                tracing::warn!(
                    session_dir = %session_dir.display(),
                    %error,
                    "failed to write the checkpoint.acked marker"
                );
            }
        }
    }

    /// Signaled once per final record observed, so the reporter loop (Task 4)
    /// can ship it without waiting out the full refresh interval.
    pub fn wake(&self) -> &Notify {
        &self.wake
    }

    /// Whether any step is tracked at all — used by callers (and tests) that
    /// need to confirm the feature stayed off rather than inspecting a dirty
    /// snapshot, which only ever shows the entries flagged dirty.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn apply_if_tracked(&self, key: StepKey, mtime: SystemTime, record: CheckpointRecord) {
        let is_final = record.is_final;
        let applied = {
            let mut entries = self.lock();
            match entries.get_mut(&key) {
                Some(entry) if should_apply(entry, Some(mtime), is_final) => {
                    entry.record = record;
                    entry.mtime = Some(mtime);
                    entry.dirty = true;
                    true
                }
                // Already acknowledged and dropped between the snapshot and
                // this read, or this read lost the ordering race against a
                // concurrent `collect_now`/earlier refresh: nothing to update.
                Some(_) | None => false,
            }
        };
        if applied && is_final {
            self.wake.notify_one();
        }
    }

    fn upsert(
        &self,
        key: StepKey,
        session_dir: &Path,
        mtime: Option<SystemTime>,
        record: CheckpointRecord,
    ) {
        let is_final = record.is_final;
        let applied = {
            let mut entries = self.lock();
            match entries.entry(key) {
                hash_map::Entry::Occupied(mut occupied) => {
                    let entry = occupied.get_mut();
                    if should_apply(entry, mtime, is_final) {
                        entry.record = record;
                        entry.mtime = mtime;
                        entry.dirty = true;
                        true
                    } else {
                        false
                    }
                }
                hash_map::Entry::Vacant(vacant) => {
                    vacant.insert(Entry {
                        session_dir: session_dir.to_path_buf(),
                        record,
                        mtime,
                        dirty: true,
                        finished_at: None,
                    });
                    true
                }
            }
        };
        if applied && is_final {
            self.wake.notify_one();
        }
    }

    /// Evicts tracked entries that can never be reported again: a session
    /// whose directory is confirmed gone, a finished step with no record and
    /// nothing dirty to send, or a finished step whose final record has sat
    /// unacknowledged past `ORPHAN_RETENTION` (logged, since the record is
    /// then lost for good). A live step with no file yet is never evicted,
    /// and any filesystem error other than "not found" is treated as
    /// transient — the entry is kept and retried next time. `now` is a
    /// parameter so tests can simulate the retention window without sleeping.
    pub fn evict_stale(&self, now: SystemTime) {
        let snapshot: Vec<(StepKey, PathBuf, Option<SystemTime>, bool, bool)> = {
            let entries = self.lock();
            entries
                .iter()
                .map(|(key, entry)| {
                    (
                        *key,
                        entry.session_dir.clone(),
                        entry.finished_at,
                        entry.record.is_final,
                        entry.dirty,
                    )
                })
                .collect()
        };

        let mut to_evict = Vec::new();
        for (key, session_dir, finished_at, is_final, dirty) in snapshot {
            match fs::symlink_metadata(&session_dir) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    to_evict.push(key);
                    continue;
                }
                Err(error) => {
                    tracing::debug!(
                        job_id = key.0,
                        run_attempt = key.1,
                        step_id = key.2,
                        session_dir = %session_dir.display(),
                        %error,
                        "failed to stat a tracked session directory; keeping the entry"
                    );
                    continue;
                }
                Ok(_) => {}
            }
            let Some(finished_at) = finished_at else {
                // Live step: absence of a file (or of a directory listing we
                // could even take) says nothing about whether it is stuck.
                continue;
            };
            let record_path = session_dir.join(crate::checkpoint_listener::RECORD_FILE);
            match fs::metadata(&record_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !dirty {
                        to_evict.push(key);
                    }
                    continue;
                }
                Err(error) => {
                    tracing::debug!(
                        job_id = key.0,
                        run_attempt = key.1,
                        step_id = key.2,
                        path = %record_path.display(),
                        %error,
                        "failed to stat a finished step's checkpoint.json; keeping the entry"
                    );
                    continue;
                }
                Ok(_) => {}
            }
            if is_final
                && now.duration_since(finished_at).unwrap_or(Duration::ZERO)
                    > crate::step_completion::ORPHAN_RETENTION
            {
                tracing::warn!(
                    job_id = key.0,
                    run_attempt = key.1,
                    step_id = key.2,
                    session_dir = %session_dir.display(),
                    "evicting a finished step's unacknowledged checkpoint record past \
                     the retention window; the record is lost"
                );
                to_evict.push(key);
            }
        }

        if to_evict.is_empty() {
            return;
        }
        let mut entries = self.lock();
        for key in to_evict {
            entries.remove(&key);
        }
    }
}

/// Whether a candidate read may apply over `entry`'s current record: a final
/// record is never downgraded to a non-final one, and only a read that is
/// unambiguously newer overwrites one that might already be newer — an
/// unknown mtime on either side (the entry has none yet, or the caller could
/// not stat its own read) is not provable staleness, so it is allowed through.
fn should_apply(
    entry: &Entry,
    candidate_mtime: Option<SystemTime>,
    candidate_is_final: bool,
) -> bool {
    if entry.record.is_final && !candidate_is_final {
        return false;
    }
    match (entry.mtime, candidate_mtime) {
        (Some(current), Some(new)) => new > current,
        _ => true,
    }
}

fn record_mtime(session_dir: &Path) -> Option<SystemTime> {
    fs::metadata(session_dir.join(crate::checkpoint_listener::RECORD_FILE))
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::time::Duration;

    fn record(completed: u64, is_final: bool) -> CheckpointRecord {
        CheckpointRecord {
            completed,
            is_final,
            ..CheckpointRecord::default()
        }
    }

    fn write_record(session_dir: &Path, record: &CheckpointRecord, mtime: SystemTime) {
        let path = session_dir.join(crate::checkpoint_listener::RECORD_FILE);
        fs::write(&path, serde_json::to_vec(record).expect("encode")).expect("write");
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(mtime)
            .expect("set mtime");
    }

    fn new_session_dir(root: &tempfile::TempDir, name: &str) -> PathBuf {
        let dir = root.path().join(name);
        fs::create_dir(&dir).expect("create session dir");
        dir
    }

    #[test]
    fn refresh_only_re_reads_the_record_once_its_mtime_moves() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let t0 = SystemTime::now();
        write_record(&session_dir, &record(1, false), t0);

        let collector = CheckpointCollector::new();
        let key = (7, 1, 0);
        collector.track(key, session_dir.clone());
        collector.refresh();
        assert_eq!(collector.dirty(10), vec![(key, record(1, false))]);
        collector.acknowledge(&[(key, record(1, false))]);
        assert!(collector.dirty(10).is_empty());

        // Same mtime, new content: refresh must not notice.
        write_record(&session_dir, &record(2, false), t0);
        collector.refresh();
        assert!(
            collector.dirty(10).is_empty(),
            "an unchanged mtime must not trigger a re-read"
        );

        // A later mtime: refresh picks up the new content.
        let t1 = t0 + Duration::from_secs(1);
        write_record(&session_dir, &record(2, false), t1);
        collector.refresh();
        assert_eq!(collector.dirty(10), vec![(key, record(2, false))]);
    }

    #[test]
    fn dirty_respects_the_cap_and_sorts_finals_first() {
        let collector = CheckpointCollector::new();
        {
            let mut entries = collector.lock();
            entries.insert(
                (1, 1, 0),
                Entry {
                    session_dir: PathBuf::from("/a"),
                    record: record(1, false),
                    mtime: None,
                    dirty: true,
                    finished_at: None,
                },
            );
            entries.insert(
                (2, 1, 0),
                Entry {
                    session_dir: PathBuf::from("/b"),
                    record: record(2, true),
                    mtime: None,
                    dirty: true,
                    finished_at: None,
                },
            );
            entries.insert(
                (3, 1, 0),
                Entry {
                    session_dir: PathBuf::from("/c"),
                    record: record(3, false),
                    mtime: None,
                    dirty: true,
                    finished_at: None,
                },
            );
        }

        let capped = collector.dirty(1);
        assert_eq!(capped, vec![((2, 1, 0), record(2, true))]);

        let all = collector.dirty(10);
        assert_eq!(all.len(), 3);
        assert!(all[0].1.is_final, "the final record must sort first");
    }

    #[test]
    fn acknowledge_clears_dirty_drops_finals_and_writes_the_marker() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let collector = CheckpointCollector::new();
        let key = (9, 1, 0);
        let final_record = record(3, true);
        {
            collector.lock().insert(
                key,
                Entry {
                    session_dir: session_dir.clone(),
                    record: final_record.clone(),
                    mtime: None,
                    dirty: true,
                    finished_at: None,
                },
            );
        }

        collector.acknowledge(&[(key, final_record)]);

        assert!(collector.dirty(10).is_empty());
        assert!(
            !collector.lock().contains_key(&key),
            "a final entry must be dropped once acknowledged"
        );
        assert!(session_dir.join(ACKED_FILE).exists());
    }

    #[test]
    fn acknowledging_a_stale_version_leaves_a_newer_record_dirty() {
        let collector = CheckpointCollector::new();
        let key = (4, 1, 0);
        let old = record(1, false);
        {
            collector.lock().insert(
                key,
                Entry {
                    session_dir: PathBuf::from("/irrelevant"),
                    record: old.clone(),
                    mtime: None,
                    dirty: true,
                    finished_at: None,
                },
            );
        }
        let sent = collector.dirty(10);
        assert_eq!(sent, vec![(key, old.clone())]);

        // The record moves on before the ack for the older version arrives.
        let newer = record(2, false);
        {
            let mut entries = collector.lock();
            let entry = entries.get_mut(&key).expect("entry still tracked");
            entry.record = newer.clone();
            entry.dirty = true;
        }

        collector.acknowledge(&sent);

        assert_eq!(collector.dirty(10), vec![(key, newer)]);
    }

    #[test]
    fn collect_now_on_a_corrupt_file_records_a_final_empty_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        fs::write(
            session_dir.join(crate::checkpoint_listener::RECORD_FILE),
            b"not json",
        )
        .expect("write corrupt file");

        let collector = CheckpointCollector::new();
        let key = (5, 1, 0);
        collector.collect_now(key, &session_dir);

        let dirty = collector.dirty(10);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].0, key);
        assert_eq!(
            dirty[0].1,
            CheckpointRecord {
                is_final: true,
                ..CheckpointRecord::default()
            }
        );
    }

    #[test]
    fn collect_now_on_a_missing_file_tracks_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");

        let collector = CheckpointCollector::new();
        collector.collect_now((6, 1, 0), &session_dir);

        assert!(collector.dirty(10).is_empty());
        assert!(collector.lock().is_empty());
    }

    #[tokio::test]
    async fn a_final_record_wakes_the_reporter() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        write_record(&session_dir, &record(1, true), SystemTime::now());

        let collector = CheckpointCollector::new();
        collector.collect_now((8, 1, 0), &session_dir);

        // `notify_one()` stores a permit when nothing is yet waiting, so this
        // resolves immediately rather than needing a timeout to bound it.
        collector.wake().notified().await;
    }

    #[test]
    fn track_does_not_mark_an_entry_dirty() {
        let collector = CheckpointCollector::new();
        collector.track((1, 1, 0), PathBuf::from("/irrelevant"));
        assert!(collector.dirty(10).is_empty());
    }

    #[test]
    fn apply_if_tracked_never_downgrades_a_final_record_to_an_older_non_final_read() {
        let collector = CheckpointCollector::new();
        let key = (10, 1, 0);
        let t2 = SystemTime::now();
        let t1 = t2 - Duration::from_secs(5);
        let final_record = record(9, true);
        collector.lock().insert(
            key,
            Entry {
                session_dir: PathBuf::from("/irrelevant"),
                record: final_record.clone(),
                mtime: Some(t2),
                dirty: false,
                finished_at: None,
            },
        );

        // An older, non-final read racing in behind the final one must not apply.
        collector.apply_if_tracked(key, t1, record(1, false));

        let entries = collector.lock();
        let entry = entries.get(&key).expect("entry still tracked");
        assert_eq!(
            entry.record, final_record,
            "the entry must stay final and unchanged"
        );
        assert_eq!(entry.mtime, Some(t2));
        assert!(
            !entry.dirty,
            "an ignored stale read must not mark the entry dirty"
        );
    }

    #[test]
    fn collect_now_never_downgrades_a_final_record_to_an_older_non_final_read() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let t2 = SystemTime::now();
        write_record(&session_dir, &record(9, true), t2);

        let collector = CheckpointCollector::new();
        let key = (11, 1, 0);
        collector.collect_now(key, &session_dir);
        assert_eq!(collector.dirty(10), vec![(key, record(9, true))]);

        // An older non-final read must not replace the already-final entry.
        let t1 = t2 - Duration::from_secs(5);
        write_record(&session_dir, &record(1, false), t1);
        collector.collect_now(key, &session_dir);

        assert_eq!(
            collector.dirty(10),
            vec![(key, record(9, true))],
            "an older non-final read must not replace the final record"
        );
    }

    #[test]
    fn evict_stale_removes_an_entry_whose_session_dir_is_gone() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let collector = CheckpointCollector::new();
        let key = (20, 1, 0);
        collector.track(key, session_dir.clone());

        fs::remove_dir_all(&session_dir).expect("remove session dir");
        collector.evict_stale(SystemTime::now());

        assert!(
            collector.lock().is_empty(),
            "an entry whose session directory vanished must be evicted"
        );
    }

    #[test]
    fn evict_stale_removes_a_finished_entry_with_no_record_and_nothing_dirty() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let collector = CheckpointCollector::new();
        let key = (21, 1, 0);
        collector.track(key, session_dir.clone());
        collector.mark_finished(key);

        collector.evict_stale(SystemTime::now());

        assert!(
            collector.lock().is_empty(),
            "a finished step with no checkpoint.json and nothing dirty has nothing left to report"
        );
    }

    #[test]
    fn evict_stale_keeps_a_live_entry_with_no_record_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        let collector = CheckpointCollector::new();
        let key = (22, 1, 0);
        collector.track(key, session_dir.clone());

        collector.evict_stale(SystemTime::now());

        assert!(
            collector.lock().contains_key(&key),
            "a live step with no file yet must never be evicted"
        );
    }

    #[test]
    fn evict_stale_removes_an_unacknowledged_final_record_past_orphan_retention_with_dir_present() {
        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        write_record(&session_dir, &record(5, true), SystemTime::now());
        let collector = CheckpointCollector::new();
        let key = (23, 1, 0);
        collector.collect_now(key, &session_dir);
        collector.mark_finished(key);

        let finished_at = collector
            .lock()
            .get(&key)
            .expect("tracked")
            .finished_at
            .expect("marked finished");
        let past_retention =
            finished_at + crate::step_completion::ORPHAN_RETENTION + Duration::from_secs(1);

        collector.evict_stale(past_retention);

        assert!(
            collector.lock().is_empty(),
            "an unacknowledged final record past the retention window must be evicted, \
             even with the session directory still present"
        );
    }

    #[test]
    fn evict_stale_keeps_an_entry_whose_dir_is_unreadable() {
        // Root bypasses the permission check this test relies on.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("tempdir");
        let session_dir = new_session_dir(&root, "session");
        write_record(&session_dir, &record(1, true), SystemTime::now());
        let collector = CheckpointCollector::new();
        let key = (24, 1, 0);
        collector.collect_now(key, &session_dir);
        collector.mark_finished(key);

        fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o000))
            .expect("chmod session dir to 000");

        collector.evict_stale(SystemTime::now());

        fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700))
            .expect("restore session dir permissions");

        assert!(
            collector.lock().contains_key(&key),
            "a transient permission error must keep the entry, not evict it"
        );
    }
}
