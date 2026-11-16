// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use nix::fcntl::{openat, openat2, AtFlags, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::{fstatat, Mode};
use nix::unistd::{unlinkat, UnlinkatFlags};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let anchor = File::open(if path.is_absolute() { "/" } else { "." })?;
    let result = openat2(
        &anchor,
        path,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS),
    );
    match result {
        Ok(directory) => Ok(directory.into()),
        Err(error) if unsupported_openat2(error) => {
            let mut directory = anchor;
            for component in path.components() {
                let name = match component {
                    Component::RootDir | Component::CurDir => continue,
                    Component::Normal(name) => name,
                    Component::ParentDir => OsStr::new(".."),
                    Component::Prefix(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid spool root",
                        ));
                    }
                };
                directory = openat(
                    &directory,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                    Mode::empty(),
                )?
                .into();
            }
            Ok(directory)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn open_child(parent: &File, name: &OsStr, flags: OFlag) -> io::Result<File> {
    if name.is_empty() || name == OsStr::new("..") || name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a direct spool entry",
        ));
    }
    let result = openat2(
        parent,
        name,
        OpenHow::new()
            .flags(flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_XDEV,
            ),
    );
    match result {
        Ok(entry) => Ok(entry.into()),
        Err(error) if unsupported_openat2(error) => {
            let entry: File = openat(
                parent,
                name,
                flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )?
            .into();
            // Device IDs do not distinguish bind mounts of the same filesystem.
            if mount_id(parent)? != mount_id(&entry)? {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            Ok(entry)
        }
        Err(error) => Err(error.into()),
    }
}

fn unsupported_openat2(error: nix::errno::Errno) -> bool {
    matches!(
        error,
        nix::errno::Errno::ENOSYS | nix::errno::Errno::EPERM | nix::errno::Errno::EINVAL
    )
}

fn mount_id(file: &File) -> io::Result<u64> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| io::Error::other("cannot establish spool entry mount identity"))
}

pub(crate) fn directory_names(directory: &File) -> io::Result<Vec<OsString>> {
    // A dup shares the directory offset; each inventory needs its own cursor.
    let cursor = open_child(
        directory,
        OsStr::new("."),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY,
    )?;
    let mut directory = nix::dir::Dir::from_fd(cursor.into())?;
    directory
        .iter()
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(
                OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
            )),
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

fn verify_root(root: &File) -> io::Result<()> {
    let metadata = root.metadata()?;
    if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "spool root must be daemon-owned and not writable by group or others",
        ));
    }
    Ok(())
}

pub(crate) fn canonical_job_id(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let id = name.strip_prefix("job")?.parse::<u32>().ok()?;
    (id > 0 && name == format!("job{id}")).then_some(id)
}

pub(crate) struct SpoolRoot {
    pub(crate) path: PathBuf,
    directory: File,
}

#[derive(Clone)]
pub(crate) struct JobSpool {
    pub(crate) job_id: u32,
    metadata: Metadata,
}

impl SpoolRoot {
    pub(crate) fn open(path: PathBuf) -> io::Result<Self> {
        let directory = open_directory(&path)?;
        verify_root(&directory)?;
        Ok(Self { path, directory })
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let metadata = self.directory.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(crate) fn candidates(&self) -> io::Result<Vec<JobSpool>> {
        let mut candidates = Vec::new();
        for name in directory_names(&self.directory)? {
            let Some(job_id) = canonical_job_id(&name) else {
                continue;
            };
            let metadata = fstatat(
                &self.directory,
                name.as_os_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            )?;
            if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
                tracing::info!(job_id, root = %self.path.display(), "skipped job spool name that is not a direct directory");
                continue;
            }
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => candidates.push(JobSpool {
                    job_id,
                    metadata: directory.metadata()?,
                }),
                Err(error) => tracing::warn!(job_id, root = %self.path.display(), %error,
                    "skipped unsafe or unreadable job spool"),
            }
        }
        Ok(candidates)
    }

    pub(crate) fn delete(&self, spool: &JobSpool) -> io::Result<()> {
        verify_root(&self.directory)?;
        let current = Self::open(self.path.clone())?;
        if current.identity()? != self.identity()? {
            return Err(io::Error::other("spool root changed since discovery"));
        }
        let name = OsString::from(format!("job{}", spool.job_id));
        let directory =
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
        require_identity(&self.directory, &name, &spool.metadata)?;
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other("job spool changed since discovery"));
        }
        remove_contents(&directory)?;
        require_identity(&self.directory, &name, &spool.metadata)?;
        absent_is_success(
            unlinkat(&self.directory, name.as_os_str(), UnlinkatFlags::RemoveDir)
                .map_err(Into::into),
        )
    }

    pub(crate) fn held_identities(&self, spool: &JobSpool) -> io::Result<Vec<(u64, u64)>> {
        let name = OsString::from(format!("job{}", spool.job_id));
        let directory =
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other("job spool changed since discovery"));
        }
        let mut identities = Vec::new();
        collect_identities(&directory, &mut identities)?;
        Ok(identities)
    }
}

