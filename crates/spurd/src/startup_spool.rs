// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::agent_server::AgentService;
use crate::spool_fs::{
    budget_exhausted, directory_names, inode_identity, kept_reason, open_child, open_directory,
    read_mounts, Bound, JobSpool, Mount, SpoolKind, SpoolRoot,
};
use nix::fcntl::OFlag;
use spur_core::job::JobState;
use spur_proto::proto::GetJobsResponse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub(crate) container_root: Option<PathBuf>,
    #[cfg(test)]
    pub(crate) worker_gate: Option<Arc<(tokio::sync::Notify, std::sync::Barrier)>>,
}

#[derive(Debug, Default)]
pub(crate) struct CleanupSummary {
    pub(crate) deleted: usize,
    pub(crate) retained: usize,
    pub(crate) errors: usize,
    pub(crate) pending_rootfs: Vec<PendingRootfs>,
}

impl CleanupSummary {
    fn log(&self) {
        info!(
            deleted = self.deleted,
            retained = self.retained,
            errors = self.errors,
            queued_rootfs = self.pending_rootfs.len(),
            "startup job spool cleanup finished"
        );
    }

    fn absorb(&mut self, batch: CleanupSummary) {
        self.deleted += batch.deleted;
        self.retained += batch.retained;
        self.errors += batch.errors;
        self.pending_rootfs.extend(batch.pending_rootfs);
    }
}

struct Candidate {
    root: Arc<SpoolRoot>,
    spool: JobSpool,
    path: PathBuf,
}

/// A container rootfs in the trash, left for background deletion; its job's path is already free.
pub(crate) struct PendingRootfs(Candidate);

impl PendingRootfs {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
}

impl std::fmt::Debug for PendingRootfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let candidate = &self.0;
        f.debug_struct("PendingRootfs")
            .field("job_id", &candidate.spool.job_id)
            .field("path", &candidate.path)
            .finish()
    }
}

