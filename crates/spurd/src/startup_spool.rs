// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::agent_server::AgentService;
use crate::spool_fs::{
    budget_exhausted, directory_names, inode_identity, kept_reason, open_child, open_directory,
    JobSpool, SpoolRoot,
};
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
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const QUERY_BATCH_SIZE: usize = 128;
pub(crate) const STARTUP_CLEANUP_BUDGET: Duration = Duration::from_secs(60);

pub(crate) struct ActivityPaths {
    pub(crate) proc_root: PathBuf,
    pub(crate) cgroup_root: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) cgroups_enabled: bool,
    pub(crate) supervisor_uid: u32,
    pub(crate) budget: Duration,
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

pub(crate) async fn run(
    service: &AgentService,
    roots: Vec<PathBuf>,
    paths: ActivityPaths,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    let deadline = Instant::now() + paths.budget;
    if !paths.cgroups_enabled {
        warn!("skipped startup job spool cleanup: cgroups are disabled");
        summary.log();
        return summary;
    }
    let discovery = tokio::task::spawn_blocking(move || discover(roots));
    let candidates = match tokio::time::timeout_at(deadline.into(), discovery).await {
        Ok(Ok(candidates)) => candidates,
        Ok(Err(error)) => {
            warn!(%error, "startup spool discovery failed");
            summary.errors += 1;
            summary.log();
            return summary;
        }
        Err(_) => {
            warn!("startup spool discovery exceeded its time budget; startup will continue");
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
    let mut reported_hidden_jobs = false;
    for batch in ids.chunks(QUERY_BATCH_SIZE) {
        if Instant::now() >= deadline {
            retain_remaining(&candidates, "cleanup time budget exhausted", &mut summary);
            warn!("stopped startup spool cleanup at its time budget; startup will continue");
            break;
        }
        let query = service.reporter.query_spool_jobs(batch.to_vec());
        let response = match tokio::time::timeout_at(deadline.into(), query).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                retain_remaining(&candidates, "controller query unavailable", &mut summary);
                warn!(%error, "stopped startup spool queries; startup will continue");
                break;
            }
            Err(_) => {
                retain_remaining(&candidates, "cleanup time budget exhausted", &mut summary);
                warn!("stopped startup spool cleanup at its time budget; startup will continue");
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
        if response.authoritative
            && !response.unrestricted_visibility
            && !reported_hidden_jobs
            && batch.iter().any(|id| !states.contains_key(id))
        {
            reported_hidden_jobs = true;
            info!("retaining job spools the controller does not show: private job data hides other users' jobs from this node");
        }
        let mut guards = Vec::new();
        let mut eligible = Vec::new();
        for &job_id in batch {
            let job_candidates = candidates.remove(&job_id).unwrap_or_default();
            // Ids at or above the controller's issue high-water mark are not confirmed absent.
            let absent_confirmed =
                response.unrestricted_visibility && job_id < response.next_job_id;
            let controller_allows =
                response.authoritative && states.get(&job_id).copied().unwrap_or(absent_confirmed);
            if !controller_allows {
                for candidate in &job_candidates {
                    info!(job_id, path = %candidate.path.display(), "retained job spool: controller has not confirmed terminality or absence");
                }
                summary.retained += job_candidates.len();
                continue;
            }
            let guard = service.spool_lifecycle_guard(job_id).await;
            if let Some(reason) = service.spool_local_owner(job_id).await {
                for candidate in &job_candidates {
                    info!(job_id, path = %candidate.path.display(), reason, "retained locally owned job spool");
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
        let in_flight = eligible.len();
        let paths = paths.clone();
        // Keep guards in the worker so cancellation cannot release launch exclusion
        // while blocking filesystem operations still run.
        let worker = tokio::task::spawn_blocking(move || {
            let _guards = guards;
            #[cfg(test)]
            if let Some(barriers) = &paths.worker_barriers {
                barriers.0.wait();
                barriers.1.wait();
            }
            clean_inactive(eligible, &paths, deadline)
        });
        // A worker stuck in the kernel keeps its guards; its own deadline checks stop late deletions.
        let Ok(worker) = tokio::time::timeout_at(deadline.into(), worker).await else {
            summary.retained += in_flight;
            retain_remaining(&candidates, "cleanup time budget exhausted", &mut summary);
            warn!("startup spool cleanup worker exceeded its time budget; startup will continue");
            break;
        };
        match worker {
            Ok(batch_summary) => {
                summary.deleted += batch_summary.deleted;
                summary.retained += batch_summary.retained;
                summary.errors += batch_summary.errors;
            }
            Err(error) => {
                summary.retained += in_flight;
                summary.errors += 1;
                warn!(%error, "startup spool cleanup worker failed");
            }
        }
    }
    summary.log();
    summary
}

fn retain_remaining(
    candidates: &HashMap<u32, Vec<Candidate>>,
    reason: &str,
    summary: &mut CleanupSummary,
) {
    for candidate in candidates.values().flatten() {
        warn!(job_id = candidate.spool.job_id, path = %candidate.path.display(), reason,
            "retained job spool");
        summary.retained += 1;
    }
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
            path: root.job_path(spool.job_id),
            root: root.clone(),
            spool,
        }));
    }
    candidates
}

fn clean_inactive(
    candidates: Vec<Candidate>,
    paths: &ActivityPaths,
    deadline: Instant,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    let inventory = activity_inventory(&candidates, paths, deadline);
    for candidate in candidates {
        let job_id = candidate.spool.job_id;
        let path = &candidate.path;
        let inventory = match &inventory {
            Err(error) => {
                warn!(job_id, path = %path.display(), %error, "retained job spool: activity inventory uncertain");
                summary.retained += 1;
                continue;
            }
            Ok(inventory) => inventory,
        };
        let held = if inventory.protected.contains(&job_id) {
            Ok(true)
        } else {
            candidate
                .root
                .holds_any(&candidate.spool, &inventory.held, Some(deadline))
        };
        let result = match held {
            Ok(true) => {
                info!(job_id, path = %path.display(), "retained job spool: runtime session, cgroup or process still holds it");
                summary.retained += 1;
                continue;
            }
            Ok(false) if Instant::now() >= deadline => Err(budget_exhausted()),
            Ok(false) => candidate.root.delete(&candidate.spool, Some(deadline)),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                summary.deleted += 1;
                info!(job_id, path = %path.display(), "deleted orphan job spool");
            }
            Err(error) => match kept_reason(&error) {
                Some(reason) => {
                    summary.retained += 1;
                    warn!(job_id, path = %path.display(), "kept job spool: {reason}");
                }
                None => {
                    summary.errors += 1;
                    warn!(job_id, path = %path.display(), %error, "failed to delete orphan job spool; will reconsider on next startup");
                }
            },
        }
    }
    summary
}

