// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::agent_server::AgentService;
use crate::spool_fs::{directory_names, open_child, open_directory, JobSpool, SpoolRoot};
use nix::fcntl::OFlag;
use spur_core::job::JobState;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

const QUERY_BATCH_SIZE: usize = 128;

pub(crate) struct ActivityPaths {
    pub(crate) proc_root: PathBuf,
    pub(crate) cgroup_root: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) cgroups_enabled: bool,
    #[cfg(test)]
    pub(crate) worker_barriers: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}

#[derive(Debug, Default)]
pub(crate) struct CleanupSummary {
    pub(crate) deleted: usize,
    pub(crate) retained: usize,
    pub(crate) errors: usize,
}

impl CleanupSummary {
    fn log(&self) {
        info!(
            deleted = self.deleted,
            retained = self.retained,
            errors = self.errors,
            "startup job spool cleanup finished"
        );
    }
}

struct Candidate {
    root: Arc<SpoolRoot>,
    spool: JobSpool,
    path: PathBuf,
}

impl Candidate {
    fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) async fn run(
    service: &AgentService,
    roots: Vec<PathBuf>,
    paths: ActivityPaths,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    if !paths.cgroups_enabled {
        warn!("skipped startup job spool cleanup: cgroups are disabled");
        summary.log();
        return summary;
    }
    let candidates = match tokio::task::spawn_blocking(move || discover(roots)).await {
        Ok(candidates) => candidates,
        Err(error) => {
            warn!(%error, "startup spool discovery failed");
            summary.errors += 1;
            summary.log();
            return summary;
        }
    };
    let ids: Vec<_> = candidates
        .iter()
        .map(|c| c.spool.job_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let paths = Arc::new(paths);
    let mut candidates: HashMap<u32, Vec<Candidate>> =
        candidates
            .into_iter()
            .fold(HashMap::new(), |mut map, candidate| {
                map.entry(candidate.spool.job_id)
                    .or_default()
                    .push(candidate);
                map
            });
    for batch in ids.chunks(QUERY_BATCH_SIZE) {
        let response = match service.reporter.query_spool_jobs(batch.to_vec()).await {
            Ok(response) => response,
            Err(error) => {
                for candidate in candidates.values().flatten() {
                    warn!(job_id = candidate.spool.job_id, path = %candidate.path().display(),
                        "retained job spool: controller query unavailable");
                    summary.retained += 1;
                }
                warn!(%error, "stopped startup spool queries; startup will continue");
                break;
            }
        };
        let mut states = HashMap::<u32, bool>::new();
        for job in response.jobs {
            let terminal =
                JobState::from_proto_i32(job.state).is_some_and(|state| state.is_terminal());
            // Conflicting duplicate records cannot establish terminality.
            states
                .entry(job.job_id)
                .and_modify(|known| *known &= terminal)
                .or_insert(terminal);
        }
        let mut guards = Vec::new();
        let mut eligible = Vec::new();
        for &job_id in batch {
            let job_candidates = candidates.remove(&job_id).unwrap_or_default();
            let controller_allows = response.authoritative
                && states
                    .get(&job_id)
                    .copied()
                    .unwrap_or(response.unrestricted_visibility);
            if !controller_allows {
                for candidate in &job_candidates {
                    info!(job_id, path = %candidate.path().display(), "retained job spool: controller has not confirmed terminality or absence");
                }
                summary.retained += job_candidates.len();
                continue;
            }
            let guard = service.spool_lifecycle_guard(job_id).await;
            if let Some(reason) = service.spool_local_owner(job_id).await {
                for candidate in &job_candidates {
                    info!(job_id, path = %candidate.path().display(), reason, "retained locally owned job spool");
                }
                summary.retained += job_candidates.len();
                continue;
            }
            guards.push(guard);
            eligible.extend(job_candidates);
        }
        if eligible.is_empty() {
            continue;
        }
        let paths = paths.clone();
        // Keep guards in the worker so cancellation cannot release launch exclusion
        // while blocking filesystem operations still run.
        match tokio::task::spawn_blocking(move || {
            let _guards = guards;
            #[cfg(test)]
            if let Some(barriers) = &paths.worker_barriers {
                barriers.0.wait();
                barriers.1.wait();
            }
            clean_inactive(eligible, &paths)
        })
        .await
        {
            Ok(batch_summary) => {
                summary.deleted += batch_summary.deleted;
                summary.retained += batch_summary.retained;
                summary.errors += batch_summary.errors;
            }
            Err(error) => {
                summary.errors += 1;
                warn!(%error, "startup spool cleanup worker failed");
            }
        }
    }
    summary.log();
    summary
}

fn discover(paths: Vec<PathBuf>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut roots = HashSet::new();
    for path in paths {
        let root = match SpoolRoot::open(path.clone()) {
            Ok(root) => Arc::new(root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(root = %path.display(), %error, "skipped unsafe or unreadable spool root");
                continue;
            }
        };
        let spools = match root.identity().and_then(|identity| {
            if !roots.insert(identity) {
                return Ok(Vec::new());
            }
            root.candidates()
        }) {
            Ok(spools) => spools,
            Err(error) => {
                warn!(root = %path.display(), %error, "skipped unreadable spool inventory");
                continue;
            }
        };
        candidates.extend(spools.into_iter().map(|spool| Candidate {
            path: root.path.join(format!("job{}", spool.job_id)),
            root: root.clone(),
            spool,
        }));
    }
    candidates
}

fn clean_inactive(candidates: Vec<Candidate>, paths: &ActivityPaths) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    let inventory = activity_inventory(&candidates, paths);
    for candidate in candidates {
        let job_id = candidate.spool.job_id;
        let path = candidate.path();
        match &inventory {
            Err(error) => {
                warn!(job_id, path = %path.display(), %error, "retained job spool: activity inventory uncertain");
                summary.retained += 1;
                continue;
            }
            Ok(protected) if protected.contains(&job_id) => {
                info!(job_id, path = %path.display(), "retained job spool: runtime session, cgroup or process still holds it");
                summary.retained += 1;
                continue;
            }
            Ok(_) => {}
        }
        match candidate.root.delete(&candidate.spool) {
            Ok(()) => {
                summary.deleted += 1;
                info!(job_id, path = %path.display(), "deleted orphan job spool");
            }
            Err(error) => {
                summary.errors += 1;
                warn!(job_id, path = %path.display(), %error, "failed to delete orphan job spool; will reconsider on next startup");
            }
        }
    }
    summary
}

