// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A launch's claim on its node from prolog entry until a supervisor owns the
//! job or the epilog undoing the prolog has run, kept on disk so a restarted
//! agent still owes that epilog and never hands the resources out meanwhile.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spur_core::hooks::HookContext;

use crate::teardown_lease::HookProcess;

const RECORD_DIR: &str = "launch-holds";
const RECORD_SUFFIX: &str = ".json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub(crate) enum LaunchPhase {
    /// The prolog may have run; `hook` is its group once spawned.
    Prolog { hook: Option<HookProcess> },
    /// The epilog undoing the prolog may be running; `hook` is its group once spawned.
    Epilog { hook: Option<HookProcess> },
    /// The epilog failed and the controller has not yet acknowledged the drain.
    QuarantinePending,
}

impl LaunchPhase {
    pub(crate) fn hook(&self) -> Option<&HookProcess> {
        match self {
            Self::Prolog { hook } | Self::Epilog { hook } => hook.as_ref(),
            Self::QuarantinePending => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchRecord {
    pub job_id: u32,
    pub run_attempt: u32,
    pub cpu_ids: Vec<u32>,
    pub memory_mb: u64,
    pub gpu_ids: Vec<u64>,
    pub prolog_ctx: HookContext,
    #[serde(flatten)]
    pub phase: LaunchPhase,
}

#[derive(Clone, Debug)]
pub(crate) struct LaunchRecords {
    dir: PathBuf,
}

impl LaunchRecords {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join(RECORD_DIR),
        }
    }

    /// Replaces the attempt's earlier record in one step.
    pub(crate) fn write(&self, record: &LaunchRecord) -> io::Result<()> {
        crate::stepd::create_private_dir_all(&self.dir)?;
        crate::stepd::publish_private(
            &self.dir,
            &file_name(record.job_id, record.run_attempt),
            &serde_json::to_vec(record)?,
        )
    }

    pub(crate) fn remove(&self, job_id: u32, run_attempt: u32) -> io::Result<()> {
        match fs::remove_file(self.dir.join(file_name(job_id, run_attempt))) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Every readable record; one that cannot be parsed is logged and skipped.
    pub(crate) fn load(&self) -> io::Result<Vec<LaunchRecord>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(RECORD_SUFFIX))
            {
                continue;
            }
            match fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
            {
                Ok(record) => records.push(record),
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "skipping an unreadable launch hold record"
                ),
            }
        }
        Ok(records)
    }
}

fn file_name(job_id: u32, run_attempt: u32) -> String {
    format!("{job_id}.{run_attempt}{RECORD_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(job_id: u32, phase: LaunchPhase) -> LaunchRecord {
        LaunchRecord {
            job_id,
            run_attempt: 2,
            cpu_ids: vec![3],
            memory_mb: 64,
            gpu_ids: vec![7],
            prolog_ctx: HookContext {
                job_id,
                work_dir: "/tmp".into(),
                uid: 1000,
                gid: 1000,
                partition: "gpu".into(),
                nodelist: "node1".into(),
                script_context: "prolog_slurmd".into(),
                gpu_devices: vec![7],
                cpus: 1,
                memory_mb: 64,
            },
            phase,
        }
    }

    #[test]
    fn a_written_record_reads_back_and_a_new_phase_replaces_it() {
        let state = tempfile::tempdir().unwrap();
        let records = LaunchRecords::new(state.path());
        assert_eq!(records.load().unwrap(), Vec::new());

        records
            .write(&record(5, LaunchPhase::Prolog { hook: None }))
            .unwrap();
        let epilog = record(
            5,
            LaunchPhase::Epilog {
                hook: Some(HookProcess {
                    pid: 4242,
                    group_token: "token".into(),
                }),
            },
        );
        records.write(&epilog).unwrap();
        assert_eq!(records.load().unwrap(), vec![epilog]);
    }

    #[test]
    fn removing_a_record_tolerates_one_already_gone() {
        let state = tempfile::tempdir().unwrap();
        let records = LaunchRecords::new(state.path());
        records.remove(5, 2).unwrap();
        records
            .write(&record(5, LaunchPhase::QuarantinePending))
            .unwrap();
        records.remove(5, 2).unwrap();
        assert_eq!(records.load().unwrap(), Vec::new());
    }

    #[test]
    fn an_unparseable_record_is_skipped_without_hiding_the_rest() {
        let state = tempfile::tempdir().unwrap();
        let records = LaunchRecords::new(state.path());
        let pending = record(6, LaunchPhase::QuarantinePending);
        records.write(&pending).unwrap();
        fs::write(state.path().join(RECORD_DIR).join("7.0.json"), b"{").unwrap();
        assert_eq!(records.load().unwrap(), vec![pending]);
    }
}
