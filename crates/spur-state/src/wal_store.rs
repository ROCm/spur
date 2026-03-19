use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use spur_core::wal::{WalEntry, WalStore, WalStoreError};
use tracing::{debug, warn};

/// File-based WAL store. Each entry is a JSON line.
pub struct FileWalStore {
    path: PathBuf,
    latest_seq: u64,
}

impl FileWalStore {
    pub fn new(dir: &Path) -> Result<Self, WalStoreError> {
        fs::create_dir_all(dir)?;
        let path = dir.join("wal.jsonl");

        // Recover latest sequence from existing WAL
        let latest_seq = if path.exists() {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut max_seq = 0u64;
            for line in reader.lines() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<WalEntry>(&line) {
                    Ok(entry) => max_seq = max_seq.max(entry.sequence),
                    Err(e) => {
                        warn!(error = %e, "skipping corrupted WAL entry");
                    }
                }
            }
            max_seq
        } else {
            0
        };

        debug!(path = %path.display(), latest_seq, "WAL store opened");

        Ok(Self { path, latest_seq })
    }
}

impl WalStore for FileWalStore {
    fn append(&mut self, entry: &WalEntry) -> Result<(), WalStoreError> {
        let json = serde_json::to_string(entry)
            .map_err(|e| WalStoreError::Serialization(e.to_string()))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        writeln!(file, "{}", json)?;
        file.flush()?;

        self.latest_seq = self.latest_seq.max(entry.sequence);
        Ok(())
    }

    fn read_from(&self, sequence: u64) -> Result<Vec<WalEntry>, WalStoreError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<WalEntry>(&line) {
                Ok(entry) if entry.sequence >= sequence => {
                    entries.push(entry);
                }
                Ok(_) => {} // Before requested sequence
                Err(e) => {
                    warn!(error = %e, "skipping corrupted WAL entry during read");
                }
            }
        }

        entries.sort_by_key(|e| e.sequence);
        Ok(entries)
    }

    fn latest_sequence(&self) -> u64 {
        self.latest_seq
    }

    fn truncate_before(&mut self, sequence: u64) -> Result<(), WalStoreError> {
        if !self.path.exists() {
            return Ok(());
        }

        // Read all entries, keep only those >= sequence, rewrite
        let entries = self.read_from(sequence)?;
        let tmp_path = self.path.with_extension("jsonl.tmp");

        {
            let mut file = File::create(&tmp_path)?;
            for entry in &entries {
                let json = serde_json::to_string(entry)
                    .map_err(|e| WalStoreError::Serialization(e.to_string()))?;
                writeln!(file, "{}", json)?;
            }
            file.flush()?;
        }

        fs::rename(&tmp_path, &self.path)?;
        debug!(sequence, remaining = entries.len(), "WAL truncated");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;
    use spur_core::wal::WalOperation;
    use tempfile::TempDir;

    #[test]
    fn test_wal_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut store = FileWalStore::new(dir.path()).unwrap();

        let entry = WalEntry::new(
            1,
            WalOperation::JobSubmit {
                job_id: 42,
                spec: JobSpec {
                    name: "test".into(),
                    user: "alice".into(),
                    partition: Some("gpu".into()),
                    num_nodes: 2,
                    num_tasks: 16,
                    cpus_per_task: 1,
                    ..Default::default()
                },
            },
        );

        store.append(&entry).unwrap();
        assert_eq!(store.latest_sequence(), 1);

        let entries = store.read_from(0).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sequence, 1);
    }

    #[test]
    fn test_wal_recovery() {
        let dir = TempDir::new().unwrap();

        // Write entries
        {
            let mut store = FileWalStore::new(dir.path()).unwrap();
            for i in 1..=5 {
                store
                    .append(&WalEntry::new(
                        i,
                        WalOperation::JobSubmit {
                            job_id: i as u32,
                            spec: JobSpec {
                                name: format!("job{}", i),
                                user: "alice".into(),
                                ..Default::default()
                            },
                        },
                    ))
                    .unwrap();
            }
        }

        // Reopen and verify
        let store = FileWalStore::new(dir.path()).unwrap();
        assert_eq!(store.latest_sequence(), 5);
        let entries = store.read_from(3).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_wal_truncate() {
        let dir = TempDir::new().unwrap();
        let mut store = FileWalStore::new(dir.path()).unwrap();

        for i in 1..=10 {
            store
                .append(&WalEntry::new(
                    i,
                    WalOperation::NodeHeartbeat {
                        name: "node001".into(),
                        cpu_load: 50,
                        free_memory_mb: 100_000,
                    },
                ))
                .unwrap();
        }

        store.truncate_before(6).unwrap();
        let entries = store.read_from(0).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].sequence, 6);
    }
}