struct Inventory {
    protected: HashSet<u32>,
    held: HashSet<(u64, u64)>,
}

fn activity_inventory(
    candidates: &[Candidate],
    paths: &ActivityPaths,
    deadline: Instant,
) -> io::Result<Inventory> {
    let mut inventory = Inventory {
        protected: retained_sessions(&paths.runtime_root)?,
        held: HashSet::new(),
    };
    inventory
        .protected
        .extend(populated_jobs(&paths.cgroup_root)?);
    inspect_processes(paths, candidates, deadline, &mut inventory)?;
    Ok(inventory)
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
        let entry = match open_child(&directory, &name, OFlag::O_PATH) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
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
    // Population is read from cgroup v2 `cgroup.events`; without v2 nothing proves a job idle.
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
        let populated = open_child(&directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)
            .and_then(|job| cgroup_populated(&job));
        match populated {
            Ok(true) => {
                protected.insert(id);
            }
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
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
    paths: &ActivityPaths,
    candidates: &[Candidate],
    deadline: Instant,
    inventory: &mut Inventory,
) -> io::Result<()> {
    let holders = HolderFilter {
        devices: candidates.iter().map(|c| c.spool.device()).collect(),
        remote: RemoteMounts::read(&paths.proc_root.join("self/mountinfo"))?,
    };
    for entry in fs::read_dir(&paths.proc_root)? {
        if Instant::now() >= deadline {
            return Err(budget_exhausted());
        }
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
        let result = inspect_process(&path, paths.supervisor_uid, candidates, &holders, inventory);
        match fs::read(path.join("stat")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(after) if process_identity(&before)? != process_identity(&after)? => {
                return Err(io::Error::other(
                    "process identity changed during spool inventory",
                ));
            }
            // An exiting process loses its files piecemeal; it no longer holds anything.
            Ok(after) if result.is_err() && matches!(process_state(&after)?, b"Z" | b"X") => {}
            Ok(_) => result?,
        }
    }
    Ok(())
}

fn stat_field(stat: &[u8], index: usize) -> io::Result<&[u8]> {
    let end = stat
        .iter()
        .rposition(|b| *b == b')')
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    stat[end + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .nth(index)
        .ok_or_else(|| io::Error::other("truncated process stat"))
}

fn process_identity(stat: &[u8]) -> io::Result<&[u8]> {
    stat_field(stat, 19)
}

fn process_state(stat: &[u8]) -> io::Result<&[u8]> {
    stat_field(stat, 0)
}

struct HolderFilter {
    devices: HashSet<u64>,
    remote: RemoteMounts,
}

/// Network and FUSE mounts, whose stat can block indefinitely; spools never live on them.
#[derive(Default)]
struct RemoteMounts {
    mounts: Vec<(PathBuf, bool)>,
}

impl RemoteMounts {
    fn read(path: &Path) -> io::Result<Self> {
        let info = match fs::read_to_string(path) {
            Ok(info) => info,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let mounts = info
            .lines()
            .filter_map(|line| {
                let (left, right) = line.split_once(" - ")?;
                let point = left.split(' ').nth(4)?;
                let kind = right.split(' ').next()?;
                Some((PathBuf::from(unescape_mount_path(point)), is_remote(kind)))
            })
            .collect();
        Ok(Self { mounts })
    }

    fn contains(&self, path: &Path) -> bool {
        self.mounts
            .iter()
            .filter(|(point, _)| path.starts_with(point))
            .max_by_key(|(point, _)| point.as_os_str().len())
            .is_some_and(|(_, remote)| *remote)
    }
}

fn is_remote(kind: &str) -> bool {
    kind.starts_with("fuse")
        || kind.starts_with("nfs")
        || matches!(
            kind,
            "cifs"
                | "smb3"
                | "smbfs"
                | "9p"
                | "ceph"
                | "glusterfs"
                | "lustre"
                | "gpfs"
                | "beegfs"
                | "afs"
        )
}

fn unescape_mount_path(point: &str) -> String {
    point
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Real and effective uid from `/proc/<pid>/status`; unlike the directory owner, a process cannot fake these.
fn process_uids(path: &Path) -> io::Result<(u32, u32)> {
    let status = fs::read_to_string(path.join("status"))?;
    let uids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .map(|line| {
            line.split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
        })
        .ok_or_else(|| io::Error::other("missing process uid"))?
        .map_err(|_| io::Error::other("invalid process uid"))?;
    match uids.as_slice() {
        [real, effective, ..] => Ok((*real, *effective)),
        _ => Err(io::Error::other("invalid process uid")),
    }
}

fn is_supervisor(
    path: &Path,
    supervisor_uid: u32,
    comm: &[u8],
    args: &[&[u8]],
) -> io::Result<bool> {
    if process_uids(path)? != (supervisor_uid, supervisor_uid) {
        return Ok(false);
    }
    if comm.strip_suffix(b"\n").unwrap_or(comm) == b"spurstepd"
        || args.first().is_some_and(|arg| {
            Path::new(OsStr::from_bytes(arg)).file_name() == Some(OsStr::new("spurstepd"))
        })
    {
        return Ok(true);
    }
    match fs::read_link(path.join("exe")) {
        Ok(exe) => {
            let bytes = exe.as_os_str().as_bytes();
            let exe = Path::new(OsStr::from_bytes(
                bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
            ));
            Ok(exe.file_name() == Some(OsStr::new("spurstepd")))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn inspect_process(
    path: &Path,
    supervisor_uid: u32,
    candidates: &[Candidate],
    holders: &HolderFilter,
    inventory: &mut Inventory,
) -> io::Result<()> {
    let cmdline = fs::read(path.join("cmdline"))?;
    let comm = fs::read(path.join("comm"))?;
    let args: Vec<_> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    if is_supervisor(path, supervisor_uid, &comm, &args)? {
        let id = args
            .get(2)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0);
        let Some(id) = id else {
            return Err(io::Error::other("unidentified live supervisor"));
        };
        inventory.protected.insert(id);
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
        match std::str::from_utf8(value)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|id| *id > 0)
        {
            Some(id) => {
                inventory.protected.insert(id);
            }
            None => debug!(process = %path.display(), "ignored unparsable job marker"),
        }
    }
    let cwd = path.join("cwd");
    match fs::read_link(&cwd) {
        Ok(target) => protect_holder(&cwd, &target, candidates, holders, inventory)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && args.is_empty() => {}
        Err(error) => return Err(error),
    }
    for fd in fs::read_dir(path.join("fd"))? {
        let fd = fd?.path();
        match fs::read_link(&fd) {
            Ok(target) => protect_holder(&fd, &target, candidates, holders, inventory)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

// Inode identity catches holders that reach a spool through another path, e.g. a bind mount.
fn protect_holder(
    holder: &Path,
    target: &Path,
    candidates: &[Candidate],
    holders: &HolderFilter,
    inventory: &mut Inventory,
) -> io::Result<()> {
    protect_target(target, candidates, &mut inventory.protected);
    if !target.is_absolute() || holders.remote.contains(target) {
        return Ok(());
    }
    match fs::metadata(holder) {
        Ok(metadata) if holders.devices.contains(&metadata.dev()) => {
            inventory.held.insert(inode_identity(&metadata));
        }
        Ok(_) => {}
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
        if target.starts_with(&candidate.path) {
            protected.insert(candidate.spool.job_id);
        }
    }
}