fn collect_identities(directory: &File, identities: &mut Vec<(u64, u64)>) -> io::Result<()> {
    let metadata = directory.metadata()?;
    identities.push((metadata.dev(), metadata.ino()));
    let mut stack = vec![TraversalFrame::new(directory.try_clone()?, None)?];
    while let Some(frame) = stack.last_mut() {
        let Some(name) = frame.names.next() else {
            stack.pop();
            continue;
        };
        let entry = open_child(&frame.directory, &name, OFlag::O_PATH)?;
        let metadata = entry.metadata()?;
        identities.push((metadata.dev(), metadata.ino()));
        if metadata.is_dir() {
            let child = open_child(
                &frame.directory,
                &name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            )?;
            if !same_identity(&child.metadata()?, &metadata) {
                return Err(io::Error::other("spool directory changed during inventory"));
            }
            stack.push(TraversalFrame::new(child, None)?);
        }
    }
    Ok(())
}

fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn require_identity(parent: &File, name: &OsStr, expected: &Metadata) -> io::Result<()> {
    let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if current.st_dev != expected.dev() || current.st_ino != expected.ino() {
        return Err(io::Error::other("spool entry changed during cleanup"));
    }
    Ok(())
}

fn remove_contents(directory: &File) -> io::Result<()> {
    let mut stack = vec![TraversalFrame::new(directory.try_clone()?, None)?];
    while let Some(frame) = stack.last_mut() {
        let Some(name) = frame.names.next() else {
            let completed = stack
                .pop()
                .ok_or_else(|| io::Error::other("missing spool traversal frame"))?;
            if let Some((name, metadata)) = completed.entry {
                let parent = stack
                    .last()
                    .ok_or_else(|| io::Error::other("missing spool parent"))?;
                require_identity(&parent.directory, &name, &metadata)?;
                absent_is_success(
                    unlinkat(
                        &parent.directory,
                        name.as_os_str(),
                        UnlinkatFlags::RemoveDir,
                    )
                    .map_err(Into::into),
                )?;
            }
            continue;
        };
        let entry = match open_child(&frame.directory, &name, OFlag::O_PATH) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let child = open_child(
                &frame.directory,
                &name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            )?;
            if !same_identity(&child.metadata()?, &metadata) {
                return Err(io::Error::other("spool directory changed during cleanup"));
            }
            stack.push(TraversalFrame::new(child, Some((name, metadata)))?);
            continue;
        }
        require_identity(&frame.directory, &name, &metadata)?;
        absent_is_success(
            unlinkat(
                &frame.directory,
                name.as_os_str(),
                UnlinkatFlags::NoRemoveDir,
            )
            .map_err(Into::into),
        )?;
    }
    Ok(())
}

struct TraversalFrame {
    directory: File,
    names: std::vec::IntoIter<OsString>,
    entry: Option<(OsString, Metadata)>,
}

impl TraversalFrame {
    fn new(directory: File, entry: Option<(OsString, Metadata)>) -> io::Result<Self> {
        let names = directory_names(&directory)?.into_iter();
        Ok(Self {
            directory,
            names,
            entry,
        })
    }
}

fn absent_is_success(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub(crate) fn cleanup_job_spool_under(root: &Path, job_id: u32) -> io::Result<()> {
    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "job id must be positive",
        ));
    }
    let root = match SpoolRoot::open(root.to_owned()) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = OsString::from(format!("job{job_id}"));
    let directory = match open_child(&root.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    root.delete(&JobSpool {
        job_id,
        metadata: directory.metadata()?,
    })
}

#[cfg(test)]
#[path = "spool_fs_tests.rs"]
mod tests;