pub(crate) async fn run(
    service: &AgentService,
    spool_roots: Vec<PathBuf>,
    paths: ActivityPaths,
) -> CleanupSummary {
    let roots = spool_roots
        .into_iter()
        .map(|root| (root, SpoolKind::Spool))
        .chain(
            paths
                .container_root
                .clone()
                .map(|root| (root, SpoolKind::Rootfs)),
        )
        .collect();
    let mut summary = CleanupSummary::default();
    let deadline = Instant::now() + paths.budget;
    if !paths.cgroups_enabled {
        warn!("skipped startup job spool cleanup: cgroups are disabled");
        summary.log();
        return summary;
    }
    let discovery = tokio::task::spawn_blocking(move || discover(roots));
    let candidates = match tokio::time::timeout_at(deadline.into(), discovery).await {
        Ok(Ok((candidates, leftovers))) => {
            summary
                .pending_rootfs
                .extend(leftovers.into_iter().map(PendingRootfs));
            candidates
        }
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
    // Read before any job lock is taken: a /proc read can block forever, and must not strand locks.
    let found = candidates.len();
    let paths = Arc::new(paths);
    let reading = tokio::task::spawn_blocking({
        let paths = paths.clone();
        move || {
            let inventory = activity_inventory(&candidates, &paths, deadline);
            (candidates, inventory)
        }
    });
    let (candidates, inventory) = match tokio::time::timeout_at(deadline.into(), reading).await {
        Ok(Ok((candidates, Ok(inventory)))) => (candidates, Arc::new(inventory)),
        Ok(Ok((candidates, Err(error)))) => {
            for candidate in &candidates {
                debug!(job_id = candidate.spool.job_id, path = %candidate.path.display(), "retained job spool: activity inventory uncertain");
            }
            warn!(count = found, %error, "retained job spools: activity inventory uncertain");
            summary.retained += found;
            summary.log();
            return summary;
        }
        Ok(Err(error)) => {
            warn!(count = found, %error, "retained job spools: activity inventory failed");
            summary.retained += found;
            summary.errors += 1;
            summary.log();
            return summary;
        }
        Err(_) => {
            warn!(count = found, "retained job spools: activity inventory exceeded the time budget; startup will continue");
            summary.retained += found;
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
    let mut candidates: HashMap<u32, Vec<Candidate>> =
        candidates
            .into_iter()
            .fold(HashMap::new(), |mut map, candidate| {
                map.entry(candidate.spool.job_id)
                    .or_default()
                    .push(candidate);
                map
            });
    let owners = service.spool_ownership();
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
        let confirmed = controller_confirmed(&response, batch);
        if response.authoritative && !response.unrestricted_visibility && !reported_hidden_jobs {
            let reported: HashSet<_> = response.jobs.iter().map(|job| job.job_id).collect();
            if batch.iter().any(|id| !reported.contains(id)) {
                reported_hidden_jobs = true;
                info!("retaining directories the controller does not show: private job data hides other users' jobs from this node");
            }
        }
        let mut guards = Vec::new();
        let mut eligible = Vec::new();
        let mut unconfirmed = 0;
        for &job_id in batch {
            let job_candidates = candidates.remove(&job_id).unwrap_or_default();
            if !confirmed.contains(&job_id) {
                for candidate in &job_candidates {
                    debug!(job_id, path = %candidate.path.display(), "retained {}: controller has not confirmed terminality or absence", candidate.spool.kind.label());
                }
                unconfirmed += job_candidates.len();
                continue;
            }
            let guard = owners.guard(job_id).await;
            if let Some(reason) = owners.local_owner(job_id).await {
                for candidate in &job_candidates {
                    info!(job_id, path = %candidate.path.display(), reason, "retained locally owned {}", candidate.spool.kind.label());
                }
                summary.retained += job_candidates.len();
                continue;
            }
            guards.push(guard);
            eligible.extend(job_candidates);
        }
        if unconfirmed > 0 {
            info!(
                count = unconfirmed,
                "retained directories: controller has not confirmed terminality or absence"
            );
            summary.retained += unconfirmed;
        }
        if eligible.is_empty() {
            continue;
        }
        let in_flight = eligible.len();
        let inventory = inventory.clone();
        #[cfg(test)]
        let gate = paths.worker_gate.clone();
        // Keep guards in the worker so cancellation cannot release launch exclusion
        // while blocking filesystem operations still run.
        let worker = tokio::task::spawn_blocking(move || {
            let _guards = guards;
            #[cfg(test)]
            if let Some(gate) = &gate {
                gate.0.notify_one();
                gate.1.wait();
            }
            clean_inactive(eligible, &inventory, deadline)
        });
        // A worker stuck in the kernel keeps its guards; its own deadline checks stop late deletions.
        let Ok(worker) = tokio::time::timeout_at(deadline.into(), worker).await else {
            summary.retained += in_flight;
            retain_remaining(&candidates, "cleanup time budget exhausted", &mut summary);
            warn!("startup spool cleanup worker exceeded its time budget; startup will continue");
            break;
        };
        match worker {
            Ok(batch_summary) => summary.absorb(batch_summary),
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
    let count = candidates.values().map(Vec::len).sum::<usize>();
    for candidate in candidates.values().flatten() {
        debug!(job_id = candidate.spool.job_id, path = %candidate.path.display(), reason, "retained {}", candidate.spool.kind.label());
    }
    if count > 0 {
        warn!(count, reason, "retained remaining directories");
    }
    summary.retained += count;
}

/// Ids in `batch` the controller reports terminal, or with full visibility no longer keeps below its next id.
pub(crate) fn controller_confirmed(response: &GetJobsResponse, batch: &[u32]) -> HashSet<u32> {
    if !response.authoritative {
        return HashSet::new();
    }
    let mut terminal = HashMap::<u32, bool>::new();
    for job in &response.jobs {
        let done = JobState::from_proto_i32(job.state).is_some_and(|state| state.is_terminal());
        // Conflicting duplicate records cannot establish terminality.
        terminal
            .entry(job.job_id)
            .and_modify(|known| *known &= done)
            .or_insert(done);
    }
    batch
        .iter()
        .copied()
        .filter(|id| {
            terminal
                .get(id)
                .copied()
                .unwrap_or(response.unrestricted_visibility && *id < response.next_job_id)
        })
        .collect()
}

fn discover(paths: Vec<(PathBuf, SpoolKind)>) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut candidates = Vec::new();
    let mut leftovers = Vec::new();
    let mut roots = HashSet::new();
    for (path, kind) in paths {
        let root = match SpoolRoot::open(path.clone()) {
            Ok(root) => Arc::new(root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(root = %path.display(), %error, "skipped unsafe or unreadable spool root");
                continue;
            }
        };
        let spools = match root.identity() {
            Ok(identity) if !roots.insert((identity, kind)) => continue,
            Ok(_) => root.candidates(kind),
            Err(error) => Err(error),
        };
        let spools = match spools {
            Ok(spools) => spools,
            Err(error) => {
                warn!(root = %path.display(), %error, "skipped unreadable spool inventory");
                continue;
            }
        };
        candidates.extend(spools.into_iter().map(|spool| Candidate {
            path: root.entry_path(&spool),
            root: root.clone(),
            spool,
        }));
        if kind == SpoolKind::Rootfs {
            leftovers.extend(trash_leftovers(&root));
        }
    }
    (candidates, leftovers)
}

fn trash_leftovers(root: &SpoolRoot) -> Vec<Candidate> {
    let (trash, spools) = match root.trash_leftovers() {
        Ok(Some(found)) => found,
        Ok(None) => return Vec::new(),
        Err(error) => {
            warn!(root = %root.path.display(), %error, "skipped unsafe or unreadable container trash");
            return Vec::new();
        }
    };
    let trash = Arc::new(trash);
    spools
        .into_iter()
        .map(|spool| Candidate {
            path: trash.entry_path(&spool),
            root: trash.clone(),
            spool,
        })
        .collect()
}

fn clean_inactive(
    candidates: Vec<Candidate>,
    inventory: &Inventory,
    deadline: Instant,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    let inventory = match activity_inventory(&candidates, paths, deadline) {
        Ok(inventory) => inventory,
        Err(error) => {
            for candidate in &candidates {
                debug!(job_id = candidate.spool.job_id, path = %candidate.path.display(), "retained {}: activity inventory uncertain", candidate.spool.kind.label());
            }
            warn!(count = candidates.len(), %error, "retained directories: activity inventory uncertain");
            summary.retained += candidates.len();
            return summary;
        }
    };
    let bound = Bound::until(deadline);
    for candidate in candidates {
        let job_id = candidate.spool.job_id;
        let path = &candidate.path;
        let kind = candidate.spool.kind.label();
        let held = if inventory.protected.contains(&job_id) {
            Ok(true)
        } else {
            match candidate.spool.kind {
                SpoolKind::Spool => {
                    candidate
                        .root
                        .holds_any(&candidate.spool, &inventory.held, &bound)
                }
                // A rootfs is held through its root inode; walking a full image tree is too slow.
                SpoolKind::Rootfs => Ok(candidate
                    .spool
                    .identities()
                    .any(|identity| inventory.held.contains(&identity))),
            }
        };
        let result = match held {
            Ok(true) => {
                info!(job_id, path = %path.display(), "retained {kind}: runtime session, cgroup or process still holds it");
                summary.retained += 1;
                continue;
            }
            Ok(false) if Instant::now() >= deadline => Err(budget_exhausted()),
            Ok(false) if candidate.spool.kind == SpoolKind::Rootfs => {
                let mountinfo = paths.proc_root.join("self/mountinfo");
                match unmount_rootfs(&candidate, &inventory.held, &mountinfo) {
                    Ok(Some((detached, spool))) => {
                        match move_to_trash(&candidate.root, &spool) {
                            Ok(trashed) => {
                                info!(job_id, path = %path.display(), detached, trash = %trashed.path.display(), "queued orphan container rootfs for background deletion");
                                summary.pending_rootfs.push(PendingRootfs(trashed));
                            }
                            Err(error) => {
                                warn!(job_id, path = %path.display(), %error, "kept container rootfs: could not move it to the trash");
                                summary.retained += 1;
                            }
                        }
                        continue;
                    }
                    Ok(None) => {
                        info!(job_id, path = %path.display(), "retained {kind}: a process holds the directory under its mount");
                        summary.retained += 1;
                        continue;
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(false) => candidate.root.delete(&candidate.spool, &bound),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                summary.deleted += 1;
                info!(job_id, path = %path.display(), "deleted orphan {kind}");
            }
            Err(error) => match kept_reason(&error) {
                Some(reason) => {
                    summary.retained += 1;
                    warn!(job_id, path = %path.display(), "kept {kind}: {reason}");
                }
                None => {
                    summary.errors += 1;
                    warn!(job_id, path = %path.display(), %error, "failed to delete orphan {kind}; will reconsider on next startup");
                }
            },
        }
    }
    summary
}

// Freeing the job's path here, under its lock, keeps a relaunch from reusing the old tree.
fn move_to_trash(root: &SpoolRoot, spool: &JobSpool) -> io::Result<Candidate> {
    let trash = Arc::new(root.trash()?);
    let spool = root.move_to(spool, &trash)?;
    Ok(Candidate {
        path: trash.entry_path(&spool),
        root: trash,
        spool,
    })
}

/// Unmounts a confirmed orphan rootfs and reopens it; `None` when a process holds the uncovered directory.
fn unmount_rootfs(
    candidate: &Candidate,
    held: &HashSet<(u64, u64)>,
    mountinfo: &Path,
) -> io::Result<Option<(usize, JobSpool)>> {
    let unmounted = candidate
        .root
        .unmount_beneath(&candidate.spool, mountinfo)?;
    let spool = candidate.root.reopen(&candidate.spool, unmounted)?;
    // The directory a mount hid is not among the identities discovered for the entry.
    if spool.identities().any(|id| held.contains(&id)) {
        return Ok(None);
    }
    Ok(Some((unmounted.count, spool)))
}

/// Deletes the container rootfs trees in the trash after the agent serves; no job lock is needed.
pub(crate) async fn delete_pending_rootfs(pending: Vec<PendingRootfs>, stop: Arc<AtomicBool>) {
    let (mut deleted, mut kept, mut failed) = (0, 0, 0);
    for PendingRootfs(candidate) in pending {
        if stop.load(Ordering::Relaxed) {
            kept += 1;
            continue;
        }
        let job_id = candidate.spool.job_id;
        let path = candidate.path.clone();
        info!(job_id, path = %path.display(), "deleting orphan container rootfs");
        let bound = Bound::until_stopped(stop.clone());
        let result =
            tokio::task::spawn_blocking(move || candidate.root.delete(&candidate.spool, &bound))
                .await;
        match result {
            Ok(Ok(())) => {
                deleted += 1;
                info!(job_id, path = %path.display(), "deleted orphan container rootfs");
            }
            Ok(Err(error)) => match kept_reason(&error) {
                Some(reason) => {
                    kept += 1;
                    warn!(job_id, path = %path.display(), "kept container rootfs: {reason}");
                }
                None => {
                    failed += 1;
                    warn!(job_id, path = %path.display(), %error, "failed to delete orphan container rootfs; the next start retries");
                }
            },
            Err(error) => {
                failed += 1;
                warn!(job_id, %error, "container rootfs deletion task failed");
            }
        }
    }
    if stop.load(Ordering::Relaxed) {
        info!("stopped container rootfs deletion; the next start finishes the trash");
    }
    info!(
        deleted,
        kept, failed, "background container rootfs cleanup finished"
    );
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
    let own_namespace = mount_namespace(&paths.proc_root.join("self"))?;
    let mut devices: HashSet<u64> = candidates
        .iter()
        .flat_map(|c| c.spool.identities().map(|(device, _)| device))
        .collect();
    for candidate in candidates {
        devices.insert(candidate.root.device()?);
    }
    let host = MountTable::read(&paths.proc_root.join("self/mountinfo"))?;
    let mut namespaces = HashMap::<PathBuf, MountTable>::new();
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
        // A process in another mount namespace is judged by its own mount table; an unreadable one is doubt.
        let mounts = match mount_namespace(&path) {
            Ok(Some(namespace)) if Some(&namespace) != own_namespace.as_ref() => {
                match namespaces.entry(namespace) {
                    Entry::Occupied(entry) => Ok(&*entry.into_mut()),
                    Entry::Vacant(entry) => match MountTable::read(&path.join("mountinfo")) {
                        Ok(table) if !table.mounts.is_empty() => Ok(&*entry.insert(table)),
                        Ok(_) => Err(io::Error::other("empty mount table")),
                        Err(error) => Err(error),
                    },
                }
            }
            Ok(_) => Ok(&host),
            Err(error) => Err(error),
        };
        let result = mounts.and_then(|mounts| {
            let holders = HolderFilter {
                devices: &devices,
                mounts,
            };
            inspect_process(&path, paths.supervisor_uid, candidates, &holders, inventory)
        });
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
    protect_bound_rootfs(candidates, &host, &namespaces, &mut inventory.protected);
    Ok(())
}

// A bind of a rootfs subtree in the agent's or a scanned process's namespace can be in use
// without any path or inode the scan sees.
fn protect_bound_rootfs(
    candidates: &[Candidate],
    host: &MountTable,
    namespaces: &HashMap<PathBuf, MountTable>,
    protected: &mut HashSet<u32>,
) {
    for candidate in candidates {
        if candidate.spool.kind != SpoolKind::Rootfs {
            continue;
        }
        // Located through the parent: mounts on or under the rootfs are its own and get unmounted.
        let (Some(parent), Some(name)) = (candidate.path.parent(), candidate.path.file_name())
        else {
            continue;
        };
        let Some((device, inside)) = host.locate(parent) else {
            continue;
        };
        let inside = inside.join(name);
        let bound = std::iter::once(host)
            .chain(namespaces.values())
            .flat_map(|table| &table.mounts)
            .filter(|mount| !mount.point.starts_with(&candidate.path))
            .any(|mount| mount.device == device && mount.root.starts_with(&inside));
        if bound {
            protected.insert(candidate.spool.job_id);
        }
    }
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

struct HolderFilter<'a> {
    devices: &'a HashSet<u64>,
    mounts: &'a MountTable,
}

/// A mount namespace's mounts: which files are on network or FUSE mounts (never stat-ed), and binds.
struct MountTable {
    mounts: Vec<Mount>,
}

impl MountTable {
    fn read(path: &Path) -> io::Result<Self> {
        Ok(Self {
            mounts: read_mounts(path)?,
        })
    }

    fn containing(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.point))
            .max_by_key(|mount| mount.point.as_os_str().len())
    }

    fn is_remote(&self, path: &Path) -> bool {
        self.containing(path)
            .is_some_and(|mount| is_remote(&mount.kind))
    }

    fn is_remote_id(&self, id: u64) -> Option<bool> {
        self.mounts
            .iter()
            .find(|mount| mount.id == id)
            .map(|mount| is_remote(&mount.kind))
    }

    /// The filesystem holding `path` and the path within that filesystem.
    fn locate(&self, path: &Path) -> Option<(String, PathBuf)> {
        let mount = self.containing(path)?;
        let rest = path.strip_prefix(&mount.point).ok()?;
        Some((mount.device.clone(), mount.root.join(rest)))
    }
}

fn fd_mount_id(fdinfo: &Path) -> Option<u64> {
    fs::read_to_string(fdinfo)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))?
        .trim()
        .parse()
        .ok()
}

fn mount_namespace(process: &Path) -> io::Result<Option<PathBuf>> {
    match fs::read_link(process.join("ns/mnt")) {
        Ok(namespace) => Ok(Some(namespace)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
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
        let Some(id) = crate::stepd::job_id_from_argv(&args) else {
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
    for link in ["cwd", "root"] {
        let link = path.join(link);
        match fs::read_link(&link) {
            Ok(target) => protect_holder(&link, &target, None, candidates, holders, inventory)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound && args.is_empty() => {}
            Err(error) => return Err(error),
        }
    }
    for fd in fs::read_dir(path.join("fd"))? {
        let fd = fd?;
        let mount = fd_mount_id(&path.join("fdinfo").join(fd.file_name()));
        let fd = fd.path();
        match fs::read_link(&fd) {
            Ok(target) => protect_holder(&fd, &target, mount, candidates, holders, inventory)?,
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
    mount: Option<u64>,
    candidates: &[Candidate],
    holders: &HolderFilter,
    inventory: &mut Inventory,
) -> io::Result<()> {
    protect_target(target, candidates, &mut inventory.protected);
    let remote = mount
        .and_then(|id| holders.mounts.is_remote_id(id))
        .unwrap_or_else(|| holders.mounts.is_remote(target));
    if !target.is_absolute() || remote {
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