fn activity_inventory(candidates: &[Candidate], paths: &ActivityPaths) -> io::Result<HashSet<u32>> {
    let mut protected = retained_sessions(&paths.runtime_root)?;
    protected.extend(populated_jobs(&paths.cgroup_root)?);
    inspect_processes(&paths.proc_root, candidates, &mut protected)?;
    Ok(protected)
}

fn optional_directory(path: &Path) -> io::Result<Option<File>> {
    match open_directory(path) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn retained_sessions(root: &Path) -> io::Result<HashSet<u32>> {
    let mut protected = HashSet::new();
    let Some(directory) = optional_directory(root)? else {
        return Ok(protected);
    };
    for name in directory_names(&directory)? {
        if name == "cleanup" {
            continue;
        }
        let entry = open_child(&directory, &name, OFlag::O_PATH)?;
        let metadata = entry.metadata()?;
        if !metadata.is_dir() && !metadata.is_symlink() {
            continue;
        }
        let id = name
            .to_str()
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0);
        let Some(id) = id else {
            return Err(io::Error::other("unidentified retained runtime session"));
        };
        protected.insert(id);
    }
    Ok(protected)
}

fn populated_jobs(root: &Path) -> io::Result<HashSet<u32>> {
    let parent = root
        .parent()
        .ok_or_else(|| io::Error::other("cgroup root has no parent"))?;
    let parent = open_directory(parent)?;
    open_child(&parent, OsStr::new("cgroup.controllers"), OFlag::O_RDONLY)?;
    let mut protected = HashSet::new();
    let Some(directory) = optional_directory(root)? else {
        return Ok(protected);
    };
    for name in directory_names(&directory)? {
        let Some(name_str) = name.to_str() else {
            return Err(io::Error::other("unidentified cgroup"));
        };
        let Some(rest) = name_str.strip_prefix("job_") else {
            continue;
        };
        let Some(id) = rest
            .split('_')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0)
        else {
            return Err(io::Error::other("unidentified job cgroup"));
        };
        let job = open_child(&directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)?;
        if cgroup_populated(&job)? {
            protected.insert(id);
        }
    }
    Ok(protected)
}

