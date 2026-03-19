//! T53: WAL and snapshot state management.
//!
//! Tests for WAL append/read/truncate, snapshot save/load, recovery.

#[cfg(test)]
mod tests {
    use spur_core::job::{Job, JobSpec};
    use spur_core::node::Node;
    use spur_core::resource::ResourceSet;
    use spur_core::wal::*;
    use spur_state::snapshot::SnapshotStore;
    use spur_state::wal_store::FileWalStore;
    use tempfile::{NamedTempFile, TempDir};

    // ── T53.1: WAL basic operations ──────────────────────────────

    #[test]
    fn t53_1_wal_append_and_read() {
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
    fn t53_2_wal_multiple_entries() {
        let dir = TempDir::new().unwrap();
        let mut store = FileWalStore::new(dir.path()).unwrap();

        for i in 1..=10 {
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

        assert_eq!(store.latest_sequence(), 10);
        let entries = store.read_from(0).unwrap();
        assert_eq!(entries.len(), 10);
    }

    #[test]
    fn t53_3_wal_read_from_offset() {
        let dir = TempDir::new().unwrap();
        let mut store = FileWalStore::new(dir.path()).unwrap();

        for i in 1..=5 {
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

        let entries = store.read_from(3).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
    }

    // ── T53.4: WAL recovery ──────────────────────────────────────

    #[test]
    fn t53_4_wal_recovery_after_reopen() {
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

    // ── T53.5: WAL truncation ────────────────────────────────────

    #[test]
    fn t53_5_wal_truncate() {
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

    // ── T53.6: Snapshot operations ───────────────────────────────

    #[test]
    fn t53_6_snapshot_save_and_load() {
        let tmp = NamedTempFile::new().unwrap();
        let store = SnapshotStore::open(tmp.path()).unwrap();

        let jobs = vec![
            Job::new(
                1,
                JobSpec {
                    name: "job1".into(),
                    user: "alice".into(),
                    ..Default::default()
                },
            ),
            Job::new(
                2,
                JobSpec {
                    name: "job2".into(),
                    user: "bob".into(),
                    ..Default::default()
                },
            ),
        ];
        let nodes = vec![Node::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        )];

        store.save(&jobs, &nodes, 42).unwrap();

        let loaded_jobs = store.load_jobs().unwrap();
        assert_eq!(loaded_jobs.len(), 2);

        let loaded_nodes = store.load_nodes().unwrap();
        assert_eq!(loaded_nodes.len(), 1);
        assert_eq!(loaded_nodes[0].name, "node001");

        assert_eq!(store.wal_sequence().unwrap(), 42);
    }

    #[test]
    fn t53_7_snapshot_empty_load() {
        let tmp = NamedTempFile::new().unwrap();
        let store = SnapshotStore::open(tmp.path()).unwrap();

        let jobs = store.load_jobs().unwrap();
        assert!(jobs.is_empty());

        let nodes = store.load_nodes().unwrap();
        assert!(nodes.is_empty());

        assert_eq!(store.wal_sequence().unwrap(), 0);
    }

    #[test]
    fn t53_8_snapshot_overwrite() {
        let tmp = NamedTempFile::new().unwrap();
        let store = SnapshotStore::open(tmp.path()).unwrap();

        // First snapshot
        let jobs = vec![Job::new(
            1,
            JobSpec {
                name: "old".into(),
                user: "a".into(),
                ..Default::default()
            },
        )];
        store.save(&jobs, &[], 10).unwrap();

        // Second snapshot with more jobs
        let jobs = vec![
            Job::new(
                1,
                JobSpec {
                    name: "new".into(),
                    user: "a".into(),
                    ..Default::default()
                },
            ),
            Job::new(
                2,
                JobSpec {
                    name: "added".into(),
                    user: "b".into(),
                    ..Default::default()
                },
            ),
        ];
        store.save(&jobs, &[], 20).unwrap();

        let loaded = store.load_jobs().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(store.wal_sequence().unwrap(), 20);
    }
}