fn cgroup_populated(directory: &File) -> io::Result<bool> {
    let mut events = String::new();
    open_child(directory, OsStr::new("cgroup.events"), OFlag::O_RDONLY)?
        .read_to_string(&mut events)?;
    let values: Vec<_> = events
        .lines()
        .filter_map(|line| line.strip_prefix("populated "))
        .collect();
    match values.as_slice() {
        ["1"] => Ok(true),
        ["0"] => Ok(false),
        _ => Err(io::Error::other("invalid cgroup population metadata")),
    }
}

fn inspect_processes(
    root: &Path,
    candidates: &[Candidate],
    protected: &mut HashSet<u32>,
) -> io::Result<()> {
    let mut identities = HashMap::<(u64, u64), HashSet<u32>>::new();
    for candidate in candidates {
        for identity in candidate.root.held_identities(&candidate.spool)? {
            identities
                .entry(identity)
                .or_default()
                .insert(candidate.spool.job_id);
        }
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        let path = entry.path();
        let before = match fs::read(path.join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let result = inspect_process(&path, candidates, &identities, protected);
        match fs::read(path.join("stat")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(after) if process_identity(&before)? != process_identity(&after)? => {
                return Err(io::Error::other(
                    "process identity changed during spool inventory",
                ));
            }
            Ok(_) => result?,
        }
    }
    Ok(())
}

fn process_identity(stat: &[u8]) -> io::Result<&[u8]> {
    let end = stat
        .iter()
        .rposition(|b| *b == b')')
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    stat[end + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .nth(19)
        .ok_or_else(|| io::Error::other("missing process start time"))
}

fn inspect_process(
    path: &Path,
    candidates: &[Candidate],
    identities: &HashMap<(u64, u64), HashSet<u32>>,
    protected: &mut HashSet<u32>,
) -> io::Result<()> {
    let cmdline = fs::read(path.join("cmdline"))?;
    let comm = fs::read(path.join("comm"))?;
    let args: Vec<_> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    let executable = fs::read_link(path.join("exe"));
    let supervisor = comm.strip_suffix(b"\n").unwrap_or(&comm) == b"spurstepd"
        || args.first().is_some_and(|arg| {
            Path::new(OsStr::from_bytes(arg)).file_name() == Some(OsStr::new("spurstepd"))
        })
        || executable.as_ref().is_ok_and(|exe| {
            let bytes = exe.as_os_str().as_bytes();
            Path::new(OsStr::from_bytes(
                bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
            ))
            .file_name()
                == Some(OsStr::new("spurstepd"))
        });
    if supervisor {
        let id = args
            .get(2)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0);
        let Some(id) = id else {
            return Err(io::Error::other("unidentified live supervisor"));
        };
        protected.insert(id);
    }
    if let Err(error) = executable {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    for variable in fs::read(path.join("environ"))?.split(|b| *b == 0) {
        let Some(separator) = variable.iter().position(|b| *b == b'=') else {
            continue;
        };
        let (key, value) = (&variable[..separator], &variable[separator + 1..]);
        if !matches!(
            key,
            b"SPUR_JOB_ID" | b"SPUR_JOBID" | b"SLURM_JOB_ID" | b"SLURM_JOBID"
        ) {
            continue;
        }
        let id = std::str::from_utf8(value)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0)
            .ok_or_else(|| io::Error::other("invalid process job marker"))?;
        protected.insert(id);
    }
    match fs::read_link(path.join("cwd")) {
        Ok(target) => protect_holder(
            &path.join("cwd"),
            &target,
            candidates,
            identities,
            protected,
        )?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && args.is_empty() => {}
        Err(error) => return Err(error),
    }
    for fd in fs::read_dir(path.join("fd"))? {
        let fd = fd?.path();
        match fs::read_link(&fd) {
            Ok(target) => protect_holder(&fd, &target, candidates, identities, protected)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn protect_holder(
    holder: &Path,
    target: &Path,
    candidates: &[Candidate],
    identities: &HashMap<(u64, u64), HashSet<u32>>,
    protected: &mut HashSet<u32>,
) -> io::Result<()> {
    protect_target(target, candidates, protected);
    match fs::metadata(holder) {
        Ok(metadata) => {
            if let Some(jobs) = identities.get(&(metadata.dev(), metadata.ino())) {
                protected.extend(jobs);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn protect_target(target: &Path, candidates: &[Candidate], protected: &mut HashSet<u32>) {
    let bytes = target.as_os_str().as_bytes();
    let target = Path::new(OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ));
    for candidate in candidates {
        if target.starts_with(candidate.path()) {
            protected.insert(candidate.spool.job_id);
        }
    }
}
